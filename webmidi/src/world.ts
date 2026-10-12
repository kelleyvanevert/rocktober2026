// The automaton world: positions, the per-frame step, and the interpreter.

import type { Dir, Expr, Program } from "./lang";
import { noteKey } from "./midi";
import { FUNCTIONS } from "./functions";

export interface Automaton {
  id: number;
  col: number;
  row: number;
  program: Program;
  code: string;
  /** Set with `value <expr>`, read by others on the same cell as `hitv`. */
  value: number;
  /** Has run at least once (so `once { … }` blocks are done). */
  started: boolean;
}

export interface CellView {
  /** Automatons on this cell, oldest first. */
  ids: number[];
  /** Sum of the values of the automatons here, at the start of the frame. */
  valueSum: number;
  /** How many of them have any code to run (commented-out ones don't). */
  active: number;
  sounding: boolean;
}

/** What a frame looks like: occupancy before the frame's moves were applied. */
export interface View {
  frame: number;
  cells: Map<string, CellView>;
}

export interface FrameResult {
  view: View;
  /** noteKey(channel, note) -> duration in frames */
  notes: Map<number, number>;
}

/** Plain-data form of the world, for saving. */
export interface SavedWorld {
  frame: number;
  automatons: { col: number; row: number; code: string; value: number; started: boolean }[];
}

/** Upper bound so runaway spawning can't freeze the page. */
const MAX_AUTOMATONS = 2000;

const DELTA: Record<Dir, [number, number]> = {
  left: [-1, 0],
  right: [1, 0],
  up: [0, -1],
  down: [0, 1],
};

const HERE: [number, number][] = [[0, 0]];
const NEIGHBOURS: [number, number][] = [
  [-1, -1], [0, -1], [1, -1],
  [-1, 0],           [1, 0],
  [-1, 1],  [0, 1],  [1, 1],
];

export const cellKey = (col: number, row: number) => `${col},${row}`;

interface Ctx {
  hit: number;
  hitv: number;
  /** The automaton's own value; assignments are visible to itself right away. */
  value: number;
  /** First frame this automaton runs: `once` blocks execute. */
  first: boolean;
  frame: number;
  dx: number;
  dy: number;
  notes: [number, number][];
  spawns: { program: Program; code: string; spread: boolean }[];
  kill: boolean;
  /** Code from the last `morph` block this frame, if any. */
  morph: { program: Program; code: string } | null;
}

export class World {
  frame = 0;
  automatons: Automaton[] = [];
  private nextId = 1;

  add(col: number, row: number, program: Program, code: string, value = 0, started = false) {
    if (this.automatons.length >= MAX_AUTOMATONS) return;
    this.automatons.push({ id: this.nextId++, col, row, program, code, value, started });
  }

  get(id: number): Automaton | undefined {
    return this.automatons.find((a) => a.id === id);
  }

  remove(ids: number[]) {
    const gone = new Set(ids);
    this.automatons = this.automatons.filter((a) => !gone.has(a.id));
  }

  clear() {
    this.automatons = [];
    this.frame = 0;
  }

  save(): SavedWorld {
    return { frame: this.frame, automatons: this.automatons.map(({ col, row, code, value, started }) => ({ col, row, code, value, started })) };
  }

  /** Current state, for drawing while paused. */
  view(): View {
    return { frame: this.frame, cells: this.occupancy() };
  }

