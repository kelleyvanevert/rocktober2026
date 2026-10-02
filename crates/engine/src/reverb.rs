//! Convolution reverb.
//!
//! Convolving with an impulse response (IR) sample by sample would cost
//! `IR length` multiplications per sample: far too slow for a seconds-long IR.
//! Instead this uses uniformly partitioned convolution: the IR is cut into
//! blocks of `BLOCK` frames, each block is FFT'd once up front, and every
//! incoming block of audio is FFT'd once and multiplied against all of them in
//! the frequency domain. The cost per sample is then roughly
//! `IR length / BLOCK` complex multiplications, plus a few small FFTs.
//!
//! The wet signal comes out `BLOCK` frames (about 5 ms) late, which for a
//! reverb just acts as a little extra pre-delay; the dry signal isn't delayed.

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

use crate::nodes::{Frame, Node};

const BLOCK: usize = 256;
/// Longest IR we'll accept; longer sounds are cut off (with a fade).
pub const MAX_IR_SECONDS: f64 = 10.0;

/// An impulse response, prepared for convolution: per channel, the spectrum of
/// each `BLOCK`-sized piece. Shared by every reverb node that uses it.
pub struct Impulse {
    partitions: [Vec<Vec<Complex<f32>>>; 2],
    len: usize,
    fft: Arc<dyn RealToComplex<f32>>,
    ifft: Arc<dyn ComplexToReal<f32>>,
}

impl Impulse {
    /// Prepare a stereo IR (already at the output sample rate). It's normalized
    /// to unit energy per channel, so the reverb is about as loud as the input
    /// whatever IR you use.
    pub fn new(mut ir: Vec<Frame>) -> Self {
        // Silence at the end costs as much CPU as sound, so drop it.
        while ir.last().is_some_and(|f| f[0].abs().max(f[1].abs()) < 1e-5) {
            ir.pop();
        }
        if ir.is_empty() {
            ir.push([0.0; 2]);
        }

        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(2 * BLOCK);
        let ifft = planner.plan_fft_inverse(2 * BLOCK);

        let partitions = [0, 1].map(|c| {
            let energy: f32 = ir.iter().map(|f| f[c] * f[c]).sum();
            let scale = if energy > 0.0 {
                1.0 / energy.sqrt()
            } else {
                0.0
            };
            ir.chunks(BLOCK)
                .map(|chunk| {
                    let mut input = fft.make_input_vec();
                    for (x, f) in input.iter_mut().zip(chunk) {
                        *x = f[c] * scale;
                    }
                    let mut spectrum = fft.make_output_vec();
                    fft.process(&mut input, &mut spectrum).unwrap();
                    spectrum
                })
                .collect()
        });

        Self {
            partitions,
            len: ir.len(),
            fft,
            ifft,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }
}

pub struct Reverb {
    child: Box<dyn Node>,
    child_done: bool,
    impulse: Arc<Impulse>,
    mix: f32,
    /// Frames of wet tail still to produce after the child ends.
    tail: usize,

    /// The previous and current input block (mono), in time order.
    window: Vec<f32>,
    /// How much of the current input block is filled.
    fill: usize,
    /// Spectra of past input blocks, newest at `head`.
    history: Vec<Vec<Complex<f32>>>,
    head: usize,
    /// The wet output for the block being played now, per channel.
    wet: [Vec<f32>; 2],

    fft_in: Vec<f32>,
    accumulator: Vec<Complex<f32>>,
    fft_out: Vec<f32>,
    scratch: Vec<Complex<f32>>,
}

impl Reverb {
    /// `mix` crossfades from dry (0) to fully wet (1). Allocates everything up
    /// front; processing allocates nothing.
    pub fn new(child: Box<dyn Node>, impulse: Arc<Impulse>, mix: f32) -> Self {
        let parts = impulse.partitions[0].len();
        let fft = &impulse.fft;
        let scratch_len = fft.get_scratch_len().max(impulse.ifft.get_scratch_len());
        Self {
            child,
            child_done: false,
            mix,
            tail: impulse.len + BLOCK,
            window: vec![0.0; 2 * BLOCK],
            fill: 0,
            history: vec![fft.make_output_vec(); parts],
            head: 0,
            wet: [vec![0.0; BLOCK], vec![0.0; BLOCK]],
            fft_in: fft.make_input_vec(),
            accumulator: fft.make_output_vec(),
            fft_out: impulse.ifft.make_output_vec(),
            scratch: vec![Complex::default(); scratch_len],
            impulse,
        }
    }

