// Thin wrapper around the Web MIDI API.

let access: MIDIAccess | null = null;

export async function initMidi(onChange: () => void): Promise<MIDIAccess> {
  if (!navigator.requestMIDIAccess) {
    throw new Error("Web MIDI is not supported in this browser (use Chrome/Edge/Firefox).");
  }
  access = await navigator.requestMIDIAccess({ sysex: false });
  access.onstatechange = onChange;
  return access;
}

export function listOutputs(): MIDIOutput[] {
  return access ? [...access.outputs.values()] : [];
}

export function getOutput(id: string): MIDIOutput | undefined {
  return access?.outputs.get(id);
}

// `time` is a performance.now() timestamp; the browser delivers the message at that moment.
export function noteOn(out: MIDIOutput, ch: number, note: number, vel: number, time?: number) {
  out.send([0x90 | (ch - 1), note, vel], time);
}

export function noteOff(out: MIDIOutput, ch: number, note: number, time?: number) {
  out.send([0x80 | (ch - 1), note, 0], time);
}

export function panic(out: MIDIOutput) {
  for (let ch = 0; ch < 16; ch++) {
    out.send([0xb0 | ch, 123, 0]); // all notes off
    out.send([0xb0 | ch, 120, 0]); // all sound off
  }
}

/** One number per (channel 1-16, note) pair, for use as a map key. */
export const noteKey = (channel: number, note: number) => (channel - 1) * 128 + note;
const fromNoteKey = (key: number) => [Math.floor(key / 128) + 1, key % 128] as const;

/**
 * Tracks sounding notes and releases them on frame boundaries. Note-offs are sent
 * per frame instead of being scheduled up front, so a retriggered note can't be cut
 * short by the note-off of an earlier, shorter note of the same pitch.
 */
export class Voices {
  /** noteKey -> frame at which it ends */
  private ends = new Map<number, number>();

  /** `notes` maps noteKey(channel, note) -> duration in frames. */
  frame(out: MIDIOutput | undefined, frame: number, time: number, notes: Map<number, number>) {
    for (const [key, end] of this.ends) {
      if (end > frame) continue;
      if (out) noteOff(out, ...fromNoteKey(key), time);
      this.ends.delete(key);
    }
    for (const [key, dur] of notes) {
      if (out) {
        const [ch, note] = fromNoteKey(key);
        if (this.ends.has(key)) noteOff(out, ch, note, time);
        noteOn(out, ch, note, 100, time);
      }
      this.ends.set(key, frame + dur);
    }
  }

  releaseAll(out: MIDIOutput | undefined, time?: number) {
    if (out) for (const key of this.ends.keys()) noteOff(out, ...fromNoteKey(key), time);
    this.ends.clear();
  }
}
