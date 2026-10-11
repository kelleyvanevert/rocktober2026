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
