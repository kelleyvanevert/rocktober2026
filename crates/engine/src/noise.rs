//! Noise in a few colors, and the random numbers behind it (and behind
//! `random`, see `control`).
//!
//! The colors differ in how their energy is spread over the spectrum: white is
//! flat, pink falls 3 dB per octave (equal energy per octave, so it sounds
//! even), brown 6 dB (a rumble), blue rises 3 dB and violet 6 dB (hiss).

use std::sync::atomic::{AtomicU64, Ordering};

use crate::nodes::{Frame, Node};

/// A seed for a new random stream. Seeds are handed out in order, so a
/// program renders the same every time it's run from scratch.
pub fn next_seed() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0x2545_f491_4f6c_dd1d);
    mix(NEXT.fetch_add(1, Ordering::Relaxed))
}

/// A well-mixed 64-bit hash (SplitMix64's finalizer).
pub fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The `k`th random number (0..1) of the stream `seed`, without having to
/// compute the ones before it.
pub fn random_at(seed: u64, k: i64) -> f64 {
    (mix(seed ^ mix(k as u64)) >> 11) as f64 / (1u64 << 53) as f64
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    White,
    Pink,
    Brown,
    Blue,
    Violet,
}

impl Color {
    pub const NAMES: [&str; 5] = ["white", "pink", "brown", "blue", "violet"];

    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "white" => Color::White,
            "pink" => Color::Pink,
            "brown" => Color::Brown,
            "blue" => Color::Blue,
            "violet" => Color::Violet,
            _ => return None,
        })
    }
}

/// Plays noise forever. Mono, like the wavetables: `spread` and `pan` place it.
pub struct Noise {
    color: Color,
    seed: u64,
    state: u64,
    /// Paul Kellet's pink filter state.
    pink: [f32; 7],
    /// The integrator (brown), or the last value (blue, violet: differences).
    last: f32,
}

impl Noise {
    pub fn new(color: Color, seed: u64) -> Self {
        let mut noise = Self {
            color,
            seed,
            state: 0,
            pink: [0.0; 7],
            last: 0.0,
        };
        noise.reset();
        noise
    }

    /// Uniform white noise, -1..1 (xorshift64*).
    #[inline]
    fn white(&mut self) -> f32 {
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        let x = self.state.wrapping_mul(0x2545_f491_4f6c_dd1d);
        (x >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    }

    /// Pink noise: white through a bank of one-pole filters that together
    /// fall 3 dB per octave (Paul Kellet's "refined" method).
    #[inline]
    fn pinked(&mut self, w: f32) -> f32 {
        let b = &mut self.pink;
        b[0] = 0.99886 * b[0] + w * 0.0555179;
        b[1] = 0.99332 * b[1] + w * 0.0750759;
        b[2] = 0.96900 * b[2] + w * 0.153_852;
        b[3] = 0.86650 * b[3] + w * 0.3104856;
        b[4] = 0.55000 * b[4] + w * 0.5329522;
        b[5] = -0.7616 * b[5] - w * 0.0168980;
        let out = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
        b[6] = w * 0.115926;
        out
    }

    #[inline]
    fn sample(&mut self) -> f32 {
        let w = self.white();
        // The gains bring every color to about the same loudness as a
        // wavetable (RMS ~0.2), see the test below.
        match self.color {
            Color::White => w * 0.35,
            Color::Pink => self.pinked(w) * 0.07,
            Color::Brown => {
                // A leaky integrator: -6 dB per octave above a few Hz.
                self.last = (self.last + 0.02 * w) / 1.02;
                self.last * 3.2
            }
            Color::Blue => {
                // The slope of pink: +6 - 3 = +3 dB per octave.
                let p = self.pinked(w);
                let out = p - self.last;
                self.last = p;
                out * 0.16
            }
            Color::Violet => {
                let out = w - self.last;
                self.last = w;
                out * 0.25
            }
        }
    }
}

impl Node for Noise {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        for frame in out.iter_mut() {
            let s = self.sample();
            *frame = [s, s];
        }
        out.len()
    }

    fn reset(&mut self) {
        // Never 0, the one state xorshift can't leave.
        self.state = mix(self.seed) | 1;
        self.pink = [0.0; 7];
        self.last = 0.0;
    }

    fn skip(&mut self, frames: usize) -> usize {
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(color: Color, n: usize) -> Vec<f32> {
        let mut buf = vec![[0.0; 2]; n];
        Noise::new(color, 1).process(&mut buf);
        buf.iter().map(|f| f[0]).collect()
    }

    fn rms(x: &[f32]) -> f32 {
        (x.iter().map(|s| s * s).sum::<f32>() / x.len() as f32).sqrt()
    }

    /// Energy below and above ~1.5 kHz at 48 kHz (one-pole split).
    fn low_high(x: &[f32]) -> (f32, f32) {
        let (mut lp, mut low, mut high) = (0.0f32, Vec::new(), Vec::new());
        for &s in x {
            lp += 0.18 * (s - lp);
            low.push(lp);
            high.push(s - lp);
        }
        (rms(&low), rms(&high))
    }

    #[test]
    fn colors_are_about_equally_loud_and_differently_bright() {
        let mut brightness = Vec::new();
        for color in [
            Color::Brown,
            Color::Pink,
            Color::White,
            Color::Blue,
            Color::Violet,
        ] {
            let x = render(color, 96_000);
            let level = rms(&x);
            assert!((0.12..0.3).contains(&level), "{color:?}: rms {level}");
            assert!(x.iter().all(|s| s.abs() < 1.0), "{color:?} clips");
            let (low, high) = low_high(&x);
            brightness.push(high / low);
        }
        // From dark to bright, in order.
        assert!(brightness.windows(2).all(|w| w[0] < w[1]), "{brightness:?}");
    }

    #[test]
    fn seeds_give_different_streams_and_reset_repeats() {
        let mut a = Noise::new(Color::White, 1);
        let mut buf = [[0.0; 2]; 64];
        a.process(&mut buf);
        let first = buf;
        a.reset();
        a.process(&mut buf);
        assert_eq!(buf, first);
        Noise::new(Color::White, 2).process(&mut buf);
        assert_ne!(buf, first);
        assert_ne!(next_seed(), next_seed());
    }

    #[test]
    fn random_numbers_are_uniform() {
        let n = 10_000;
        let mean = (0..n).map(|k| random_at(7, k)).sum::<f64>() / n as f64;
        assert!((mean - 0.5).abs() < 0.02, "{mean}");
        assert!((0..n).all(|k| (0.0..1.0).contains(&random_at(7, k))));
        assert_eq!(random_at(7, 3), random_at(7, 3));
        assert_ne!(random_at(7, 3), random_at(8, 3));
    }
}
