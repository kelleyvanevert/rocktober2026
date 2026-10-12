import { initMidi, listOutputs, getOutput, panic, Voices } from "./midi";
import { Scheduler } from "./scheduler";
import { Grid, type Cell } from "./grid";
import { Sidebar } from "./sidebar";
import { World, cellKey, type Automaton, type View } from "./world";
import { parse, isCommentedOut, commentOut, uncomment, type Program } from "./lang";

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const outputSel = $<HTMLSelectElement>("output");
const bpmInput = $<HTMLInputElement>("bpm");
const playBtn = $<HTMLButtonElement>("play");
const clearBtn = $<HTMLButtonElement>("clear");
const panicBtn = $<HTMLButtonElement>("panic");
const zoomInBtn = $<HTMLButtonElement>("zoom-in");
const zoomOutBtn = $<HTMLButtonElement>("zoom-out");
const frameEl = $<HTMLElement>("frame");
const statusEl = $<HTMLElement>("status");

// Bottom-bar messages, one slot per topic so they don't overwrite each other.
const statusMessages = new Map<string, string>();
function setStatus(topic: string, message: string | null) {
  if (message) statusMessages.set(topic, message);
  else statusMessages.delete(topic);
  statusEl.textContent = [...statusMessages.values()].join(" · ");
}

