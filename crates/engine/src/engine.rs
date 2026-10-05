//! The audio-thread side: receives commands, mixes the playing voices.
//!
//! The audio thread owns the one true clock: the number of frames it has
//! rendered. Commands can say at which frame they should happen; they wait in a
//! pending list until the block that contains that frame, and a voice then
//! starts at exactly that frame within the block. Deciding *when* things happen
//! (tempo, bars, patterns) is up to the application thread, which looks a
//! little ahead and sends timed commands (see `scheduler`).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rtrb::{Consumer, Producer};

use crate::nodes::{Frame, Limiter, Node};
use crate::sidechain::Bus;

/// The largest block `Engine::process` handles; the audio callback splits bigger
/// device buffers into chunks of this size.
pub const MAX_BLOCK: usize = 1024;
const MAX_VOICES: usize = 256;
/// Timed commands waiting for their block.
const MAX_PENDING: usize = 1024;
/// Slots that can be ducked under.
const MAX_BUSES: usize = 32;

/// The limiter on the mix: it looks this far ahead (which delays everything
/// that much), and recovers over this long. Its ceiling is full scale, so a
/// mix that wouldn't clip passes through untouched. The release is slow on
/// purpose: a faster one lets the gain wobble along with the peaks of a
/// chord (at the differences between its notes), which adds tones of its
/// own; this way a mix that's too loud is just turned down for a while.
const MASTER_LOOKAHEAD_SECONDS: f64 = 0.005;
const MASTER_RELEASE_SECONDS: f64 = 1.0;

/// How many frames late the mix comes out of the engine (the master
/// limiter's lookahead), after the frame it was timed for.
pub fn latency(sample_rate: u32) -> usize {
    (MASTER_LOOKAHEAD_SECONDS * sample_rate as f64).round() as usize
}

/// A named group of voices (see `Session`), so they can be stopped together.
/// 0 means none.
pub type Slot = u32;

/// Messages from the application thread to the audio thread.
pub enum Command {
    /// Start playing at frame `at` (now, if it's `None` or already past).
    Play {
        node: Box<dyn Node>,
        at: Option<u64>,
        slot: Slot,
    },
    /// Fade out the voices in a slot, at frame `at`.
    StopSlot {
        slot: Slot,
        at: u64,
    },
    StopAll,
    /// From now on, copy the level of the voices in `slot` into `bus` (for
    /// sidechaining, see `sidechain`).
    Bus {
        slot: Slot,
        bus: Arc<Bus>,
    },
    /// Copy the output (interleaved stereo) into this queue until told to stop.
    StartRecording(Producer<f32>),
    StopRecording,
}

/// Feedback from the audio thread, readable from anywhere.
#[derive(Default)]
pub struct Status {
    /// Frames rendered so far: the clock everything is timed by.
    pub frames: AtomicU64,
    pub voices: AtomicUsize,
    pub commands_received: AtomicU64,
    /// Samples the recorder couldn't keep up with (should stay 0).
    pub recording_dropped: AtomicU64,
    /// Frames where the mix went over full scale, so the master limiter
    /// turned it down (without it, they'd have clipped).
    pub clipped: AtomicU64,
}

struct Voice {
    node: Box<dyn Node>,
    /// Frames left in a stop fade-out, if the voice is being stopped.
    stopping: Option<usize>,
    slot: Slot,
    /// Frames into the current block before the voice starts.
    delay: usize,
}

enum Pending {
    Play(Box<dyn Node>, Slot),
    Stop(Slot),
}

pub struct Engine {
    commands: Consumer<Command>,
    /// Finished nodes go back to the app thread to be freed there: `free()` can
    /// take a lock inside the allocator, which we never want on the audio thread.
    garbage: Producer<Box<dyn Node>>,
    status: Arc<Status>,
    voices: Vec<Voice>,
    /// Timed commands, unordered, with the frame they're for.
    pending: Vec<(u64, Pending)>,
    /// Frames rendered so far.
    frame: u64,
    scratch: Vec<Frame>,
    stop_fade: usize,
    recorder: Option<Producer<f32>>,
    /// The slots whose level is copied to a bus. Their voices are rendered
    /// first, so the bus is full before anything that listens to it runs.
    buses: Vec<(Slot, Arc<Bus>)>,
    /// Keeps the mix within full scale without clipping it.
    limiter: Limiter,
}

