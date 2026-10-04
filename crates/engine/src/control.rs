//! Control signals: one value per frame, mono, nominally 0..1. They aren't
//! heard themselves; they shape the parameters of audio nodes (a gain now, a
//! wavetable position later).
//!
//! Like audio nodes they're pull-based and run on the audio thread, so the same
//! rules apply: `process` never allocates, locks, prints or touches files.
//! They're computed per frame, like audio, rather than at a lower control rate:
//! mono and cheap, and it means there's never any zipper noise.

use std::sync::Arc;

use crate::engine::MAX_BLOCK;
use crate::envelope::Envelope;
use crate::modulation::Modulation;
use crate::noise::random_at;
use crate::pattern::{Pattern, Step};

pub trait ControlNode: Send {
    /// Write up to `out.len()` values. Returns how many were written; fewer
    /// means the signal has finished (like an envelope whose release is over),
    /// which also ends whatever it controls.
    fn process(&mut self, out: &mut [f32]) -> usize;

    /// Start over.
    fn reset(&mut self);
}

/// The same value forever.
pub struct Constant(pub f32);

impl ControlNode for Constant {
    fn process(&mut self, out: &mut [f32]) -> usize {
        out.fill(self.0);
        out.len()
    }

    fn reset(&mut self) {}
}

/// Plays a modulation's curve `times` times, then holds its last value. It
/// never finishes.
pub struct ModulationPlayer {
    data: Arc<Modulation>,
    /// The length of one pass, in frames.
    length: f64,
    /// Frames into the current pass.
    pos: f64,
    times: usize,
    passes: usize,
}

impl ModulationPlayer {
    /// Starting `start` frames in.
    pub fn new(data: Arc<Modulation>, times: usize, start: usize, sample_rate: u32) -> Self {
        let length = (data.length * sample_rate as f64).max(1.0);
        let passes = (start as f64 / length).floor() as usize;
        let (passes, pos) = if passes >= times {
            (times, length)
        } else {
            (passes, start as f64 - passes as f64 * length)
        };
        Self {
            data,
            length,
            pos,
            times,
            passes,
        }
    }
}

impl ControlNode for ModulationPlayer {
    fn process(&mut self, out: &mut [f32]) -> usize {
        for slot in out.iter_mut() {
            if self.pos >= self.length && self.passes < self.times {
                self.passes += 1;
                if self.passes < self.times {
                    self.pos -= self.length;
                }
            }
            if self.passes >= self.times {
                *slot = self.data.value_at(1.0) as f32;
                continue;
            }
            *slot = self.data.value_at(self.pos / self.length) as f32;
            self.pos += 1.0;
        }
        out.len()
    }

    fn reset(&mut self) {
        self.pos = 0.0;
        self.passes = 0;
    }
}

/// Plays an envelope from note-on. With a gate, it's released after that many
/// frames and finishes when the release is over; without one, it sustains
/// forever.
pub struct EnvelopePlayer {
    env: Arc<Envelope>,
    sample_rate: f64,
    gate: Option<usize>,
    /// Frames since note-on.
    t: usize,
    /// The last level, which is where the release starts from.
    level: f64,
    released_from: Option<f64>,
}

impl EnvelopePlayer {
    pub fn new(env: Arc<Envelope>, gate: Option<usize>, sample_rate: u32) -> Self {
        Self {
            env,
            sample_rate: sample_rate as f64,
            gate,
            t: 0,
            level: 0.0,
            released_from: None,
        }
    }
}

impl ControlNode for EnvelopePlayer {
    fn process(&mut self, out: &mut [f32]) -> usize {
        for (i, slot) in out.iter_mut().enumerate() {
            let level = match self.gate {
                Some(gate) if self.t >= gate => {
                    let from = *self.released_from.get_or_insert(self.level);
                    let seconds = (self.t - gate) as f64 / self.sample_rate;
                    if seconds >= self.env.release.time {
                        return i;
                    }
                    self.env.level_released(from, seconds)
                }
                _ => self.env.level_held(self.t as f64 / self.sample_rate),
            };
            self.level = level;
            *slot = level as f32;
            self.t += 1;
        }
        out.len()
    }

    fn reset(&mut self) {
        self.t = 0;
        self.level = 0.0;
        self.released_from = None;
    }
}

