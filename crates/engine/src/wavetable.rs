//! Wavetables and the oscillator that plays them.
//!
//! A wavetable is a series of single-cycle waveforms ("frames") of `SIZE`
//! samples; the position parameter morphs through them. Playing a cycle with
//! many harmonics at a high pitch would alias (harmonics above Nyquist fold
//! back down as inharmonic noise), so every frame is stored at `LEVELS`
//! bandwidths, one per octave, made by cutting off its spectrum. The
//! oscillator reads from the levels that fit its pitch, crossfading between
//! the two nearest so a pitch sweep doesn't step in brightness.
//!
//! Tables are built on the application thread; the oscillator only reads them.

use std::sync::Arc;

use realfft::RealFftPlanner;
use realfft::num_complex::Complex;

use crate::control::Param;
use crate::nodes::{Frame, Node};
use crate::sample;

/// Samples per frame: the Serum convention, so its tables load as they are.
pub const SIZE: usize = 2048;
/// Level `k` keeps the harmonics up to `SIZE / 2 >> k`, from all 1024 down to 1.
const LEVELS: usize = 11;
/// Each stored waveform has one extra sample (a copy of the first), so
/// interpolation never has to wrap around.
const STRIDE: usize = SIZE + 1;
/// Frames in the built-in tables.
const BUILTIN_FRAMES: usize = 32;
/// Every frame is normalized to this peak (-6 dBFS), so morphing doesn't jump
/// in loudness and a plain oscillator isn't painfully loud.
const PEAK: f32 = 0.5;

pub struct Wavetable {
    frames: usize,
    /// For each frame, for each level, `STRIDE` samples.
    data: Vec<f32>,
}

/// A frame's spectrum: one complex value per harmonic, from DC up to `SIZE / 2`.
type Spectrum = Vec<Complex<f32>>;

/// The built-in tables, by name.
pub const BUILTIN_NAMES: [&str; 5] = ["basic", "sine-square", "sine-saw", "bright", "pulse"];

fn spectrum(harmonic: impl Fn(usize) -> Complex<f32>) -> Spectrum {
    (0..=SIZE / 2)
        .map(|h| {
            if h == 0 {
                Complex::default()
            } else {
                harmonic(h)
            }
        })
        .collect()
}

/// A harmonic `a·sin(h·θ)`.
fn sine_part(a: f32) -> Complex<f32> {
    Complex::new(0.0, -a)
}

fn sine() -> Spectrum {
    spectrum(|h| sine_part(if h == 1 { 1.0 } else { 0.0 }))
}

fn saw() -> Spectrum {
    spectrum(|h| sine_part(if h % 2 == 1 { 1.0 } else { -1.0 } / h as f32))
}

fn square() -> Spectrum {
    spectrum(|h| sine_part(if h % 2 == 1 { 1.0 / h as f32 } else { 0.0 }))
}

fn triangle() -> Spectrum {
    spectrum(|h| {
        let sign = if h % 4 == 1 { 1.0 } else { -1.0 };
        sine_part(if h % 2 == 1 {
            sign / (h * h) as f32
        } else {
            0.0
        })
    })
}

/// A pulse that's high for `width` of the cycle.
fn pulse(width: f32) -> Spectrum {
    use std::f32::consts::PI;
    spectrum(|h| Complex::new((PI * h as f32 * width).sin() / h as f32, 0.0))
}

/// `frames` frames morphing evenly through `keys` (crossfading their spectra).
fn morph(keys: &[Spectrum], frames: usize) -> Vec<Spectrum> {
    (0..frames)
        .map(|f| {
            let x = f as f32 / (frames - 1) as f32 * (keys.len() - 1) as f32;
            let i = (x as usize).min(keys.len() - 2);
            let t = x - i as f32;
            keys[i]
                .iter()
                .zip(&keys[i + 1])
                .map(|(a, b)| a * (1.0 - t) + b * t)
                .collect()
        })
        .collect()
}

