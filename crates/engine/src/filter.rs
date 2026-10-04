//! Resonant filters: lowpass, highpass and bandpass.
//!
//! A state-variable filter in its "topology-preserving transform" form (Andrew
//! Simper / Vadim Zavalishin): 12 dB per octave, and stable however fast its
//! cutoff moves, so it can be swept by envelopes and modulations.
//!
//! The cutoff is a pitch (a MIDI note number), like the oscillator's: `800hz`
//! is one, so is `c6`, so is `?note + 24` (a filter that follows the note).
//! Sweeping it in notes sounds even, where sweeping it in Hz would rush
//! through the lows.

use crate::control::Param;
use crate::nodes::{Frame, Node};
use crate::wavetable::note_to_hz;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Lowpass,
    Highpass,
    Bandpass,
}

/// The quality factor for a resonance (0..1): from a flat Butterworth (0.707)
/// to a sharp peak just short of ringing on its own (25), evenly in between on
/// a log scale.
pub fn q(resonance: f32) -> f32 {
    std::f32::consts::FRAC_1_SQRT_2
        * (25.0 / std::f32::consts::FRAC_1_SQRT_2).powf(resonance.clamp(0.0, 1.0))
}

/// One channel's filter: coefficients plus the two integrators.
#[derive(Clone, Copy, Default)]
pub struct Svf {
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
    ic1: f32,
    ic2: f32,
}

impl Svf {
    /// Set the cutoff (Hz) and quality factor.
    #[inline]
    pub fn tune(&mut self, hz: f32, q: f32, sample_rate: f32) {
        let hz = hz.clamp(10.0, sample_rate * 0.45);
        let g = (std::f32::consts::PI * hz / sample_rate).tan();
        self.k = 1.0 / q;
        self.a1 = 1.0 / (1.0 + g * (g + self.k));
        self.a2 = g * self.a1;
        self.a3 = g * self.a2;
    }

    #[inline]
    pub fn process(&mut self, kind: Kind, v0: f32) -> f32 {
        let v3 = v0 - self.ic2;
        let v1 = self.a1 * self.ic1 + self.a2 * v3;
        let v2 = self.ic2 + self.a2 * self.ic1 + self.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        match kind {
            Kind::Lowpass => v2,
            Kind::Highpass => v0 - self.k * v1 - v2,
            // Scaled so the peak stays at 0 dB however narrow it gets.
            Kind::Bandpass => self.k * v1,
        }
    }

    pub fn clear(&mut self) {
        self.ic1 = 0.0;
        self.ic2 = 0.0;
    }
}

/// A filter on a sound, both channels alike. Ends with the sound (or with
/// its cutoff or resonance, if one of those is a control that ends).
pub struct Filter {
    child: Box<dyn Node>,
    kind: Kind,
    cutoff: Param,
    resonance: Param,
    svf: [Svf; 2],
    /// The cutoff and resonance the coefficients are for.
    tuned: (f32, f32),
    sample_rate: f32,
}

impl Filter {
    pub fn new(
        child: Box<dyn Node>,
        kind: Kind,
        cutoff: Param,
        resonance: Param,
        sample_rate: u32,
    ) -> Self {
        Self {
            child,
            kind,
            cutoff,
            resonance,
            svf: [Svf::default(); 2],
            tuned: (f32::NAN, f32::NAN),
            sample_rate: sample_rate as f32,
        }
    }
}

impl Node for Filter {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let block = self.cutoff.block().min(self.resonance.block());
        let mut done = 0;
        while done < out.len() {
            let len = (out.len() - done).min(block);
            let chunk = &mut out[done..done + len];
            let n = self.child.process(chunk);
            let n = self.cutoff.next(n).min(self.resonance.next(n));
            for (i, frame) in chunk[..n].iter_mut().enumerate() {
                let params = (self.cutoff.get(i), self.resonance.get(i));
                // Only retune when something moved: `tan` isn't free.
                if params != self.tuned {
                    self.tuned = params;
                    let (hz, q) = (note_to_hz(params.0), q(params.1));
                    for svf in &mut self.svf {
                        svf.tune(hz, q, self.sample_rate);
                    }
                }
                for (c, s) in frame.iter_mut().enumerate() {
                    *s = self.svf[c].process(self.kind, *s);
                }
            }
            done += n;
            if n < len {
                break;
            }
        }
        done
    }

    fn reset(&mut self) {
        self.child.reset();
        self.cutoff.reset();
        self.resonance.reset();
        self.svf.iter_mut().for_each(Svf::clear);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::hz_to_note;

    /// A sine of `hz` through a filter: its level once settled.
    fn gain(kind: Kind, cutoff_hz: f64, resonance: f32, hz: f32) -> f32 {
        struct Sine(f32, f32);
        impl Node for Sine {
            fn process(&mut self, out: &mut [Frame]) -> usize {
                for f in out.iter_mut() {
                    let s = (self.1 * std::f32::consts::TAU).sin();
                    *f = [s, s];
                    self.1 = (self.1 + self.0 / 48_000.0).fract();
                }
                out.len()
            }
            fn reset(&mut self) {}
        }
        let mut filter = Filter::new(
            Box::new(Sine(hz, 0.0)),
            kind,
            Param::Const(hz_to_note(cutoff_hz) as f32),
            Param::Const(resonance),
            48_000,
        );
        let mut buf = vec![[0.0; 2]; 48_000];
        filter.process(&mut buf);
        // RMS rather than peak: at 8 kHz, the samples miss the peaks.
        let settled = &buf[24_000..];
        (settled.iter().map(|f| f[0] * f[0]).sum::<f32>() / settled.len() as f32).sqrt()
            * std::f32::consts::SQRT_2
    }

    fn db(x: f32) -> f32 {
        20.0 * x.log10()
    }

    #[test]
    fn lowpass_and_highpass_cut_on_the_right_side() {
        // 12 dB per octave: two octaves away is about -24 dB.
        assert!(db(gain(Kind::Lowpass, 1000.0, 0.0, 100.0)).abs() < 0.5);
        assert!((db(gain(Kind::Lowpass, 1000.0, 0.0, 1000.0)) + 3.0).abs() < 0.5);
        assert!((db(gain(Kind::Lowpass, 1000.0, 0.0, 4000.0)) + 24.0).abs() < 2.0);
        assert!(db(gain(Kind::Highpass, 1000.0, 0.0, 8000.0)).abs() < 0.5);
        assert!((db(gain(Kind::Highpass, 1000.0, 0.0, 250.0)) + 24.0).abs() < 2.0);
    }

    #[test]
    fn bandpass_peaks_at_the_cutoff_and_narrows_with_resonance() {
        assert!(db(gain(Kind::Bandpass, 1000.0, 0.5, 1000.0)).abs() < 0.5);
        let wide = gain(Kind::Bandpass, 1000.0, 0.0, 2000.0);
        let narrow = gain(Kind::Bandpass, 1000.0, 0.8, 2000.0);
        assert!(narrow < wide * 0.3, "{narrow} vs {wide}");
    }

    #[test]
    fn resonance_peaks() {
        let flat = gain(Kind::Lowpass, 1000.0, 0.0, 1000.0);
        let peaked = gain(Kind::Lowpass, 1000.0, 0.7, 1000.0);
        assert!(db(peaked / flat) > 12.0);
    }
}