/// Two signals combined value by value (multiplied, added). Finishes as soon
/// as either does.
pub struct Combine {
    a: Box<dyn ControlNode>,
    b: Box<dyn ControlNode>,
    op: fn(f32, f32) -> f32,
    buf: Vec<f32>,
}

impl Combine {
    pub fn new(a: Box<dyn ControlNode>, b: Box<dyn ControlNode>, op: fn(f32, f32) -> f32) -> Self {
        Self {
            a,
            b,
            op,
            buf: vec![0.0; MAX_BLOCK],
        }
    }
}

impl ControlNode for Combine {
    fn process(&mut self, out: &mut [f32]) -> usize {
        let mut done = 0;
        for chunk in out.chunks_mut(self.buf.len()) {
            let n = self.a.process(chunk);
            let m = self.b.process(&mut self.buf[..n]);
            for (x, y) in chunk.iter_mut().zip(&self.buf[..m]) {
                *x = (self.op)(*x, *y);
            }
            done += m;
            if m < chunk.len() {
                break;
            }
        }
        done
    }

    fn reset(&mut self) {
        self.a.reset();
        self.b.reset();
    }
}

/// A new random value (0..1) every `period` frames, held in between (sample
/// and hold). The values are a function of the period's number, so a voice
/// can pick the stream up anywhere, like a modulation. It never finishes.
pub struct RandomPlayer {
    seed: u64,
    period: f64,
    start: f64,
    /// Frames from the start of period 0.
    pos: f64,
}

impl RandomPlayer {
    /// Starting `start` frames in.
    pub fn new(seed: u64, period: f64, start: f64) -> Self {
        Self {
            seed,
            period: period.max(1.0),
            start,
            pos: start,
        }
    }
}

impl ControlNode for RandomPlayer {
    fn process(&mut self, out: &mut [f32]) -> usize {
        for slot in out.iter_mut() {
            let k = (self.pos / self.period + 1e-9).floor() as i64;
            *slot = random_at(self.seed, k) as f32;
            self.pos += 1.0;
        }
        out.len()
    }

    fn reset(&mut self) {
        self.pos = self.start;
    }
}

/// `x` (0..1) mapped onto `lo..hi`. With `whole`, onto the whole numbers from
/// `lo` to `hi`, both included and (for a uniform `x`) all equally likely.
/// Finishes when any of the three does.
pub struct Range {
    x: Box<dyn ControlNode>,
    lo: Box<dyn ControlNode>,
    hi: Box<dyn ControlNode>,
    whole: bool,
    bufs: [Vec<f32>; 2],
}

impl Range {
    pub fn new(
        x: Box<dyn ControlNode>,
        lo: Box<dyn ControlNode>,
        hi: Box<dyn ControlNode>,
        whole: bool,
    ) -> Self {
        Self {
            x,
            lo,
            hi,
            whole,
            bufs: [vec![0.0; MAX_BLOCK], vec![0.0; MAX_BLOCK]],
        }
    }
}

impl ControlNode for Range {
    fn process(&mut self, out: &mut [f32]) -> usize {
        let mut done = 0;
        for chunk in out.chunks_mut(MAX_BLOCK) {
            let n = self.x.process(chunk);
            let n = self.lo.process(&mut self.bufs[0][..n]);
            let n = self.hi.process(&mut self.bufs[1][..n]);
            for (i, x) in chunk[..n].iter_mut().enumerate() {
                let (lo, hi) = (self.bufs[0][i], self.bufs[1][i]);
                *x = if self.whole {
                    let (lo, hi) = (lo.round(), hi.round());
                    let steps = (hi - lo).abs();
                    let step = (x.clamp(0.0, 1.0) * (steps + 1.0)).floor().min(steps);
                    lo + step * (hi - lo).signum()
                } else {
                    lo + *x * (hi - lo)
                };
            }
            done += n;
            if n < chunk.len() {
                break;
            }
        }
        done
    }

    fn reset(&mut self) {
        self.x.reset();
        self.lo.reset();
        self.hi.reset();
    }
}

/// A function applied to every value.
pub struct Map {
    x: Box<dyn ControlNode>,
    f: fn(f32) -> f32,
}

impl Map {
    pub fn new(x: Box<dyn ControlNode>, f: fn(f32) -> f32) -> Self {
        Self { x, f }
    }
}

