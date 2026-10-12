// vstmidi: a code-based MIDI sequencer plugin that follows the host's transport.
//
// The code lives in the plugin state (so it's saved with the Ableton set), the editor parses it
// on every edit and hands the result to the audio thread, which runs it once per sixteenth note.

use nih_plug::prelude::*;
use nih_plug_egui::EguiState;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};

mod edit;
mod editor;
mod keys;
pub mod lang;
pub mod seq;

use lang::Program;
use seq::{Block, Event, Sequencer};

const DEFAULT_CODE: &str = "\
-- kick on every beat, snare on the backbeat
every 4 { note c1 }
at x.1|3.0 { note d1 }

-- closed hi-hat on the offbeat eighths
at x.y.2 { note f#1 vel 70 }

-- a crash at the start of every 4th bar
at 4x.0.0 { note c#2 dur 4 }
";

pub struct VstMidi {
    params: Arc<VstMidiParams>,
    /// The program the audio thread runs.
    program: Program,
    /// A freshly parsed program from the editor, picked up by the audio thread.
    inbox: Arc<Mutex<Option<Program>>>,
    seq: Sequencer,
    status: Arc<Status>,
}

#[derive(Params)]
pub struct VstMidiParams {
    #[persist = "editor-state"]
    editor_state: Arc<EguiState>,

    /// The source code. Not a parameter (those are only numbers), but persisted plugin state.
    #[persist = "code"]
    code: Arc<RwLock<String>>,
}

/// What the audio thread last saw, for the editor's status line.
#[derive(Default)]
pub struct Status {
    playing: AtomicBool,
    bar: AtomicI64,
    beat: AtomicU32,
    sixteenth: AtomicU32,
    tempo: AtomicF32,
    time_sig: AtomicU32, // numerator << 16 | denominator
}

impl Default for VstMidi {
    fn default() -> Self {
        Self {
            params: Arc::new(VstMidiParams::default()),
            program: Vec::new(),
            inbox: Arc::new(Mutex::new(None)),
            seq: Sequencer::default(),
            status: Arc::new(Status::default()),
        }
    }
}

impl Default for VstMidiParams {
    fn default() -> Self {
        Self {
            editor_state: EguiState::from_size(860, 520),
            code: Arc::new(RwLock::new(DEFAULT_CODE.to_string())),
        }
    }
}

impl Plugin for VstMidi {
    const NAME: &'static str = "vstmidi";
    const VENDOR: &'static str = "rocktober";
    const URL: &'static str = "";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    // No audio in, silent stereo out: that makes hosts (Ableton) treat it as an instrument, so it
    // can sit on a MIDI track and other tracks can take their MIDI from it.
    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[AudioIOLayout {
        main_input_channels: None,
        main_output_channels: NonZeroU32::new(2),
        ..AudioIOLayout::const_default()
    }];

    // We ignore incoming MIDI, but Ableton refuses to load a VST3 instrument without an event
    // input bus ("plugin has instrument category, but no valid event input bus").
    const MIDI_INPUT: MidiConfig = MidiConfig::Basic;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::Basic;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(self.params.clone(), self.inbox.clone(), self.status.clone())
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        _buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        // Also called after the host restores the state, so this picks up the saved code.
        if let Ok(program) = lang::parse(&self.params.code.read().unwrap()) {
            self.program = program;
        }
        self.seq = Sequencer::default();
        true
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        // Swap in new code without ever waiting for the editor. Sketch-level shortcut: the old
        // program gets deallocated here, on the audio thread.
        if let Ok(mut inbox) = self.inbox.try_lock()
            && let Some(program) = inbox.take()
        {
            self.program = program;
        }

        for channel in buffer.as_slice() {
            channel.fill(0.0);
        }

        let t = context.transport();
        let block = Block {
            playing: t.playing && t.pos_beats().is_some(),
            pos_beats: t.pos_beats().unwrap_or(0.0),
            tempo: t.tempo.unwrap_or(120.0),
            sample_rate: t.sample_rate,
            samples: buffer.samples() as u32,
            time_sig: (
                t.time_sig_numerator.unwrap_or(4).max(1) as u32,
                t.time_sig_denominator.unwrap_or(4).max(1) as u32,
            ),
            bar: t.bar_start_pos_beats().zip(t.bar_number()),
        };

        let pos = self.seq.process(&self.program, &block, &mut |event| {
            context.send_event(match event {
                Event::On { timing, ch, note, vel } => NoteEvent::NoteOn {
                    timing,
                    voice_id: None,
                    channel: ch - 1,
                    note,
                    velocity: vel as f32 / 127.0,
                },
                Event::Off { timing, ch, note } => NoteEvent::NoteOff {
                    timing,
                    voice_id: None,
                    channel: ch - 1,
                    note,
                    velocity: 0.0,
                },
            })
        });

        let s = &self.status;
        s.playing.store(pos.is_some(), Ordering::Relaxed);
        if let Some(pos) = pos {
            s.bar.store(pos.bar, Ordering::Relaxed);
            s.beat.store(pos.beat, Ordering::Relaxed);
            s.sixteenth.store(pos.sixteenth, Ordering::Relaxed);
        }
        s.tempo.store(block.tempo as f32, Ordering::Relaxed);
        s.time_sig.store(block.time_sig.0 << 16 | block.time_sig.1, Ordering::Relaxed);

        ProcessStatus::Normal
    }
}

impl ClapPlugin for VstMidi {
    const CLAP_ID: &'static str = "rocktober.vstmidi";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("A code-based MIDI sequencer");
    const CLAP_MANUAL_URL: Option<&'static str> = None;
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] =
        &[ClapFeature::Instrument, ClapFeature::NoteEffect, ClapFeature::Utility];
}

impl Vst3Plugin for VstMidi {
    const VST3_CLASS_ID: [u8; 16] = *b"RocktoberVstMidi";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] =
        &[Vst3SubCategory::Instrument, Vst3SubCategory::Tools];
}

nih_export_clap!(VstMidi);
nih_export_vst3!(VstMidi);
