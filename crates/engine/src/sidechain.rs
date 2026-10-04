//! Sidechaining: one sound's level pushing another one down (`pad.duck("kick")`).
//!
//! Voices are independent node trees, so a ducker can't reach the kick's nodes.
//! Instead the engine copies what the voices in a slot play into that slot's
//! `Bus` (a level per frame), and does so before it renders any other voice,
//! so a ducker reads the kick of the very same block.
//!
//! A ducker doesn't know where in the block its first frame falls (nodes
//! don't know the time; only how many frames they're asked for). Once
//! running it keeps count, so it's exact; on its first call it assumes it's
//! asked for the end of the block, which is where a voice that starts
//! mid-block, or a part of a `seq` or `delay`, is. The worst case is a duck
//! that's early or late by part of a block (at most ~20 ms), on its first block.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::control::Param;
use crate::engine::MAX_BLOCK;
use crate::nodes::{Frame, Node};

/// The level of a slot's voices in the block being rendered. Written and
/// read only on the audio thread; atomics (relaxed, so they cost nothing)
/// only because the bus is shared through an `Arc`.
pub struct Bus {
    /// The frame the block starts at.
    start: AtomicU64,
    len: AtomicUsize,
    level: Box<[AtomicU32]>,
}

impl Default for Bus {
    fn default() -> Self {
        Self {
            start: AtomicU64::new(0),
            len: AtomicUsize::new(0),
            level: (0..MAX_BLOCK).map(|_| AtomicU32::new(0)).collect(),
        }
    }
}

impl Bus {
    /// A new block, silent so far.
    pub fn begin(&self, start: u64, len: usize) {
        self.start.store(start, Ordering::Relaxed);
        self.len.store(len, Ordering::Relaxed);
        for l in &self.level[..len] {
            l.store(0, Ordering::Relaxed);
        }
    }

    /// Add a voice's output, which starts `offset` frames into the block.
    pub fn add(&self, offset: usize, frames: &[Frame], gain: impl Fn(usize) -> f32) {
        for (i, f) in frames.iter().enumerate() {
            let slot = &self.level[offset + i];
            let level = f32::from_bits(slot.load(Ordering::Relaxed));
            let add = f[0].abs().max(f[1].abs()) * gain(i);
            slot.store((level + add).to_bits(), Ordering::Relaxed);
        }
    }

    fn block(&self) -> (u64, usize) {
        (
            self.start.load(Ordering::Relaxed),
            self.len.load(Ordering::Relaxed),
        )
    }

    fn level(&self, i: usize) -> f32 {
        f32::from_bits(self.level[i].load(Ordering::Relaxed))
    }
}

/// Above this (-40 dBFS), the key counts as sounding.
const THRESHOLD: f32 = 0.01;
/// How long the key's level is held through its zero crossings.
const HOLD_SECONDS: f32 = 0.01;
/// How fast the duck goes down.
const ATTACK_SECONDS: f32 = 0.002;

/// Turns its child down while the key (a bus) sounds: by `amount` (0..1, 1 is
/// all the way), quickly, then back up over `release` once the key is quiet.
/// How loud the key is doesn't matter, only whether it sounds: a ghost note
/// ducks as deep as an accent, which keeps it predictable, like a volume
/// shaper triggered by the kick.
pub struct Duck {
    child: Box<dyn Node>,
    bus: Arc<Bus>,
    amount: Param,
    /// The absolute frame of the next output, once known.
    next: Option<u64>,
    peak: f32,
    hold: f32,
    duck: f32,
    attack: f32,
    release: f32,
}

impl Duck {
    pub fn new(
        child: Box<dyn Node>,
        bus: Arc<Bus>,
        amount: Param,
        release: f32,
        sample_rate: u32,
    ) -> Self {
        let rate = sample_rate as f32;
        // One-pole coefficients: after `seconds`, 99% of the way there.
        let coef = |seconds: f32| 1.0 - (-4.6 / (seconds * rate).max(1.0)).exp();
        Self {
            child,
            bus,
            amount,
            next: None,
            peak: 0.0,
            hold: 1.0 - coef(HOLD_SECONDS),
            duck: 0.0,
            attack: coef(ATTACK_SECONDS),
            release: coef(release),
        }
    }
}

