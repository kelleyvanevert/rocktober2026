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
