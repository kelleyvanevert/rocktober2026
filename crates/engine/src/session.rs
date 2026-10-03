//! A running audio engine plus the evaluator that feeds it: the one object a
//! frontend (REPL, editor app, ...) needs to hold on to.
//!
//! Three threads share the work: the caller's (evaluating code), the audio
//! thread (rendering), and a scheduler thread that sends the notes of running
//! patterns a little ahead of time (see `scheduler`).

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use rtrb::Producer;

use crate::engine::{Command, Engine, MAX_BLOCK, Slot, Status};
use crate::eval::{Action, Evaluator};
use crate::lang;
use crate::nodes::{Frame, Node};
use crate::recorder::Recording;
use crate::resource::Resources;
use crate::scheduler::{self, Scheduler};

pub struct Session {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    shared: Arc<Mutex<Shared>>,
    status: Arc<Status>,
    evaluator: Evaluator,
    recording: Option<Recording>,
    _output: Output,
}

/// What the caller's thread and the scheduler thread both use. Neither holds
/// the lock for long, and the audio thread never takes it.
struct Shared {
    commands: Producer<Command>,
    scheduler: Scheduler,
    /// Commands sent so far.
    sent: u64,
}

impl Shared {
    fn new(commands: Producer<Command>, sample_rate: u32) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            commands,
            scheduler: Scheduler::new(sample_rate),
            sent: 0,
        }))
    }

    /// Where something started now starts: its beat and frame, and its slot.
    /// In a named slot, whatever played there stops at that moment.
    fn start(&mut self, now: u64, slot: Option<String>) -> (f64, u64, Slot) {
        let beat = self.scheduler.start_beat(now);
        let at = self.scheduler.clock.frame_at(beat);
        let slot = self.scheduler.slot(slot.as_deref());
        if slot != 0 {
            self.scheduler.end_slot(slot, beat);
            self.send(Command::StopSlot { slot, at });
        }
        (beat, at, slot)
    }

    fn send(&mut self, cmd: Command) {
        // The queue only fills up if the audio thread has stalled; dropping the
        // command is better than blocking.
        if self.commands.push(cmd).is_ok() {
            self.sent += 1;
        }
    }
}

/// Every `TICK`, send the notes that are coming up. Ends with the session.
fn run_scheduler(shared: Weak<Mutex<Shared>>, status: Arc<Status>, sample_rate: u32) {
    let lookahead = (scheduler::LOOKAHEAD_SECONDS * sample_rate as f64) as u64;
    loop {
        std::thread::sleep(scheduler::TICK);
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let Ok(mut shared) = shared.lock() else {
            return;
        };
        let now = status.frames.load(Ordering::Relaxed);
        for event in shared.scheduler.due(now + lookahead) {
            shared.send(Command::Play {
                node: event.node,
                at: Some(event.at),
                slot: event.slot,
            });
        }
    }
}

// The fields are only held, never read.
#[allow(dead_code)]
enum Output {
    // Dropping the stream stops audio, so the session owns it.
    Device(cpal::Stream),
    // No device: the engine is kept but never run (see `Session::without_output`).
    None(Engine),
}

impl Session {
    /// Open the default output device and start the audio thread.
    pub fn start(resources: Resources) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no output device")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let device_name = device
            .description()
            .map_or_else(|_| "unknown device".to_string(), |d| d.to_string());

        // Two lock-free single-producer/single-consumer queues: commands go to the
        // audio thread, finished nodes come back to be freed.
        let (commands, commands_rx) = rtrb::RingBuffer::<Command>::new(1024);
        let (garbage_tx, mut garbage) = rtrb::RingBuffer::<Box<dyn Node>>::new(1024);
        let status = Arc::new(Status::default());
        let engine = Engine::new(commands_rx, garbage_tx, status.clone(), config.sample_rate);

