import { initMidi, listOutputs, getOutput, noteOn, noteOff, panic } from "./midi";
import { Scheduler } from "./scheduler";

// The beat. Notes follow Ableton's Drum Rack layout (C1 = 36).
// "x" = hit, "-" = rest; one character per 16th note.
const pattern = [
  { name: "kick", note: 36, steps: "x---x---x---x---" },
  { name: "snare", note: 38, steps: "----x-------x---" },
  { name: "hat", note: 42, steps: "--x---x---x---xx" },
];

const $ = <T extends HTMLElement>(id: string) => document.getElementById(id) as T;
const outputSel = $<HTMLSelectElement>("output");
const channelSel = $<HTMLSelectElement>("channel");
const bpmInput = $<HTMLInputElement>("bpm");
const startBtn = $<HTMLButtonElement>("start");
const stopBtn = $<HTMLButtonElement>("stop");
const panicBtn = $<HTMLButtonElement>("panic");
const stepsEl = $<HTMLElement>("steps");
const statusEl = $<HTMLElement>("status");

const store = {
  get: (k: string) => { try { return localStorage.getItem(k); } catch { return null; } },
  set: (k: string, v: string) => { try { localStorage.setItem(k, v); } catch {} },
};

// --- UI setup ---

for (let ch = 1; ch <= 16; ch++) channelSel.add(new Option(String(ch), String(ch)));
channelSel.value = store.get("channel") ?? "1";
channelSel.onchange = () => store.set("channel", channelSel.value);

const cells: HTMLElement[][] = pattern.map((track) => {
  const name = document.createElement("div");
  name.className = "name";
  name.textContent = track.name;
  stepsEl.append(name);
  return [...track.steps].map((c) => {
    const cell = document.createElement("div");
    cell.className = "cell" + (c === "x" ? " on" : "");
    stepsEl.append(cell);
    return cell;
  });
});

function highlight(step: number | null) {
  for (const row of cells) row.forEach((cell, i) => cell.classList.toggle("now", i === step));
}

function refreshOutputs() {
  const outputs = listOutputs();
  const previous = outputSel.value || store.get("output");
  outputSel.replaceChildren(
    ...(outputs.length
      ? outputs.map((o) => new Option(o.name ?? o.id, o.id))
      : [new Option("(no MIDI outputs found)", "")]),
  );
  if (previous && outputs.some((o) => o.id === previous)) outputSel.value = previous;
}

outputSel.onchange = () => store.set("output", outputSel.value);

// --- Playback ---

const scheduler = new Scheduler((step, time, dur) => {
  const out = getOutput(outputSel.value);
  const ch = Number(channelSel.value);
  const i = step % 16;

  if (out) {
    for (const track of pattern) {
      if (track.steps[i] !== "x") continue;
      noteOn(out, ch, track.note, 100, time);
      noteOff(out, ch, track.note, time + dur * 0.9);
    }
  }

  // Move the playhead when the step actually sounds, not when it's scheduled.
  setTimeout(() => scheduler.running && highlight(i), time - performance.now());
});

bpmInput.oninput = () => {
  const bpm = Number(bpmInput.value);
  if (bpm >= 20 && bpm <= 400) scheduler.bpm = bpm;
};

startBtn.onclick = () => {
  if (!getOutput(outputSel.value)) {
    statusEl.textContent = "Pick a MIDI output first.";
    return;
  }
  scheduler.start();
  startBtn.disabled = true;
  stopBtn.disabled = false;
};

stopBtn.onclick = () => {
  scheduler.stop();
  const out = getOutput(outputSel.value);
  // Notes up to `lookahead` ms ahead are already queued in the browser, so silence
  // again once those have gone out.
  if (out) {
    panic(out);
    setTimeout(() => panic(out), scheduler.lookahead + 50);
  }
  highlight(null);
  startBtn.disabled = false;
  stopBtn.disabled = true;
};

panicBtn.onclick = () => listOutputs().forEach(panic);

// --- Boot ---

initMidi(refreshOutputs)
  .then(() => {
    refreshOutputs();
    statusEl.textContent =
      "Tip: on macOS, enable the IAC Driver in Audio MIDI Setup and pick it here; in Ableton, enable Track input for it.";
  })
  .catch((err) => {
    outputSel.replaceChildren(new Option("(MIDI unavailable)", ""));
    statusEl.textContent = String(err?.message ?? err);
  });