impl Engine {
    pub fn new(
        commands: Consumer<Command>,
        garbage: Producer<Box<dyn Node>>,
        status: Arc<Status>,
        sample_rate: u32,
    ) -> Self {
        Self {
            commands,
            garbage,
            status,
            // Allocated up front, so the audio thread never has to grow them.
            voices: Vec::with_capacity(MAX_VOICES),
            pending: Vec::with_capacity(MAX_PENDING),
            frame: 0,
            scratch: vec![[0.0; 2]; MAX_BLOCK],
            stop_fade: (sample_rate as usize / 100).max(1), // 10 ms
            recorder: None,
            buses: Vec::with_capacity(MAX_BUSES),
            limiter: Limiter::new(
                1.0,
                latency(sample_rate),
                (MASTER_RELEASE_SECONDS * sample_rate as f64) as f32,
            ),
        }
    }

    /// Render one block (at most `MAX_BLOCK` frames) into `out`.
    pub fn process(&mut self, out: &mut [Frame]) {
        let mut received = 0;
        let len = out.len();
        while let Ok(cmd) = self.commands.pop() {
            received += 1;
            match cmd {
                Command::Play { node, at, slot } => {
                    let at = at.unwrap_or(0).max(self.frame);
                    self.schedule(at, Pending::Play(node, slot));
                }
                Command::StopSlot { slot, at } => {
                    // Voices sent ahead for this slot that would start after
                    // the stop never start. (What's sent for the slot after
                    // this command, the replacement, isn't touched.)
                    let mut i = 0;
                    while i < self.pending.len() {
                        match self.pending[i] {
                            (start, Pending::Play(_, s)) if s == slot && start >= at => {
                                if let (_, Pending::Play(node, _)) = self.pending.swap_remove(i) {
                                    self.retire(node);
                                }
                            }
                            _ => i += 1,
                        }
                    }
                    self.schedule(at, Pending::Stop(slot));
                }
                Command::StopAll => {
                    for v in &mut self.voices {
                        v.stopping.get_or_insert(self.stop_fade);
                    }
                    // Nothing that was waiting to start should start either.
                    while let Some((_, pending)) = self.pending.pop() {
                        if let Pending::Play(node, _) = pending {
                            self.retire(node);
                        }
                    }
                }
                // The session sends each slot once, and keeps a reference of its
                // own, so dropping one here (if it's full) never frees it here.
                Command::Bus { slot, bus } => {
                    if self.buses.len() < MAX_BUSES && self.buses.iter().all(|(s, _)| *s != slot) {
                        self.buses.push((slot, bus));
                    }
                }
                Command::StartRecording(producer) => self.recorder = Some(producer),
                // Dropping our end tells the writer thread to finish the file.
                Command::StopRecording => self.recorder = None,
            }
        }

        self.start_due(len);

        out.fill([0.0; 2]);
        for (_, bus) in &self.buses {
            bus.begin(self.frame, len);
        }
        // First the voices that feed a bus, then the rest (which may listen).
        for keyed in [true, false] {
            let mut i = 0;
            while i < self.voices.len() {
                let bus = self.buses.iter().find(|(s, _)| *s == self.voices[i].slot);
                if bus.is_some() != keyed {
                    i += 1;
                    continue;
                }
                let voice = &mut self.voices[i];
                let delay = std::mem::take(&mut voice.delay);
                let scratch = &mut self.scratch[..len - delay];
                let n = voice.node.process(scratch);
                let mut finished = n < len - delay;
                let start = voice.stopping;
                let mut played = n;
                for (j, (o, s)) in out[delay..].iter_mut().zip(&scratch[..n]).enumerate() {
                    let gain = match &mut voice.stopping {
                        None => 1.0,
                        Some(0) => {
                            finished = true;
                            played = j;
                            break;
                        }
                        Some(left) => {
                            *left -= 1;
                            *left as f32 / self.stop_fade as f32
                        }
                    };
                    o[0] += s[0] * gain;
                    o[1] += s[1] * gain;
                }
                if let Some((_, bus)) = bus {
                    let fade = self.stop_fade as f32;
                    bus.add(delay, &scratch[..played], |j| match start {
                        None => 1.0,
                        Some(left) => left.saturating_sub(j + 1) as f32 / fade,
                    });
                }
                if finished {
                    let voice = self.voices.swap_remove(i);
                    self.retire(voice.node);
                } else {
                    i += 1;
                }
            }
        }

        // Limit here rather than in the device callback, so recordings get
        // exactly what's heard. (The clamp only catches rounding.)
        let mut clipped = 0;
        for frame in out.iter_mut() {
            if frame[0].abs() > 1.0 || frame[1].abs() > 1.0 {
                clipped += 1;
            }
            let [l, r] = self.limiter.step(*frame);
            *frame = [l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0)];
        }
        if clipped > 0 {
            self.status.clipped.fetch_add(clipped, Ordering::Relaxed);
        }
        if let Some(recorder) = &mut self.recorder {
            let mut dropped = 0;
            for &[l, r] in out.iter() {
                if recorder.slots() < 2 {
                    dropped += 2;
                    continue;
                }
                let _ = recorder.push(l);
                let _ = recorder.push(r);
            }
            if dropped > 0 {
                self.status
                    .recording_dropped
                    .fetch_add(dropped, Ordering::Relaxed);
            }
        }