impl ControlNode for Map {
    fn process(&mut self, out: &mut [f32]) -> usize {
        let n = self.x.process(out);
        for x in &mut out[..n] {
            *x = (self.f)(*x);
        }
        n
    }

    fn reset(&mut self) {
        self.x.reset();
    }
}

/// Scales by name: each one's steps, in semitones above the root, within an
/// octave.
pub const SCALES: &[(&str, &[u8])] = &[
    ("major", &[0, 2, 4, 5, 7, 9, 11]),
    ("minor", &[0, 2, 3, 5, 7, 8, 10]),
    ("harmonic minor", &[0, 2, 3, 5, 7, 8, 11]),
    ("melodic minor", &[0, 2, 3, 5, 7, 9, 11]),
    ("dorian", &[0, 2, 3, 5, 7, 9, 10]),
    ("phrygian", &[0, 1, 3, 5, 7, 8, 10]),
    ("lydian", &[0, 2, 4, 6, 7, 9, 11]),
    ("mixolydian", &[0, 2, 4, 5, 7, 9, 10]),
    ("locrian", &[0, 1, 3, 5, 6, 8, 10]),
    ("major pentatonic", &[0, 2, 4, 7, 9]),
    ("minor pentatonic", &[0, 3, 5, 7, 10]),
    ("blues", &[0, 3, 5, 6, 7, 10]),
    ("chromatic", &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]),
];

pub fn scale(name: &str) -> Option<&'static [u8]> {
    SCALES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, steps)| *steps)
}

/// A scale degree (rounded to a whole one) as semitones above the root: 0 is
/// the root, 1 the next note up, `steps.len()` the root an octave up, -1 the
/// note below the root.
pub fn degree_to_semitones(steps: &[u8], degree: f32) -> f32 {
    let degree = degree.round() as i64;
    let len = steps.len() as i64;
    (degree.div_euclid(len) * 12 + steps[degree.rem_euclid(len) as usize] as i64) as f32
}

/// Scale degrees to semitones above the root (see `degree_to_semitones`).
pub struct ScaleDegree {
    x: Box<dyn ControlNode>,
    steps: &'static [u8],
}

impl ScaleDegree {
    pub fn new(x: Box<dyn ControlNode>, steps: &'static [u8]) -> Self {
        Self { x, steps }
    }
}

impl ControlNode for ScaleDegree {
    fn process(&mut self, out: &mut [f32]) -> usize {
        let n = self.x.process(out);
        for x in &mut out[..n] {
            *x = degree_to_semitones(self.steps, *x);
        }
        n
    }

    fn reset(&mut self) {
        self.x.reset();
    }
}

/// The pitch of a gliding phrase: a pattern's notes from step `start` on,
/// each sliding from where the last one was to its own pitch over `glide`
/// frames, in a straight line (in notes, so evenly in pitch). Hits without a
/// note (`x`) keep the pitch where it is. It never finishes; the phrase's
/// voice is ended by its gate.
pub struct NotePath {
    pattern: Arc<Pattern>,
    start: usize,
    step_frames: f64,
    glide: f64,
    /// Frames since the start.
    t: f64,
    /// The step that's playing.
    step: usize,
    from: f32,
    to: f32,
    /// Frames since the last note.
    since: f64,
}

impl NotePath {
    pub fn new(pattern: Arc<Pattern>, start: usize, step_frames: f64, glide: f64) -> Self {
        let mut path = Self {
            pattern,
            start,
            step_frames: step_frames.max(1.0),
            glide,
            t: 0.0,
            step: start,
            from: 0.0,
            to: 0.0,
            since: 0.0,
        };
        path.reset();
        path
    }

    fn value(&self) -> f32 {
        if self.since >= self.glide {
            self.to
        } else {
            self.from + (self.to - self.from) * (self.since / self.glide) as f32
        }
    }

    fn note_at(&self, step: usize) -> Option<f32> {
        let steps = &self.pattern.steps;
        match steps[step % steps.len()] {
            Step::Hit { note: Some(n), .. } => Some(n as f32),
            _ => None,
        }
    }
}

impl ControlNode for NotePath {
    fn process(&mut self, out: &mut [f32]) -> usize {
        for slot in out.iter_mut() {
            let step = self.start + (self.t / self.step_frames + 1e-9).floor() as usize;
            while self.step < step {
                self.step += 1;
                if let Some(note) = self.note_at(self.step) {
                    self.from = self.value();
                    self.to = note;
                    self.since = 0.0;
                }
            }
            *slot = self.value();
            self.t += 1.0;
            self.since += 1.0;
        }
        out.len()
    }

