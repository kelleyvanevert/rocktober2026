use nih_plug::prelude::*;
use vstmidi::VstMidi;

fn main() {
    nih_export_standalone::<VstMidi>();
}