        let stream = match format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, engine),
            SampleFormat::I16 => build_stream::<i16>(&device, &config, engine),
            SampleFormat::I32 => build_stream::<i32>(&device, &config, engine),
            other => return Err(format!("unsupported sample format {other}")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;

        std::thread::spawn(move || {
            loop {
                while let Ok(node) = garbage.pop() {
                    drop(node);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        let shared = Shared::new(commands, config.sample_rate);
        let (weak, scheduler_status) = (Arc::downgrade(&shared), status.clone());
        std::thread::spawn(move || run_scheduler(weak, scheduler_status, config.sample_rate));

        Ok(Self {
            device_name,
            sample_rate: config.sample_rate,
            channels: config.channels,
            shared,
            status,
            evaluator: Evaluator::new(config.sample_rate, resources),
            recording: None,
            _output: Output::Device(stream),
        })
    }

    /// A session that evaluates code but plays nothing, for tests. Commands are
    /// queued but never consumed, and patterns never advance, so don't call
    /// `wait_until_idle` on it.
    pub fn without_output(sample_rate: u32, resources: Resources) -> Self {
        let (commands, commands_rx) = rtrb::RingBuffer::<Command>::new(1024);
        let (garbage_tx, _) = rtrb::RingBuffer::<Box<dyn Node>>::new(1);
        let status = Arc::new(Status::default());
        let engine = Engine::new(commands_rx, garbage_tx, status.clone(), sample_rate);
        Self {
            device_name: "no output".to_string(),
            sample_rate,
            channels: 2,
            shared: Shared::new(commands, sample_rate),
            status,
            evaluator: Evaluator::new(sample_rate, resources),
            recording: None,
            _output: Output::None(engine),
        }
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        // The scheduler thread can't panic while holding the lock in any way
        // that leaves the state broken, so a poisoned lock is still usable.
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Parse and evaluate `src`, and carry out what it asks for. Sounds and
    /// patterns start on the next bar; one played in a named slot replaces
    /// what was playing there, at that same moment.
    pub fn eval(&mut self, src: &str) -> Result<(), lang::Error> {
        let program = lang::parse(src)?;
        let actions = self.evaluator.run(&program)?;
        let now = self.status.frames.load(Ordering::Relaxed);
        let sample_rate = self.sample_rate;
        let mut shared = self.shared();
        for action in actions {
            match action {
                Action::Bpm(bpm) => shared.scheduler.clock.set_bpm(bpm, now),
                Action::StopAll => {
                    shared.scheduler.clear();
                    shared.send(Command::StopAll);
                }
                Action::Play { sound, slot } => {
                    let (_, at, slot) = shared.start(now, slot);
                    let node = sound.instantiate(sample_rate);
                    shared.send(Command::Play {
                        node,
                        at: Some(at),
                        slot,
                    });
                }
                Action::Pattern {
                    pattern,
                    instrument,
                    slot,
                } => {
                    let (beat, _, slot) = shared.start(now, slot);
                    shared.scheduler.add(pattern, instrument, slot, beat);
                }
            }
        }
        Ok(())
    }

    /// The tempo, and the current position in beats.
    pub fn position(&self) -> (f64, f64) {
        let now = self.status.frames.load(Ordering::Relaxed);
        let clock = &self.shared().scheduler.clock;
        (clock.bpm(), clock.beat_at(now))
    }

    /// Where the code's samples, envelopes, ... are.
    pub fn resources(&self) -> &Resources {
        self.evaluator.resources()
    }

    /// Fade out everything that's playing, and stop all patterns.
    pub fn stop_all(&mut self) {
        let mut shared = self.shared();
        shared.scheduler.clear();
        shared.send(Command::StopAll);
    }

    /// Number of voices currently playing.
    pub fn voices(&self) -> usize {
        self.status.voices.load(Ordering::Relaxed)
    }

    /// Block until every command sent so far has been picked up and all voices
    /// have finished.
    pub fn wait_until_idle(&self) {
        loop {
            // (One lock at a time: the guard must be gone before taking it again.)
            let (sent, patterns) = {
                let shared = self.shared();
                (shared.sent, shared.scheduler.patterns())
            };
            let received = self.status.commands_received.load(Ordering::Acquire);
            if received >= sent && self.voices() == 0 && patterns == 0 {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Start writing everything that's played to a WAV file at `path`.
    pub fn start_recording(&mut self, path: &Path) -> Result<(), String> {
        if self.recording.is_some() {
            return Err("already recording".into());
        }
        let (recording, producer) = Recording::start(path, self.sample_rate)?;
        self.send(Command::StartRecording(producer));
        self.recording = Some(recording);
        Ok(())
    }

    /// Stop recording. The file is finished in the background, within a few
    /// tens of milliseconds. Returns its path and how long the recording was.
    pub fn stop_recording(&mut self) -> Option<(PathBuf, Duration)> {
        let recording = self.recording.take()?;
        self.send(Command::StopRecording);
        Some((recording.path.clone(), recording.started.elapsed()))
    }

    /// How long the current recording has been running, if there is one.
    pub fn recording_time(&self) -> Option<Duration> {
        self.recording.as_ref().map(|r| r.started.elapsed())
    }

    fn send(&mut self, cmd: Command) {
        self.shared().send(cmd);
    }
}

impl Drop for Session {
    /// Quitting while recording: stop, and give the writer a moment to finish
    /// the file. (The audio stream is still running here, so the stop arrives.)
    fn drop(&mut self) {
        if let Some(recording) = self.recording.take() {
            self.send(Command::StopRecording);
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while !recording.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut engine: Engine,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut mix: Vec<Frame> = vec![[0.0; 2]; MAX_BLOCK];

    device.build_output_stream(
        *config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            // The device buffer is interleaved with `channels` channels and can be
            // any size, so render in blocks of at most MAX_BLOCK frames.
            for chunk in data.chunks_mut(MAX_BLOCK * channels) {
                let frames = chunk.len() / channels;
                let mix = &mut mix[..frames];
                engine.process(mix);
                for (out, &[l, r]) in chunk.chunks_mut(channels).zip(mix.iter()) {
                    match out {
                        [mono] => *mono = T::from_sample((l + r) * 0.5),
                        [left, right, rest @ ..] => {
                            *left = T::from_sample(l);
                            *right = T::from_sample(r);
                            rest.fill(T::EQUILIBRIUM);
                        }
                        [] => {}
                    }
                }
            }
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_drives_the_clock_and_patterns() {
        let samples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples");
        let resources = Resources {
            root: samples.clone(),
            sample_dirs: vec![samples],
        };
        let mut session = Session::without_output(48_000, resources);
        assert_eq!(session.position(), (120.0, 0.0));
        session.eval("140.bpm").unwrap();
        assert_eq!(session.position().0, 140.0);
        let drums = r#"notes("x . x x", 0.25b).play(sample("kick.mp3"), "drums")"#;
        session.eval(drums).unwrap();
        session.eval(drums).unwrap();
        // The first one is told to end where the second starts; it's dropped
        // once the scheduler gets there.
        assert_eq!(session.shared().scheduler.patterns(), 2);
        session.shared().scheduler.due(10 * 48_000);
        assert_eq!(session.shared().scheduler.patterns(), 1);
        session.stop_all();
        assert_eq!(session.shared().scheduler.patterns(), 0);
    }

    /// The whole path, without a device: evaluate a pattern, then alternate
    /// the scheduler's work with rendering blocks, as the two threads would.
    #[test]
    fn pattern_hits_land_on_their_frames() {
        let samples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples");
        let resources = Resources {
            root: samples.clone(),
            sample_dirs: vec![samples],
        };
        let mut session = Session::without_output(48_000, resources);
        // A hit every beat (24000 frames at 120 bpm), from the first bar line
        // after the lookahead: beat 4.
        session
            .eval(r#"notes("x", 1b).play(sample("kick.mp3").fit(10ms))"#)
            .unwrap();
        let Output::None(engine) = &mut session._output else {
            unreachable!()
        };
        let lookahead = (scheduler::LOOKAHEAD_SECONDS * 48_000.0) as u64;
        let mut out = Vec::new();
        let mut block = vec![[0.0; 2]; 480];
        while out.len() < 200_000 {
            let now = session.status.frames.load(Ordering::Relaxed);
            let mut shared = session.shared.lock().unwrap();
            for event in shared.scheduler.due(now + lookahead) {
                shared.send(Command::Play {
                    node: event.node,
                    at: Some(event.at),
                    slot: event.slot,
                });
            }
            drop(shared);
            engine.process(&mut block);
            out.extend(block.iter().map(|f| f[0].abs()));
        }
        let onsets: Vec<usize> = (1..out.len())
            .filter(|&i| {
                out[i] > 0.0
                    && out[i - 1] == 0.0
                    && out[i - 480.min(i)..i].iter().all(|s| *s == 0.0)
            })
            .collect();
        assert_eq!(onsets, [96_000, 120_000, 144_000, 168_000, 192_000]);
    }
}