  /**
   * Run one frame. Every automaton sees the same starting positions and values (so
   * `hit` and `hitv` don't depend on evaluation order). Moves, spawns, kills and morphs
   * are applied afterwards. `kill` removes whoever shared the killer's cell at the start
   * of the frame (so two killers on a cell remove each other); `morph` gives them new
   * code instead (the oldest morpher wins, and kill beats morph). Then anything that
   * ended up outside `cols` x `rows` is removed.
   */
  step(cols: number, rows: number): FrameResult {
    const cells = this.occupancy();
    const notes = new Map<number, number>();
    const spawned: Automaton[] = [];
    const killed = new Set<number>();
    const morphed = new Map<number, { program: Program; code: string }>();

    for (const a of this.automatons) {
      const cell = cells.get(cellKey(a.col, a.row))!;
      const ctx: Ctx = {
        hit: cell.ids.length - 1,
        hitv: cell.valueSum - a.value,
        value: a.value,
        first: !a.started,
        frame: this.frame,
        dx: 0,
        dy: 0,
        notes: [],
        spawns: [],
        kill: false,
        morph: null,
      };
      exec(a.program, ctx);
      a.value = ctx.value;
      a.started = true;

      for (const [note, dur] of ctx.notes) {
        notes.set(note, Math.max(dur, notes.get(note) ?? 0));
        cell.sounding = true;
      }
      for (const { program, code, spread } of ctx.spawns) {
        for (const [dx, dy] of spread ? NEIGHBOURS : HERE) {
          const child = { col: a.col + dx, row: a.row + dy, program, code, value: 0, started: false };
          spawned.push({ id: this.nextId++, ...child });
        }
      }
      for (const id of cell.ids) {
        if (id === a.id) continue;
        if (ctx.kill) killed.add(id);
        // Automatons run oldest first, so the first morph to claim a target is the oldest.
        if (ctx.morph && !morphed.has(id)) morphed.set(id, ctx.morph);
      }
      a.col += ctx.dx;
      a.row += ctx.dy;
    }

    const survivors = this.automatons.filter((a) => !killed.has(a.id));
    for (const a of survivors) {
      const m = morphed.get(a.id);
      if (!m) continue;
      a.program = m.program;
      a.code = m.code;
      a.started = false; // so the new code's `once` blocks run
    }
    const room = Math.max(0, MAX_AUTOMATONS - survivors.length);
    this.automatons = survivors
      .concat(spawned.slice(0, room))
      .filter((a) => a.col >= 0 && a.row >= 0 && a.col < cols && a.row < rows);

    const view = { frame: this.frame, cells };
    this.frame++;
    return { view, notes };
  }

  private occupancy() {
    const cells = new Map<string, CellView>();
    for (const a of this.automatons) {
      const k = cellKey(a.col, a.row);
      const cell = cells.get(k) ?? { ids: [], valueSum: 0, active: 0, sounding: false };
      if (a.program.length) cell.active++;
      cell.ids.push(a.id);
      cell.valueSum += a.value;
      cells.set(k, cell);
    }
    return cells;
  }
}

function exec(program: Program, ctx: Ctx) {
  for (const s of program) {
    switch (s.kind) {
      case "step": {
        const n = s.count ? Math.round(evaluate(s.count, ctx)) : 1;
        ctx.dx += DELTA[s.dir][0] * n;
        ctx.dy += DELTA[s.dir][1] * n;
        break;
      }
      case "note": {
        const note = Math.round(evaluate(s.note, ctx));
        const channel = s.channel ? Math.round(evaluate(s.channel, ctx)) : 1;
        const dur = s.dur ? Math.round(evaluate(s.dur, ctx)) : 1;
        if (note >= 0 && note <= 127 && channel >= 1 && channel <= 16) {
          ctx.notes.push([noteKey(channel, note), Math.max(1, dur)]);
        }
        break;
      }
      case "pattern":
        if (s.steps[ctx.frame % s.steps.length]) exec(s.body, ctx);
        break;
      case "every": {
        const n = Math.round(evaluate(s.n, ctx));
        if (n > 0 && ctx.frame % n === 0) exec(s.body, ctx);
        break;
      }
      case "if":
        if (evaluate(s.cond, ctx) !== 0) exec(s.then, ctx);
        else if (s.else) exec(s.else, ctx);
        break;
      case "spawn":
        ctx.spawns.push({ program: s.body, code: s.source, spread: s.spread });
        break;
      case "value":
        ctx.value = evaluate(s.value, ctx);
        break;
      case "once":
        if (ctx.first) exec(s.body, ctx);
        break;
      case "morph":
        ctx.morph = { program: s.body, code: s.source };
        break;
      case "kill":
        ctx.kill = true;
        break;
    }
  }
}

function evaluate(e: Expr, ctx: Ctx): number {
  switch (e.kind) {
    case "num":
      return e.value;
    case "var":
      return ctx[e.name];
    case "neg":
      return -evaluate(e.arg, ctx);
    case "call":
      return FUNCTIONS[e.name].fn(e.args.map((a) => (a.kind === "str" ? a.value : evaluate(a, ctx))));
    case "bin": {
      const a = evaluate(e.left, ctx);
      const b = evaluate(e.right, ctx);
      switch (e.op) {
        case "+": return a + b;
        case "-": return a - b;
        case "*": return a * b;
        case "/": return b === 0 ? 0 : a / b;
        case "%": return b === 0 ? 0 : ((a % b) + b) % b;
        case ">=": return +(a >= b);
        case "<=": return +(a <= b);
        case ">": return +(a > b);
        case "<": return +(a < b);
        case "==": return +(a === b);
        case "!=": return +(a !== b);
      }
    }
  }
}