    /// A full input block is in: convolve it, producing the next wet block.
    fn convolve(&mut self) {
        let parts = self.history.len();
        self.head = (self.head + 1) % parts;
        self.fft_in.copy_from_slice(&self.window);
        let imp = &self.impulse;
        imp.fft
            .process_with_scratch(
                &mut self.fft_in,
                &mut self.history[self.head],
                &mut self.scratch,
            )
            .unwrap();

        for c in 0..2 {
            self.accumulator.fill(Complex::default());
            for (p, h) in imp.partitions[c].iter().enumerate() {
                let x = &self.history[(self.head + parts - p) % parts];
                for ((acc, x), h) in self.accumulator.iter_mut().zip(x).zip(h) {
                    *acc += x * h;
                }
            }
            // A real signal's DC and Nyquist bins are real; rounding can leave a
            // speck of imaginary part, which the inverse FFT rejects.
            self.accumulator[0].im = 0.0;
            self.accumulator[BLOCK].im = 0.0;
            imp.ifft
                .process_with_scratch(&mut self.accumulator, &mut self.fft_out, &mut self.scratch)
                .unwrap();
            // Overlap-save: the second half is the linear convolution of the
            // current block. (realfft doesn't normalize, hence the division.)
            let norm = 1.0 / (2 * BLOCK) as f32;
            for (w, y) in self.wet[c].iter_mut().zip(&self.fft_out[BLOCK..]) {
                *w = y * norm;
            }
        }

        self.window.copy_within(BLOCK.., 0);
    }
}

impl Node for Reverb {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let mut n = if self.child_done {
            0
        } else {
            self.child.process(out)
        };
        if n < out.len() {
            self.child_done = true;
            let more = (out.len() - n).min(self.tail);
            out[n..n + more].fill([0.0; 2]);
            self.tail -= more;
            n += more;
        }
        for frame in &mut out[..n] {
            self.window[BLOCK + self.fill] = (frame[0] + frame[1]) * 0.5;
            let wet = [self.wet[0][self.fill], self.wet[1][self.fill]];
            for c in 0..2 {
                frame[c] = frame[c] * (1.0 - self.mix) + wet[c] * self.mix;
            }
            self.fill += 1;
            if self.fill == BLOCK {
                self.fill = 0;
                self.convolve();
            }
        }
        n
    }

    fn reset(&mut self) {
        self.child.reset();
        self.child_done = false;
        self.tail = self.impulse.len + BLOCK;
        self.window.fill(0.0);
        self.fill = 0;
        self.history
            .iter_mut()
            .for_each(|h| h.fill(Complex::default()));
        self.wet.iter_mut().for_each(|w| w.fill(0.0));
    }
}

/// Settings for a synthesized space.
struct Space {
    name: &'static str,
    /// Seconds for the low end to decay by 60 dB.
    decay: f64,
    /// Same, for the high end. Shorter = darker.
    high_decay: f64,
    /// Silence before the reverb starts.
    predelay: f64,
}

const SPACES: &[Space] = &[
    Space {
        name: "small_room",
        decay: 0.4,
        high_decay: 0.2,
        predelay: 0.002,
    },
    Space {
        name: "bright_room",
        decay: 0.8,
        high_decay: 0.7,
        predelay: 0.004,
    },
    Space {
        name: "dark_room",
        decay: 0.9,
        high_decay: 0.25,
        predelay: 0.004,
    },
    Space {
        name: "plate",
        decay: 1.8,
        high_decay: 1.5,
        predelay: 0.0,
    },
    Space {
        name: "hall",
        decay: 2.4,
        high_decay: 1.2,
        predelay: 0.015,
    },
    Space {
        name: "cathedral",
        decay: 6.0,
        high_decay: 2.5,
        predelay: 0.03,
    },
];

pub fn preset_names() -> Vec<&'static str> {
    SPACES.iter().map(|s| s.name).collect()
}