impl Wavetable {
    /// A built-in table, if there's one with this name.
    pub fn builtin(name: &str) -> Option<Self> {
        let spectra = match name {
            "basic" => morph(&[sine(), triangle(), saw(), square()], BUILTIN_FRAMES),
            "sine-square" => morph(&[sine(), square()], BUILTIN_FRAMES),
            "sine-saw" => morph(&[sine(), saw()], BUILTIN_FRAMES),
            // Sine to saw by adding harmonics one octave-ish at a time, like
            // opening a filter.
            "bright" => (0..BUILTIN_FRAMES)
                .map(|f| {
                    let count = 1.25f32.powi(f as i32).round() as usize;
                    let full = saw();
                    spectrum(|h| {
                        if h <= count {
                            full[h]
                        } else {
                            Complex::default()
                        }
                    })
                })
                .collect(),
            "pulse" => (0..BUILTIN_FRAMES)
                .map(|f| pulse(0.5 - 0.48 * f as f32 / (BUILTIN_FRAMES - 1) as f32))
                .collect(),
            _ => return None,
        };
        Some(Self::from_spectra(spectra))
    }

    /// Load a WAV file of back-to-back `SIZE`-sample cycles (Serum's format).
    pub fn load(source: &sample::Source) -> Result<Self, String> {
        let data = sample::load(source)?;
        let mono: Vec<f32> = data.frames.iter().map(|[l, r]| (l + r) / 2.0).collect();
        if mono.len() < SIZE {
            return Err(format!(
                "{}: a wavetable needs cycles of {SIZE} samples, this has only {}",
                source.name(),
                mono.len()
            ));
        }
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(SIZE);
        let spectra = mono
            .as_chunks::<SIZE>()
            .0
            .iter()
            .map(|cycle| {
                let mut input = cycle.to_vec();
                let mut output = fft.make_output_vec();
                fft.process(&mut input, &mut output).unwrap();
                output
            })
            .collect();
        Ok(Self::from_spectra(spectra))
    }