        self.frame += len as u64;
        self.status.frames.store(self.frame, Ordering::Relaxed);
        // Store voices before commands_received, so a reader that sees the latest
        // command count also sees a voice count that includes it.
        self.status
            .voices
            .store(self.voices.len() + self.pending.len(), Ordering::Relaxed);
        self.status
            .commands_received
            .fetch_add(received, Ordering::Release);
    }

    fn schedule(&mut self, at: u64, pending: Pending) {
        if self.pending.len() < MAX_PENDING {
            self.pending.push((at, pending));
        } else if let Pending::Play(node, _) = pending {
            self.retire(node);
        }
    }

    /// Carry out the pending commands that fall in the next `len` frames: stops
    /// first (so a slot's new voice isn't stopped along with the old ones),
    /// then starts.
    fn start_due(&mut self, len: usize) {
        let end = self.frame + len as u64;
        let mut i = 0;
        while i < self.pending.len() {
            match self.pending[i] {
                (at, Pending::Stop(slot)) if at < end => {
                    for v in self.voices.iter_mut().filter(|v| v.slot == slot) {
                        v.stopping.get_or_insert(self.stop_fade);
                    }
                    self.pending.swap_remove(i);
                }
                _ => i += 1,
            }
        }
        let mut i = 0;
        while i < self.pending.len() {
            let at = self.pending[i].0;
            if at >= end || !matches!(self.pending[i].1, Pending::Play(..)) {
                i += 1;
                continue;
            }
            let Pending::Play(node, slot) = self.pending.swap_remove(i).1 else {
                unreachable!()
            };
            if self.voices.len() < MAX_VOICES {
                self.voices.push(Voice {
                    node,
                    stopping: None,
                    slot,
                    delay: at.saturating_sub(self.frame) as usize,
                });
            } else {
                self.retire(node);
            }
        }
    }

    fn retire(&mut self, node: Box<dyn Node>) {
        // If the garbage queue is full, dropping here is the lesser evil.
        let _ = self.garbage.push(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays 0.5 forever (two of them add up to 1, the most the engine lets
    /// through unlimited).
    struct Ones;
    impl Node for Ones {
        fn process(&mut self, out: &mut [Frame]) -> usize {
            out.fill([0.5, 0.5]);
            out.len()
        }
        fn reset(&mut self) {}
    }

    fn engine() -> (Engine, Producer<Command>) {
        let (commands, commands_rx) = rtrb::RingBuffer::new(16);
        let (garbage, _) = rtrb::RingBuffer::new(16);
        let engine = Engine::new(commands_rx, garbage, Arc::new(Status::default()), 1000);
        (engine, commands)
    }

    /// The master limiter's delay at 1 kHz: 5 ms.
    const LATE: usize = 5;

    fn block(engine: &mut Engine) -> Vec<f32> {
        let mut out = vec![[0.0; 2]; 100];
        engine.process(&mut out);
        out.iter().map(|f| f[0]).collect()
    }

    #[test]
    fn timed_plays_start_on_their_frame() {
        let (mut engine, mut commands) = engine();
        let play = |at| Command::Play {
            node: Box::new(Ones),
            at: Some(at),
            slot: 0,
        };
        let _ = commands.push(play(130));
        assert_eq!(block(&mut engine), vec![0.0; 100]);
        let second = block(&mut engine);
        assert_eq!(second[29 + LATE], 0.0);
        assert_eq!(second[30 + LATE], 0.5);
        assert_eq!(engine.status.frames.load(Ordering::Relaxed), 200);
        // In the past: right away.
        let _ = commands.push(play(0));
        assert_eq!(block(&mut engine)[LATE], 1.0);
    }

    #[test]
    fn stopping_a_slot_cancels_what_was_sent_ahead() {
        let (mut engine, mut commands) = engine();
        let play = |at| Command::Play {
            node: Box::new(Ones),
            at: Some(at),
            slot: 1,
        };
        let _ = commands.push(play(150));
        let _ = commands.push(play(50));
        let _ = commands.push(Command::StopSlot { slot: 1, at: 100 });
        let _ = commands.push(play(100));
        let first = block(&mut engine);
        assert_eq!(first[60], 0.5, "starts before the stop");
        let second = block(&mut engine);
        // The one at 50 fades out at 100, the replacement starts at 100, and
        // the one at 150 never does.
        assert_eq!(second[60], 0.5);
    }

    #[test]
    fn stopping_a_slot_leaves_the_others() {
        let (mut engine, mut commands) = engine();
        for slot in [1, 2] {
            let _ = commands.push(Command::Play {
                node: Box::new(Ones),
                at: None,
                slot,
            });
        }
        assert_eq!(block(&mut engine)[LATE], 1.0);
        let _ = commands.push(Command::StopSlot { slot: 1, at: 150 });
        // A new voice in the same slot, at the same time, isn't stopped.
        let _ = commands.push(Command::Play {
            node: Box::new(Ones),
            at: Some(150),
            slot: 1,
        });
        block(&mut engine);
        // The stop fade (10 ms at 1 kHz) is over well before the third block.
        assert_eq!(block(&mut engine)[50], 1.0);
        assert_eq!(engine.voices.len(), 2);
    }

    #[test]
    fn a_mix_over_full_scale_is_limited_not_clipped() {
        let (mut engine, mut commands) = engine();
        let play = || Command::Play {
            node: Box::new(Ones),
            at: None,
            slot: 0,
        };
        for _ in 0..2 {
            let _ = commands.push(play());
        }
        block(&mut engine);
        assert_eq!(block(&mut engine)[50], 1.0, "full scale passes untouched");
        assert_eq!(engine.status.clipped.load(Ordering::Relaxed), 0);
        // A third makes 1.5: turned down to full scale, smoothly, and counted.
        let _ = commands.push(play());
        let out: Vec<f32> = (0..3).flat_map(|_| block(&mut engine)).collect();
        assert!(out.iter().all(|s| *s <= 1.0));
        // The gain comes down in the frames before the louder part arrives.
        assert_eq!(
            &out[..5],
            [
                14.0 / 15.0,
                13.0 / 15.0,
                12.0 / 15.0,
                11.0 / 15.0,
                10.0 / 15.0
            ]
        );
        assert!((out[250] - 1.0).abs() < 1e-6);
        assert_eq!(engine.status.clipped.load(Ordering::Relaxed), 300);
    }
}