/// Synthesize the IR of a named space: stereo noise (independent per side, for
/// width) that decays exponentially, with the highs dying away faster than the
/// lows, the way air and soft surfaces absorb them.
pub fn preset(name: &str, sample_rate: u32) -> Option<Vec<Frame>> {
    let space = SPACES.iter().find(|s| s.name == name)?;
    let rate = sample_rate as f64;
    let predelay = (space.predelay * rate) as usize;
    let len = predelay + (space.decay * rate) as usize;
    // 60 dB is a factor 1000; ln(1000) ≈ 6.9.
    let low_per_frame = (-6.9 / (space.decay * rate)).exp() as f32;
    let high_per_frame = (-6.9 / (space.high_decay * rate)).exp() as f32;
    // One-pole lowpass at ~1.5 kHz splits the noise into lows and highs.
    let split = 1.0 - (-2.0 * std::f64::consts::PI * 1500.0 / rate).exp() as f32;
    // A few ms fade-in, so the onset is soft rather than a burst of noise.
    let attack = (0.005 * rate) as usize;

    let mut rng = 0x2545_f491_4f6c_dd1du64;
    let mut noise = move || {
        // xorshift: deterministic, so a preset sounds the same every time.
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 40) as f32 / (1u64 << 23) as f32 - 1.0
    };

    let mut ir = vec![[0.0; 2]; len];
    let mut low = [0.0f32; 2];
    let (mut low_env, mut high_env) = (1.0f32, 1.0f32);
    for (i, frame) in ir.iter_mut().enumerate().skip(predelay) {
        let fade = ((i - predelay) as f32 / attack as f32).min(1.0);
        for c in 0..2 {
            let n = noise();
            low[c] += (n - low[c]) * split;
            frame[c] = (low[c] * low_env + (n - low[c]) * high_env) * fade;
        }
        low_env *= low_per_frame;
        high_env *= high_per_frame;
    }
    Some(ir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::{SampleData, Sampler};

    fn source(frames: Vec<Frame>) -> Box<dyn Node> {
        Box::new(Sampler::new(
            Arc::new(SampleData {
                frames,
                sample_rate: 48_000,
            }),
            48_000,
        ))
    }

    fn render(node: &mut dyn Node) -> Vec<Frame> {
        let mut all = Vec::new();
        let mut buf = [[0.0; 2]; 100]; // deliberately not a multiple of BLOCK
        loop {
            let n = node.process(&mut buf);
            all.extend_from_slice(&buf[..n]);
            if n < buf.len() {
                return all;
            }
        }
    }

    /// Brute-force convolution, to check the FFT version against.
    fn direct(input: &[f32], ir: &[f32]) -> Vec<f32> {
        let mut out = vec![0.0; input.len() + ir.len()];
        for (i, x) in input.iter().enumerate() {
            for (j, h) in ir.iter().enumerate() {
                out[i + j] += x * h;
            }
        }
        out
    }

    #[test]
    fn matches_direct_convolution() {
        // An IR spanning several partitions, different per channel.
        let ir: Vec<Frame> = (0..700)
            .map(|i| [((i * 7) % 13) as f32 - 6.0, ((i * 3) % 5) as f32 - 2.0])
            .collect();
        let input: Vec<Frame> = (0..1000)
            .map(|i| [((i * 5) % 11) as f32 / 10.0, 0.0])
            .collect();

        let impulse = Arc::new(Impulse::new(ir.clone()));
        let out = render(&mut Reverb::new(source(input.clone()), impulse, 1.0));
        assert_eq!(out.len(), 1000 + 700 + BLOCK);

        for c in 0..2 {
            let ir_c: Vec<f32> = ir.iter().map(|f| f[c]).collect();
            let scale = 1.0 / ir_c.iter().map(|h| h * h).sum::<f32>().sqrt();
            let mono: Vec<f32> = input.iter().map(|f| (f[0] + f[1]) * 0.5).collect();
            let expected = direct(&mono, &ir_c);
            for (i, e) in expected.iter().enumerate() {
                // The wet signal is BLOCK frames late.
                let got = out[i + BLOCK][c];
                assert!(
                    (got - e * scale).abs() < 1e-3,
                    "channel {c} frame {i}: {got} vs {}",
                    e * scale
                );
            }
        }
    }

    #[test]
    fn dry_passes_through_and_tail_rings_out() {
        let ir = preset("hall", 48_000).unwrap();
        let impulse = Arc::new(Impulse::new(ir));
        let len = impulse.len();
        let mut click = vec![[0.0; 2]; 10];
        click[0] = [1.0, 1.0];
        let out = render(&mut Reverb::new(source(click), impulse, 0.5));
        assert_eq!(
            out[0],
            [0.5, 0.5],
            "dry, unaffected by the reverb's latency"
        );
        assert_eq!(out.len(), 10 + len + BLOCK);
        let late = out[out.len() / 2];
        assert!(late[0] != 0.0 && late[0] != late[1], "a decorrelated tail");
    }

    #[test]
    fn presets_decay() {
        for name in preset_names() {
            let ir = preset(name, 48_000).unwrap();
            let rms = |part: &[Frame]| {
                (part.iter().map(|f| f[0] * f[0]).sum::<f32>() / part.len() as f32).sqrt()
            };
            let tenth = ir.len() / 10;
            assert!(
                rms(&ir[tenth..2 * tenth]) > 10.0 * rms(&ir[8 * tenth..9 * tenth]),
                "{name}"
            );
        }
        assert!(preset("nope", 48_000).is_none());
    }
}