    fn from_spectra(spectra: Vec<Spectrum>) -> Self {
        let ifft = RealFftPlanner::<f32>::new().plan_fft_inverse(SIZE);
        let mut data = Vec::with_capacity(spectra.len() * LEVELS * STRIDE);
        for spectrum in &spectra {
            let start = data.len();
            for level in 0..LEVELS {
                let top = (SIZE / 2) >> level;
                let mut bins: Spectrum = spectrum
                    .iter()
                    .enumerate()
                    .map(|(h, &c)| {
                        if h == 0 || h > top {
                            Complex::default()
                        } else {
                            c
                        }
                    })
                    .collect();
                // The inverse transform needs the Nyquist bin to be real.
                bins[SIZE / 2].im = 0.0;
                let mut cycle = vec![0.0; SIZE];
                ifft.process(&mut bins, &mut cycle).unwrap();
                data.extend_from_slice(&cycle);
                data.push(cycle[0]);
            }
            // Normalize by the full-bandwidth version's peak.
            let peak = data[start..start + STRIDE]
                .iter()
                .fold(0f32, |m, s| m.max(s.abs()));
            if peak > 0.0 {
                for s in &mut data[start..] {
                    *s *= PEAK / peak;
                }
            }
        }
        Self {
            frames: spectra.len(),
            data,
        }
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    /// One frame at one level, as `STRIDE` samples.
    fn cycle(&self, frame: usize, level: usize) -> &[f32] {
        let start = (frame * LEVELS + level) * STRIDE;
        &self.data[start..start + STRIDE]
    }

    /// The waveform at `position` (0..1 through the frames), `level` (a
    /// fractional bandwidth level) and `phase` (0..1).
    #[inline]
    fn read(&self, position: f32, level: f32, phase: f32) -> f32 {
        let x = position.clamp(0.0, 1.0) * (self.frames - 1) as f32;
        let f0 = x as usize;
        let f1 = (f0 + 1).min(self.frames - 1);
        let ft = x - f0 as f32;
        let level = level.clamp(0.0, (LEVELS - 1) as f32);
        let l0 = level as usize;
        let l1 = (l0 + 1).min(LEVELS - 1);
        let lt = level - l0 as f32;
        let p = phase * SIZE as f32;
        let i = (p as usize).min(SIZE - 1);
        let pt = p - i as f32;
        let at = |frame, level| {
            let c = self.cycle(frame, level);
            c[i] + (c[i + 1] - c[i]) * pt
        };
        let mix = |a: f32, b: f32, t: f32| a + (b - a) * t;
        mix(
            mix(at(f0, l0), at(f0, l1), lt),
            mix(at(f1, l0), at(f1, l1), lt),
            ft,
        )
    }
}

/// The frequency of a MIDI note.
pub fn note_to_hz(note: f32) -> f32 {
    440.0 * ((note - 69.0) / 12.0).exp2()
}

/// Where the bend in the phase is, for a warp amount (0..1): the first half
/// of the cycle is squeezed into the first `knee` of the time.
fn knee(warp: f32) -> f32 {
    0.5 - 0.48 * warp.clamp(0.0, 1.0)
}

/// Phase distortion (as in Casio's CZ synths): speed through the first half of
/// the cycle and slow down through the second, which brightens the sound.
#[inline]
fn warp_phase(phase: f32, knee: f32) -> f32 {
    if phase < knee {
        0.5 * phase / knee
    } else {
        0.5 + 0.5 * (phase - knee) / (1.0 - knee)
    }
}

/// Plays a wavetable. Its parameters are controls: position (0..1), warp
/// (0..1) and pitch (a MIDI note number). It never ends, unless one of its
/// controls does (like a gated envelope).
pub struct Oscillator {
    table: Arc<Wavetable>,
    position: Param,
    warp: Param,
    pitch: Param,
    phase: f32,
    sample_rate: f32,
}

impl Oscillator {
    pub fn new(
        table: Arc<Wavetable>,
        position: Param,
        warp: Param,
        pitch: Param,
        sample_rate: u32,
    ) -> Self {
        Self {
            table,
            position,
            warp,
            pitch,
            phase: 0.0,
            sample_rate: sample_rate as f32,
        }
    }

    /// The fractional bandwidth level for a frequency and warp knee: the
    /// highest harmonic, sped up by the warp, should stay below Nyquist. Half a
    /// level of leeway keeps the sound bright; what little aliases then lands
    /// above ~60% of Nyquist.
    fn level(&self, hz: f32, knee: f32) -> f32 {
        let nyquist = self.sample_rate / 2.0;
        let speedup = 0.5 / knee;
        ((SIZE / 2) as f32 * hz * speedup / nyquist)
            .max(1e-6)
            .log2()
            + 0.5
    }
}

impl Node for Oscillator {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let block = self
            .position
            .block()
            .min(self.warp.block())
            .min(self.pitch.block());
        let mut done = 0;
        while done < out.len() {
            let len = (out.len() - done).min(block);
            let n = self
                .position
                .next(len)
                .min(self.warp.next(len))
                .min(self.pitch.next(len));
            for (i, frame) in out[done..done + n].iter_mut().enumerate() {
                let hz = note_to_hz(self.pitch.get(i)).clamp(0.0, self.sample_rate / 2.0);
                let knee = knee(self.warp.get(i));
                let level = self.level(hz, knee);
                let phase = warp_phase(self.phase, knee);
                let s = self.table.read(self.position.get(i), level, phase);
                *frame = [s, s];
                self.phase += hz / self.sample_rate;
                self.phase -= self.phase.floor();
            }
            done += n;
            if n < len {
                break;
            }
        }
        done
    }

