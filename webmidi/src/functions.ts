// Built-in functions callable from automaton code, e.g. `scale("dorian", d3, hitv)`.
// The parser checks calls against `params` (arity, and string arguments against
// `choices`); the interpreter calls `fn`. The sidebar help is generated from `docs`.

export type Param =
  | { name: string; type: "number" }
  | { name: string; type: "string"; choices: string[] };

export interface Builtin {
  params: Param[];
  /** Arguments used when the name appears without parentheses (plain `rand`). */
  bare?: number[];
  /** [usage, description] lines for the sidebar. */
  docs: [string, string][];
  fn: (args: (number | string)[]) => number;
}

/** Semitone offsets from the root, one octave. */
export const SCALES: Record<string, number[]> = {
  ionian: [0, 2, 4, 5, 7, 9, 11],
  major: [0, 2, 4, 5, 7, 9, 11],
  dorian: [0, 2, 3, 5, 7, 9, 10],
  phrygian: [0, 1, 3, 5, 7, 8, 10],
  lydian: [0, 2, 4, 6, 7, 9, 11],
  mixolydian: [0, 2, 4, 5, 7, 9, 10],
  aeolian: [0, 2, 3, 5, 7, 8, 10],
  minor: [0, 2, 3, 5, 7, 8, 10],
  locrian: [0, 1, 3, 5, 6, 8, 10],
  "harmonic minor": [0, 2, 3, 5, 7, 8, 11],
  "melodic minor": [0, 2, 3, 5, 7, 9, 11],
  "major pentatonic": [0, 2, 4, 7, 9],
  "minor pentatonic": [0, 3, 5, 7, 10],
  blues: [0, 3, 5, 6, 7, 10],
  "whole tone": [0, 2, 4, 6, 8, 10],
  chromatic: [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11],
};

const num = (v: number | string) => Math.round(v as number);

export const FUNCTIONS: Record<string, Builtin> = {
  rand: {
    params: [
      { name: "min", type: "number" },
      { name: "max", type: "number" },
    ],
    bare: [0, 8],
    docs: [
      ["rand", "random int 0-8"],
      ["rand(min, max)", "random int, both ends included"],
    ],
    fn: ([a, b]) => {
      const [lo, hi] = num(a) <= num(b) ? [num(a), num(b)] : [num(b), num(a)];
      return lo + Math.floor(Math.random() * (hi - lo + 1));
    },
  },

  scale: {
    params: [
      { name: "name", type: "string", choices: Object.keys(SCALES) },
      { name: "root", type: "number" },
      { name: "degree", type: "number" },
    ],
    docs: [
      ["scale(name, root, degree)", ""],
      ["", "note `degree` steps up the scale"],
      ["", "from root: 0 = root, 7 = octave"],
      ["", "(7-note scales), -1 = step down"],
      ['scale("dorian", d3, hitv)', ""],
    ],
    fn: ([name, root, degree]) => {
      const steps = SCALES[name as string];
      const d = num(degree);
      const octave = Math.floor(d / steps.length);
      return num(root) + 12 * octave + steps[d - octave * steps.length];
    },
  },
};