    fn reset(&mut self) {
        // Starts on its first note (or, if it starts on an `x`, on the first
        // note it gets to: the pitch is at rest until then).
        let len = self.pattern.steps.len();
        let first = (self.start..self.start + len)
            .find_map(|s| self.note_at(s))
            .unwrap_or(60.0);
        self.t = 0.0;
        self.step = self.start;
        self.from = first;
        self.to = first;
        self.since = self.glide;
    }
}

/// A numeric parameter of an audio node: a constant, or a control signal
/// computed a block at a time.
pub enum Param {
    Const(f32),
    Signal {
        node: Box<dyn ControlNode>,
        buf: Vec<f32>,
    },
}

impl Param {
    pub fn signal(node: Box<dyn ControlNode>) -> Self {
        Param::Signal {
            node,
            buf: vec![0.0; MAX_BLOCK],
        }
    }

    /// The most frames `next` can compute at once.
    pub fn block(&self) -> usize {
        match self {
            Param::Const(_) => usize::MAX,
            Param::Signal { buf, .. } => buf.len(),
        }
    }

    /// Compute the next `n` values (at most `block()`), to be read with `get`.
    /// Returns how many there are; fewer means the signal finished.
    pub fn next(&mut self, n: usize) -> usize {
        match self {
            Param::Const(_) => n,
            Param::Signal { node, buf } => node.process(&mut buf[..n]),
        }
    }

    /// The `i`th value computed by the last `next`.
    #[inline]
    pub fn get(&self, i: usize) -> f32 {
        match self {
            Param::Const(v) => *v,
            Param::Signal { buf, .. } => buf[i],
        }
    }

    /// Advance without needing the values. Returns how many frames were
    /// skipped; fewer means the signal finished.
    pub fn skip(&mut self, frames: usize) -> usize {
        let mut skipped = 0;
        while skipped < frames {
            let len = (frames - skipped).min(self.block());
            let n = self.next(len);
            skipped += n;
            if n < len {
                break;
            }
        }
        skipped
    }