    fn reset(&mut self) {
        self.phase = 0.0;
        self.position.reset();
        self.warp.reset();
        self.pitch.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn osc(table: &str, position: f32, warp: f32, note: f32) -> Oscillator {
        Oscillator::new(
            Arc::new(Wavetable::builtin(table).unwrap()),
            Param::Const(position),
            Param::Const(warp),
            Param::Const(note),
            48_000,
        )
    }

    fn render(osc: &mut Oscillator, frames: usize) -> Vec<f32> {
        let mut out = vec![[0.0; 2]; frames];
        assert_eq!(osc.process(&mut out), frames);
        out.iter().map(|f| f[0]).collect()
    }

    /// Upward zero crossings per second.
    fn frequency(samples: &[f32]) -> f32 {
        let crossings = samples
            .windows(2)
            .filter(|w| w[0] < 0.0 && w[1] >= 0.0)
            .count();
        crossings as f32 * 48_000.0 / samples.len() as f32
    }

    /// The level of each harmonic of `hz` (and anything in between), by brute
    /// force DFT over a whole number of cycles.
    fn magnitude(samples: &[f32], hz: f32) -> f32 {
        let (mut re, mut im) = (0.0f64, 0.0f64);
        for (n, s) in samples.iter().enumerate() {
            let t = std::f64::consts::TAU * hz as f64 * n as f64 / 48_000.0;
            re += *s as f64 * t.cos();
            im += *s as f64 * t.sin();
        }
        ((re * re + im * im).sqrt() * 2.0 / samples.len() as f64) as f32
    }

    #[test]
    fn notes_have_the_right_frequency() {
        assert_eq!(note_to_hz(69.0), 440.0);
        assert!((note_to_hz(60.0) - 261.63).abs() < 0.01);
        let a4 = render(&mut osc("basic", 0.0, 0.0, 69.0), 48_000);
        assert!((frequency(&a4) - 440.0).abs() <= 1.0);
    }

    #[test]
    fn frames_are_normalized() {
        for name in BUILTIN_NAMES {
            let table = Wavetable::builtin(name).unwrap();
            assert_eq!(table.frames(), BUILTIN_FRAMES);
            for f in 0..table.frames() {
                let peak = table.cycle(f, 0).iter().fold(0f32, |m, s| m.max(s.abs()));
                assert!((peak - PEAK).abs() < 1e-4, "{name} frame {f}: {peak}");
            }
        }
    }

    #[test]
    fn position_morphs_from_sine_to_square() {
        // At 100 Hz, a whole number of cycles in 4800 frames.
        let sine = render(&mut osc("sine-square", 0.0, 0.0, 43.35), 4800);
        let square = render(&mut osc("sine-square", 1.0, 0.0, 43.35), 4800);
        let hz = note_to_hz(43.35);
        assert!(magnitude(&sine, 3.0 * hz) < 0.005);
        // A square's third harmonic is a third of its fundamental.
        let ratio = magnitude(&square, 3.0 * hz) / magnitude(&square, hz);
        assert!((ratio - 1.0 / 3.0).abs() < 0.02, "{ratio}");
    }

    #[test]
    fn high_notes_dont_alias() {
        // A saw at ~4.7 kHz: only harmonics up to 5 are below Nyquist. Nothing
        // that isn't a harmonic should come out.
        let note = 110.0;
        let hz = note_to_hz(note);
        let saw = render(&mut osc("sine-saw", 1.0, 0.0, note), 48_000);
        let fundamental = magnitude(&saw, hz);
        for k in 1..=4 {
            assert!(magnitude(&saw, k as f32 * hz) > fundamental / (2.0 * k as f32 + 1.0));
        }
        // Where the 6th harmonic (28 kHz) would fold back to: 48k - 28.2k.
        let alias = 48_000.0 - 6.0 * hz;
        assert!(magnitude(&saw, alias) < fundamental * 0.01);
    }

    #[test]
    fn warp_brightens() {
        let hz = note_to_hz(43.35);
        let plain = render(&mut osc("basic", 0.0, 0.0, 43.35), 4800);
        let warped = render(&mut osc("basic", 0.0, 0.8, 43.35), 4800);
        assert!(magnitude(&plain, 2.0 * hz) < 0.005);
        assert!(magnitude(&warped, 2.0 * hz) > 0.05);
    }

    #[test]
    fn unknown_tables() {
        assert!(Wavetable::builtin("nope").is_none());
    }
}
