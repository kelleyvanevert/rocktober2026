//! A running audio engine plus the evaluator that feeds it: the one object a
//! frontend (REPL, editor app, ...) needs to hold on to.
//!
//! Three threads share the work: the caller's (evaluating code), the audio
//! thread (rendering), and a scheduler thread that sends the notes of running
//! patterns a little ahead of time (see `scheduler`).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use rtrb::Producer;

use crate::bundle::Bundle;
use crate::clock::Grid;
use crate::engine::{Command, Engine, MAX_BLOCK, Slot, Status};
use crate::eval::{self, Action, Evaluator};
use crate::lang;
use crate::nodes::{Frame, Node};
use crate::recorder::Recording;
use crate::scheduler::{self, Scheduler};

/// How soon something can start: the next audio block has to have picked up
/// the command, and a block can be ~20 ms long. Anything sooner would start
/// late, out of step with the rest of its pattern.
const START_MARGIN_SECONDS: f64 = 0.05;

pub struct Session {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    shared: Arc<Mutex<Shared>>,
    status: Arc<Status>,
    evaluator: Evaluator,
    recording: Option<Recording>,
    output: Output,
    /// See `START_MARGIN_SECONDS`, in frames (0 without a device: nothing
    /// runs until `render` is called).
    margin: u64,
    /// The buses the engine has been told about, by slot name.
    buses: HashSet<String>,
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

    /// Where something that can start at frame `earliest` starts: its beat
    /// and frame, and its slot. In a named slot, whatever played there stops
    /// at that moment.
    fn start(
        &mut self,
        earliest: u64,
        slot: Option<String>,
        grid: Option<Grid>,
    ) -> (f64, u64, Slot) {
        let beat = self.scheduler.start_beat(earliest, grid);
        let at = self.scheduler.clock.frame_at(beat);
        let slot = self.scheduler.slot(slot.as_deref());
        if slot != 0 {
            self.stop_slot(slot, beat);
        }
        (beat, at, slot)
    }

    /// Stop a slot's patterns and voices at `beat`.
    fn stop_slot(&mut self, slot: Slot, beat: f64) {
        self.scheduler.end_slot(slot, beat);
        let at = self.scheduler.clock.frame_at(beat);
        self.send(Command::StopSlot { slot, at });
    }

    fn send(&mut self, cmd: Command) {
        // The queue only fills up if the audio thread has stalled; dropping the
        // command is better than blocking.
        if self.commands.push(cmd).is_ok() {
            self.sent += 1;
        }
    }