    pub fn reset(&mut self) {
        if let Param::Signal { node, .. } = self {
            node.reset();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scale_degrees_wrap_into_octaves() {
        let minor = scale("minor").unwrap();
        let semis: Vec<f32> = (-2..=8)
            .map(|d| degree_to_semitones(minor, d as f32))
            .collect();
        assert_eq!(semis, [-4., -2., 0., 2., 3., 5., 7., 8., 10., 12., 14.]);
        assert_eq!(
            degree_to_semitones(minor, 2.4),
            3.0,
            "rounds to a whole degree"
        );
    }
    use crate::envelope::Stage;
    use crate::modulation::Point;

    /// Render a control in awkward block sizes, stopping after `max` values.
    fn render(node: &mut dyn ControlNode, max: usize) -> Vec<f32> {
        let mut all = Vec::new();
        let mut buf = [0.0; 7];
        while all.len() < max {
            let n = node.process(&mut buf);
            all.extend_from_slice(&buf[..n]);
            if n < buf.len() {
                break;
            }
        }
        all.truncate(max);
        all
    }

    fn ramp() -> Arc<Modulation> {
        Arc::new(Modulation {
            length: 4.0,
            points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
        })
    }

    #[test]
    fn modulation_plays_then_holds() {
        // 4 seconds at 1 Hz: 4 frames per pass.
        let out = render(&mut ModulationPlayer::new(ramp(), 1, 0, 1), 7);
        assert_eq!(out, [0.0, 0.25, 0.5, 0.75, 1.0, 1.0, 1.0]);
        let out = render(&mut ModulationPlayer::new(ramp(), 2, 0, 1), 10);
        assert_eq!(out, [0.0, 0.25, 0.5, 0.75, 0.0, 0.25, 0.5, 0.75, 1.0, 1.0]);
        // Picked up partway: two frames into the second pass.
        let out = render(&mut ModulationPlayer::new(ramp(), 2, 6, 1), 4);
        assert_eq!(out, [0.5, 0.75, 1.0, 1.0]);
        let out = render(&mut ModulationPlayer::new(ramp(), 2, 100, 1), 2);
        assert_eq!(out, [1.0, 1.0]);
    }

    #[test]
    fn envelope_releases_at_the_gate_and_finishes() {
        let env = Arc::new(Envelope {
            attack: Stage {
                time: 4.0,
                curve: 0.0,
            },
            decay: Stage {
                time: 4.0,
                curve: 0.0,
            },
            sustain: 0.5,
            release: Stage {
                time: 2.0,
                curve: 0.0,
            },
        });
        // Held for two frames, so released a quarter of the way up the attack:
        // back down from there in 2 frames.
        let out = render(&mut EnvelopePlayer::new(env.clone(), Some(2), 1), 100);
        assert_eq!(out, [0.0, 0.25, 0.25, 0.125]);
        // No gate: sustains forever.
        let out = render(&mut EnvelopePlayer::new(env, None, 1), 100);
        assert_eq!(out.len(), 100);
        assert_eq!(
            &out[..10],
            [0.0, 0.25, 0.5, 0.75, 1.0, 0.875, 0.75, 0.625, 0.5, 0.5]
        );
    }

    #[test]
    fn combined_signals_end_with_the_first() {
        let env = Arc::new(Envelope {
            release: Stage {
                time: 0.0,
                curve: 0.0,
            },
            ..Envelope::default()
        });
        let gated = Box::new(EnvelopePlayer::new(env, Some(5000), 48_000));
        let mut mul = Combine::new(Box::new(Constant(0.5)), gated, |a, b| a * b);
        let mut buf = vec![0.0; 3000];
        assert_eq!(mul.process(&mut buf), 3000);
        assert_eq!(mul.process(&mut buf), 2000);
        mul.reset();
        assert_eq!(mul.process(&mut buf), 3000);
    }

    #[test]
    fn random_holds_each_value_for_a_period() {
        let out = render(&mut RandomPlayer::new(5, 3.0, 0.0), 9);
        assert!(out[..3].iter().all(|v| *v == out[0]));
        assert!(out[3..6].iter().all(|v| *v == out[3]));
        assert_ne!(out[0], out[3]);
        // Picked up partway, it's the same stream.
        let later = render(&mut RandomPlayer::new(5, 3.0, 4.0), 5);
        assert_eq!(later, out[4..9]);
    }

    #[test]
    fn ranges_scale_or_count_whole_numbers() {
        let ramp = |n: usize| {
            let m = Arc::new(Modulation {
                length: n as f64,
                points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
            });
            Box::new(ModulationPlayer::new(m, 1, 0, 1))
        };
        let c = |v| Box::new(Constant(v));
        let out = render(&mut Range::new(ramp(4), c(10.0), c(20.0), false), 5);
        assert_eq!(out, [10.0, 12.5, 15.0, 17.5, 20.0]);
        // 1 to 3 inclusive: thirds of the ramp each, and 3 at the very top.
        let out = render(&mut Range::new(ramp(6), c(1.0), c(3.0), true), 7);
        assert_eq!(out, [1.0, 1.0, 2.0, 2.0, 3.0, 3.0, 3.0]);
        let out = render(&mut Map::new(c(2.6), f32::round), 2);
        assert_eq!(out, [3.0, 3.0]);
    }

    #[test]
    fn note_paths_glide_between_notes() {
        let pattern = Arc::new(Pattern::parse("c4 _ e4 x", 1.0).unwrap());
        // Four frames a step, two frames of glide.
        let out = render(&mut NotePath::new(pattern.clone(), 0, 4.0, 2.0), 16);
        assert_eq!(
            out,
            [
                60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 60.0, 62.0, 64.0, 64.0, 64.0, 64.0,
                64.0, 64.0
            ]
        );
        // From the third step on: starts right on e4.
        let out = render(&mut NotePath::new(pattern, 2, 4.0, 2.0), 2);
        assert_eq!(out, [64.0, 64.0]);
    }

    #[test]
    fn params_compute_blocks() {
        let mut p = Param::signal(Box::new(ModulationPlayer::new(ramp(), 1, 0, 1)));
        assert_eq!(p.next(3), 3);
        assert_eq!((p.get(0), p.get(2)), (0.0, 0.5));
        assert_eq!(p.skip(5000), 5000);
        let mut c = Param::Const(0.3);
        assert_eq!(c.next(10), 10);
        assert_eq!(c.get(9), 0.3);
    }
}