impl Node for Duck {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let (start, len) = self.bus.block();
        let mut done = 0;
        while done < out.len() {
            let chunk_len = (out.len() - done).min(self.amount.block());
            let chunk = &mut out[done..done + chunk_len];
            let n = self.child.process(chunk);
            let n = self.amount.next(n);
            // Where in the block this is: carry on from the last call, or
            // (first call, or the engine skipped ahead) the end of the block.
            let first = match self.next {
                Some(f) if f >= start && f <= start + len as u64 => f,
                _ => (start + len as u64).saturating_sub(n as u64).max(start),
            };
            for (i, frame) in chunk[..n].iter_mut().enumerate() {
                let at = (first - start) as usize + i;
                let key = if at < len { self.bus.level(at) } else { 0.0 };
                self.peak = key.max(self.peak * self.hold);
                let (target, coef) = if self.peak > THRESHOLD {
                    (1.0, self.attack)
                } else {
                    (0.0, self.release)
                };
                self.duck += (target - self.duck) * coef;
                let gain = 1.0 - self.amount.get(i).clamp(0.0, 1.0) * self.duck;
                frame[0] *= gain;
                frame[1] *= gain;
            }
            self.next = Some(first + n as u64);
            done += n;
            if n < chunk_len {
                break;
            }
        }
        done
    }

    fn reset(&mut self) {
        self.child.reset();
        self.amount.reset();
        self.next = None;
        self.peak = 0.0;
        self.duck = 0.0;
    }

    fn skip(&mut self, frames: usize) -> usize {
        let n = self.child.skip(frames);
        self.next = self.next.map(|f| f + n as u64);
        self.amount.skip(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::{SampleData, Sampler};

    fn ones(n: usize) -> Box<dyn Node> {
        Box::new(Sampler::new(
            Arc::new(SampleData {
                frames: vec![[1.0, 1.0]; n],
                sample_rate: 1000,
            }),
            1000,
        ))
    }

    #[test]
    fn ducks_while_the_key_sounds_then_recovers() {
        let bus = Arc::new(Bus::default());
        // 1 kHz, so frames are milliseconds: release over 100 ms.
        let mut duck = Duck::new(ones(10_000), bus.clone(), Param::Const(0.8), 0.1, 1000);
        let mut out = vec![[0.0; 2]; 100];
        let mut gains = Vec::new();
        for block in 0..10u64 {
            bus.begin(block * 100, 100);
            if block == 2 {
                // The key sounds for 50 ms, in the middle of the third block.
                let key = vec![[0.5, 0.5]; 50];
                bus.add(25, &key, |_| 1.0);
            }
            duck.process(&mut out);
            gains.extend(out.iter().map(|f| f[0]));
        }
        assert_eq!(gains[224], 1.0, "untouched before the key");
        assert!(
            (gains[240] - 0.2).abs() < 0.01,
            "down by 0.8: {}",
            gains[240]
        );
        assert!(
            gains[282] < 0.3,
            "still down just after (hold) {}",
            gains[282]
        );
        assert!(
            gains[400] > 0.95,
            "back up after the release {}",
            gains[400]
        );
    }

    #[test]
    fn a_voice_starting_mid_block_lines_up_with_the_end() {
        let bus = Arc::new(Bus::default());
        let mut duck = Duck::new(ones(1000), bus.clone(), Param::Const(1.0), 0.1, 1000);
        bus.begin(0, 100);
        bus.add(60, &[[1.0, 1.0]; 40], |_| 1.0);
        // As a voice starting 50 frames in: asked for the last 50.
        let mut out = vec![[0.0; 2]; 50];
        duck.process(&mut out);
        assert_eq!(out[9][0], 1.0);
        assert!(out[15][0] < 0.1);
    }
}