    /// Send the notes that start before `horizon`.
    fn send_due(&mut self, horizon: u64) {
        for event in self.scheduler.due(horizon) {
            self.send(Command::Play {
                node: event.node,
                at: Some(event.at),
                slot: event.slot,
            });
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
        shared.send_due(now + lookahead);
    }
}

enum Output {
    // Dropping the stream stops audio, so the session owns it (and never reads it).
    #[allow(dead_code)]
    Device(cpal::Stream),
    // No device: the engine only runs when `render` asks.
    None(Engine),
}

impl Session {
    /// Open the default output device and start the audio thread.
    pub fn start(bundle: Bundle) -> Result<Self, String> {
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
            evaluator: Evaluator::new(config.sample_rate, bundle),
            recording: None,
            output: Output::Device(stream),
            margin: (START_MARGIN_SECONDS * config.sample_rate as f64) as u64,
            buses: HashSet::new(),
        })
    }

    /// A session without a device: it plays nothing by itself, but `render`
    /// renders what it would play, as fast as it can (for tests, and for
    /// rendering files). Don't call `wait_until_idle` on it.
    pub fn without_output(sample_rate: u32, bundle: Bundle) -> Self {
        let (commands, commands_rx) = rtrb::RingBuffer::<Command>::new(4096);
        let (garbage_tx, _) = rtrb::RingBuffer::<Box<dyn Node>>::new(1);
        let status = Arc::new(Status::default());
        let engine = Engine::new(commands_rx, garbage_tx, status.clone(), sample_rate);
        Self {
            device_name: "no output".to_string(),
            sample_rate,
            channels: 2,
            shared: Shared::new(commands, sample_rate),
            status,
            evaluator: Evaluator::new(sample_rate, bundle),
            recording: None,
            output: Output::None(engine),
            margin: 0,
            buses: HashSet::new(),
        }
    }

    /// Render the next `frames` frames, for a session without a device:
    /// the scheduler's work and the audio thread's, taking turns. Commands
    /// sent by `eval` before this are played as they would be live.
    pub fn render(&mut self, frames: usize) -> Vec<Frame> {
        let Output::None(engine) = &mut self.output else {
            panic!("render: this session plays on a device");
        };
        let lookahead = (scheduler::LOOKAHEAD_SECONDS * self.sample_rate as f64) as u64;
        let block = 512;
        let mut out = Vec::with_capacity(frames);
        let mut buf = vec![[0.0; 2]; block];
        while out.len() < frames {
            let now = self.status.frames.load(Ordering::Relaxed);
            self.shared
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send_due(now + lookahead);
            let n = block.min(frames - out.len());
            engine.process(&mut buf[..n]);
            out.extend_from_slice(&buf[..n]);
        }
        out
    }

    fn shared(&self) -> MutexGuard<'_, Shared> {
        // The scheduler thread can't panic while holding the lock in any way
        // that leaves the state broken, so a poisoned lock is still usable.
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Parse and evaluate `src`, and carry out what it asks for. Sounds and
    /// patterns start right away (everything in `src` at the same moment), or
    /// on the next point of their grid (`at 4b`); one played in a named slot
    /// replaces what was playing there, at that same moment.
    pub fn eval(&mut self, src: &str) -> Result<(), lang::Error> {
        let program = lang::parse(src)?;
        let actions = self.evaluator.run(&program)?;
        let now = self.status.frames.load(Ordering::Relaxed);
        let earliest = now + self.margin;
        let sample_rate = self.sample_rate;
        let mut shared = self.shared.lock().unwrap_or_else(|e| e.into_inner());
        for (name, bus) in self.evaluator.buses() {
            if self.buses.insert(name.clone()) {
                let slot = shared.scheduler.slot(Some(name));
                shared.send(Command::Bus {
                    slot,
                    bus: bus.clone(),
                });
            }
        }
        for action in actions {
            match action {
                Action::Bpm(bpm) => shared.scheduler.clock.set_bpm(bpm, now),
                Action::StopAll => {
                    shared.scheduler.clear();
                    shared.send(Command::StopAll);
                }
                Action::Play { sound, slot, at } => {
                    let (_, at, slot) = shared.start(earliest, slot, at);
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
                    at,
                } => {
                    let (beat, _, slot) = shared.start(earliest, slot, at);
                    shared.scheduler.add(pattern, instrument, slot, beat);
                }
            }
        }
        // Send the first notes now rather than on the scheduler's next tick,
        // so even one that starts right away is on time.
        let lookahead = (scheduler::LOOKAHEAD_SECONDS * sample_rate as f64) as u64;
        shared.send_due(now + lookahead);
        Ok(())
    }

    /// Stop what the code in `src` plays into named slots (see
    /// `eval::named_slots`), without running it: right away, or on the next
    /// point of the grid it starts on. Returns the slot names.
    pub fn stop_named(&mut self, src: &str) -> Result<Vec<String>, lang::Error> {
        let named = eval::named_slots(&lang::parse(src)?);
        let earliest = self.status.frames.load(Ordering::Relaxed) + self.margin;
        let mut shared = self.shared();
        for (name, grid) in &named {
            let beat = shared.scheduler.start_beat(earliest, *grid);
            let slot = shared.scheduler.slot(Some(name));
            shared.stop_slot(slot, beat);
        }
        Ok(named.into_iter().map(|(name, _)| name).collect())
    }

    /// The tempo, and the current position in beats.
    pub fn position(&self) -> (f64, f64) {
        let now = self.status.frames.load(Ordering::Relaxed);
        let clock = &self.shared().scheduler.clock;
        (clock.bpm(), clock.beat_at(now))
    }

    /// The resources in the `.rock` file: its samples, envelopes, ...
    pub fn bundle(&self) -> &Bundle {
        self.evaluator.bundle()
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
        let mut session = Session::without_output(48_000, Bundle::with_kick());
        assert_eq!(session.position(), (120.0, 0.0));
        session.eval("140.bpm").unwrap();
        assert_eq!(session.position().0, 140.0);
        let drums = r#"notes("x . x x", 0.25b).play(sample("kick.mp3"), "drums")"#;
        session.eval(drums).unwrap();
        session.eval(drums).unwrap();
        // The first one is told to end where the second starts (right away,
        // so it's dropped right away).
        assert_eq!(session.shared().scheduler.patterns(), 1);
        session.stop_all();
        assert_eq!(session.shared().scheduler.patterns(), 0);

        // Stopping a block's slots doesn't run it, so its pattern isn't added.
        session.eval(drums).unwrap();
        assert_eq!(session.stop_named(drums).unwrap(), ["drums"]);
        session.shared().scheduler.due(20 * 48_000);
        assert_eq!(session.shared().scheduler.patterns(), 0);
        assert!(
            session
                .stop_named("sample(\"kick.mp3\").play")
                .unwrap()
                .is_empty()
        );
    }

    /// Onsets: where the left channel goes from a block of silence to sound.
    fn onsets(out: &[Frame]) -> Vec<usize> {
        (0..out.len())
            .filter(|&i| out[i][0] != 0.0 && out[i - 480.min(i)..i].iter().all(|f| f[0] == 0.0))
            .collect()
    }

    /// The whole path, without a device: evaluate a pattern, then render,
    /// which alternates the scheduler's work with the audio thread's.
    #[test]
    fn pattern_hits_land_on_their_frames() {
        let mut session = Session::without_output(48_000, Bundle::with_kick());
        session.render(10_000);
        // A hit every beat (24000 frames at 120 bpm), from the next bar line:
        // beat 4.
        session
            .eval(r#"notes("x", 1b).play(sample("kick.mp3").fit(10ms), at 1bar)"#)
            .unwrap();
        let out = session.render(200_000);
        let at: Vec<usize> = onsets(&out).iter().map(|i| i + 10_000).collect();
        assert_eq!(at, [96_000, 120_000, 144_000, 168_000, 192_000]);
    }

    #[test]
    fn plays_start_right_away_or_on_their_grid() {
        let mut session = Session::without_output(48_000, Bundle::with_kick());
        session.render(30_000);
        let kick = r#"sample("kick.mp3").fit(10ms)"#;
        session
            .eval(&format!(
                "{kick}.play\n{kick}.delay(100ms).play(\"a\", at 5b + 2)"
            ))
            .unwrap();
        // Right away, and on beat 2 (frame 48000), then the next one on the
        // grid is beat 7.
        let out = session.render(150_000);
        let at: Vec<usize> = onsets(&out).iter().map(|i| i + 30_000).collect();
        assert_eq!(at, [30_000, 48_000 + 4_800]);
        assert_eq!(
            session.stop_named(r#"x.play("a", at 5b + 2)"#).unwrap(),
            ["a"]
        );
    }

    #[test]
    fn ducking_follows_the_key_slot() {
        let render = |src: &str| {
            let mut session = Session::without_output(48_000, Bundle::with_kick());
            session.eval(src).unwrap();
            session.render(48_000)
        };
        let kicks = r#"notes("x", 1b).play(sample("kick.mp3").fit(100ms), "kick")"#;
        // A steady tone, ducked under a kick on every beat; minus the kicks,
        // that leaves the ducked tone.
        let mix = render(&format!(
            "wavetable(\"basic\", 0, 0, a4).duck(\"kick\", 1, 50ms).play\n{kicks}"
        ));
        let kick = render(kicks);
        let tone: Vec<f32> = mix.iter().zip(&kick).map(|(m, k)| m[0] - k[0]).collect();
        let level = |from: usize| {
            tone[from..from + 480]
                .iter()
                .fold(0f32, |m, s| m.max(s.abs()))
        };
        assert!(
            (level(18_000) - 0.5).abs() < 0.01,
            "between the kicks: {}",
            level(18_000)
        );
        assert!(level(24_480) < 0.01, "under a kick: {}", level(24_480));
    }
}