const store = {
  get: (k: string) => { try { return localStorage.getItem(k); } catch { return null; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch {} },
};

const world = new World();
const voices = new Voices();

// --- Grid ---

const grid = new Grid($<HTMLCanvasElement>("grid"));
grid.setSize(Number(store.get("zoom")) || grid.size);

function display(view: View) {
  grid.show(view);
  frameEl.textContent = String(view.frame);
  sidebar.refresh(idsAt(grid.selected));
}

const showWorld = () => display(world.view());

/** Automatons shown on a cell (in the displayed frame, which may lag the world slightly). */
function idsAt(cell: Cell | null): number[] {
  return (cell && grid.view?.cells.get(cellKey(cell.col, cell.row))?.ids) || [];
}

/** After an edit: redraw now when paused; when playing, the next frame shows it. */
function changed() {
  save();
  if (scheduler.running) sidebar.refresh(idsAt(grid.selected));
  else showWorld();
}

function zoom(dir: 1 | -1) {
  if (dir > 0) grid.zoomIn();
  else grid.zoomOut();
  store.set("zoom", String(grid.size));
}

zoomInBtn.onclick = () => zoom(1);
zoomOutBtn.onclick = () => zoom(-1);

// Trackpad pinch arrives as a ctrl+wheel event.
window.addEventListener(
  "wheel",
  (e) => {
    if (!e.ctrlKey) return;
    e.preventDefault();
    grid.setSize(grid.size * Math.exp(-e.deltaY * 0.01));
    store.set("zoom", String(grid.size));
  },
  { passive: false },
);

// --- Sidebar ---

const sidebar = new Sidebar($("sidebar"), store.get("draft") ?? "step right\nnote c3", (code) =>
  store.set("draft", code),
);

grid.onSelect = (cell) => {
  sidebar.show(cell);
  sidebar.refresh(idsAt(cell));
};

sidebar.lookup = (id) => world.get(id);

sidebar.onCreate = (cell, code) => {
  try {
    world.add(cell.col, cell.row, parse(code), code);
  } catch (err) {
    return (err as Error).message;
  }
  changed();
  return null;
};

sidebar.onUpdate = (id, code) => {
  const a = world.get(id);
  if (!a) return "this automaton no longer exists";
  try {
    a.program = parse(code);
  } catch (err) {
    return (err as Error).message;
  }
  a.code = code;
  a.started = false; // new code: let its `once` blocks run
  save();
  return null;
};

sidebar.onDelete = (id) => {
  world.remove([id]);
  changed();
};

// --- MIDI outputs ---

// Ports that come with the OS rather than leading somewhere external.
const BUILT_IN = /microsoft gs wavetable|midi through/i;

/** (Re)fill the output list on load and whenever devices come or go. */
function refreshOutputs() {
  const outputs = listOutputs();
  const wanted = [currentOutput, store.get("output")];
  outputSel.replaceChildren(
    ...(outputs.length
      ? outputs.map((o) => new Option(o.name ?? o.id, o.id))
      : [new Option("(no MIDI outputs found)", "")]),
  );
  // Keep the current (or remembered) choice if it's there, else prefer an external port.
  const pick =
    outputs.find((o) => wanted.includes(o.id)) ??
    outputs.find((o) => !BUILT_IN.test(o.name ?? "")) ??
    outputs[0];
  if (pick) outputSel.value = pick.id;
  currentOutput = outputSel.value;
  if (pick) setStatus("output", null);
}

let currentOutput = "";
outputSel.onchange = () => {
  const old = getOutput(currentOutput);
  if (old) panic(old);
  voices.releaseAll(undefined);
  currentOutput = outputSel.value;
  store.set("output", outputSel.value);
  setStatus("output", null);
  outputSel.blur(); // so space goes back to play/pause
};

// --- Playback ---

// Bumped on pause/clear so already-queued playhead updates are dropped.
let generation = 0;

const scheduler = new Scheduler((time) => {
  const { view, notes } = world.step(grid.cols, grid.rows);
  voices.frame(getOutput(outputSel.value), view.frame, time, notes);
  save();

  // Show the frame when it actually sounds, not when it's scheduled.
  const gen = generation;
  setTimeout(() => gen === generation && display(view), time - performance.now());
});

function play() {
  setStatus("output", getOutput(outputSel.value) ? null : "no MIDI output selected");
  scheduler.start();
  playBtn.textContent = "❚❚";
  playBtn.classList.add("playing");
}

function pause() {
  scheduler.stop();
  generation++;
  // Note-ons up to `lookahead` ms ahead are already queued in the browser, so release
  // everything right after the last of them.
  voices.releaseAll(getOutput(outputSel.value), scheduler.nextTime);
  showWorld();
  playBtn.textContent = "▶";
  playBtn.classList.remove("playing");
}

const togglePlay = () => (scheduler.running ? pause() : play());

playBtn.onclick = togglePlay;
/** Silence every note on every output (playback keeps going if it was running). */
function panicAll() {
  listOutputs().forEach(panic);
  voices.releaseAll(undefined);
}

panicBtn.onclick = panicAll;

clearBtn.onclick = () => {
  if (scheduler.running) pause();
  world.clear();
  showWorld();
  save();
};

// Keep focus off buttons, so space and arrows never act on them.
document.addEventListener("mousedown", (e) => {
  if ((e.target as Element).closest("button")) e.preventDefault();
});

bpmInput.oninput = () => {
  const bpm = Number(bpmInput.value);
  if (bpm >= 20 && bpm <= 400) scheduler.bpm = bpm;
  save();
};
bpmInput.onchange = () => bpmInput.blur(); // on enter, hand keys back to the grid

// --- Autosave ---

const SAVE_KEY = "composition";
let saveTimer = 0;

function saveNow() {
  clearTimeout(saveTimer);
  saveTimer = 0;
  store.set(SAVE_KEY, JSON.stringify({ bpm: scheduler.bpm, ...world.save() }));
}

/** Coalesced: saves at most once a second, since playing changes the world every frame. */
function save() {
  if (!saveTimer) saveTimer = window.setTimeout(saveNow, 1000);
}

window.addEventListener("pagehide", saveNow);

function restore() {
  const raw = store.get(SAVE_KEY);
  if (!raw) return;
  let broken = 0;
  try {
    const data = JSON.parse(raw);
    world.frame = data.frame ?? 0;
    for (const { col, row, code, value, started } of data.automatons ?? []) {
      // Keep code that no longer parses (the language may have changed) as an inactive
      // automaton, so it can be fixed instead of silently disappearing.
      let program: Program = [];
      try {
        program = parse(code);
      } catch {
        broken++;
      }
      world.add(col, row, program, code, value ?? 0, started ?? true);
    }
    if (data.bpm) {
      scheduler.bpm = data.bpm;
      bpmInput.value = String(data.bpm);
    }
  } catch (err) {
    console.error("could not restore saved composition", err);
  }
  if (broken) setStatus("restore", `${broken} saved automaton(s) no longer parse and are inactive`);
}

// --- Clipboard ---
// ⌘C / ⌘X / ⌘V act on all automatons of the selected cell (when not typing). The system
// clipboard gets their code as text; their full state (value, whether `once` ran) is
// kept here and used when that same text is pasted back. Any other pasted text becomes
// a new automaton.

type Copied = Pick<Automaton, "program" | "code" | "value" | "started">;
let clipboard: { text: string; automatons: Copied[] } | null = null;
let clipboardTimer = 0;

function flash(message: string) {
  setStatus("clipboard", message);
  clearTimeout(clipboardTimer);
  clipboardTimer = window.setTimeout(() => setStatus("clipboard", null), 2500);
}

const plural = (n: number) => `${n} automaton${n === 1 ? "" : "s"}`;

function copyCell(e: ClipboardEvent, cut: boolean) {
  if (isTyping(e.target) || !grid.selected) return;
  e.preventDefault();
  const automatons = idsAt(grid.selected)
    .map((id) => world.get(id))
    .filter((a) => a !== undefined);
  if (!automatons.length) return flash("nothing to copy on this cell");

  const text = automatons.map((a) => a.code).join("\n\n");
  e.clipboardData?.setData("text/plain", text);
  clipboard = { text, automatons: automatons.map(({ program, code, value, started }) => ({ program, code, value, started })) };
  if (cut) {
    world.remove(automatons.map((a) => a.id));
    changed();
  }
  flash(`${cut ? "cut" : "copied"} ${plural(automatons.length)}`);
}

document.addEventListener("copy", (e) => copyCell(e, false));
document.addEventListener("cut", (e) => copyCell(e, true));

document.addEventListener("paste", (e) => {
  if (isTyping(e.target) || !grid.selected) return;
  e.preventDefault();
  const { col, row } = grid.selected;
  const text = e.clipboardData?.getData("text/plain") ?? "";

  if (clipboard && text === clipboard.text) {
    for (const a of clipboard.automatons) world.add(col, row, a.program, a.code, a.value, a.started);
    flash(`pasted ${plural(clipboard.automatons.length)}`);
  } else {
    try {
      world.add(col, row, parse(text), text);
    } catch (err) {
      return flash(`paste: ${(err as Error).message}`);
    }
    flash("pasted text as a new automaton");
  }
  changed();
});

// --- Commenting out ---

/** ⌘/: comment out every automaton on the selected cell, or uncomment if all already are. */
function toggleComment() {
  const automatons = idsAt(grid.selected)
    .map((id) => world.get(id))
    .filter((a) => a !== undefined);
  if (!automatons.length) return;
  const uncommenting = automatons.every((a) => isCommentedOut(a.code));
  for (const a of automatons) {
    if (!uncommenting && isCommentedOut(a.code)) continue;
    a.code = uncommenting ? uncomment(a.code) : commentOut(a.code);
    try {
      a.program = parse(a.code);
    } catch {
      a.program = []; // was already broken before it got commented out
    }
  }
  changed();
}

// --- Keyboard ---

const isTyping = (el: EventTarget | null) =>
  el instanceof HTMLTextAreaElement || el instanceof HTMLInputElement || el instanceof HTMLSelectElement;

const ARROWS: Record<string, [number, number]> = {
  ArrowLeft: [-1, 0],
  ArrowRight: [1, 0],
  ArrowUp: [0, -1],
  ArrowDown: [0, 1],
};

window.addEventListener("keydown", (e) => {
  if (e.metaKey || e.ctrlKey) {
    // Take over the browser's own zoom shortcuts.
    if (e.key === "=" || e.key === "+") zoom(1);
    else if (e.key === "-" || e.key === "_") zoom(-1);
    // Works while typing in the editor, unlike plain space.
    else if (e.code === "Space") !e.repeat && togglePlay();
    else if (e.key === "/" && !isTyping(e.target)) toggleComment();
    else return;
    e.preventDefault();
  } else if (e.key === "Escape") {
    grid.select(null);
  } else if (isTyping(e.target)) {
    return;
  } else if (e.key === " ") {
    e.preventDefault();
    if (!e.repeat) togglePlay();
  } else if (e.key in ARROWS) {
    e.preventDefault();
    grid.moveSelection(...ARROWS[e.key]);
  } else if ((e.key === "Backspace" || e.key === "Delete") && grid.selected) {
    e.preventDefault();
    world.remove(idsAt(grid.selected));
    changed();
  }
});

// --- Boot ---

restore();
showWorld();
sidebar.show(null);

initMidi(refreshOutputs)
  .then(() => {
    refreshOutputs();
    panicAll(); // e.g. notes left hanging when the page was reloaded mid-note
    play();
  })
  .catch((err) => {
    outputSel.replaceChildren(new Option("(MIDI unavailable)", ""));
    setStatus("midi", String(err?.message ?? err));
  });
