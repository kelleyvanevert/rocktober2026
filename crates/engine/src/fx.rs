//! Stereo effects: panning, spreading (a stereo chorus) and echo.

use crate::control::Param;
use crate::filter::{Kind, Svf};
use crate::nodes::{Frame, Node};
use crate::wavetable::note_to_hz;

/// Read a ring buffer `delay` frames (fractional) behind `write`, which is
/// where the next frame will be written. `buf.len()` is a power of two.
#[inline]
fn read_behind(buf: &[Frame], write: usize, delay: f32, c: usize) -> f32 {
    let mask = buf.len() - 1;
    let pos = write as f32 - delay;
    let i = pos.floor();
    let t = pos - i;
    let i = (i as isize as usize) & mask;
    let a = buf[i][c];
    let b = buf[(i + 1) & mask][c];
    a + (b - a) * t
}

/// Process `out` a chunk at a time, so every parameter has its values ready:
/// `f(chunk, n)` gets the chunk and how many of its frames the child (and
/// parameters) produced, and returns how many it produced in the end.
fn chunked(
    out: &mut [Frame],
    params: &mut [&mut Param],
    mut f: impl FnMut(&mut [Frame], &mut [&mut Param]) -> usize,
) -> usize {
    let block = params.iter().map(|p| p.block()).min().unwrap_or(usize::MAX);
    let mut done = 0;
    while done < out.len() {
        let len = (out.len() - done).min(block);
        let n = f(&mut out[done..done + len], params);
        done += n;
        if n < len {
            break;
        }
    }
    done
}

/// Next `n` values of every parameter; how many there are.
fn next_all(params: &mut [&mut Param], n: usize) -> usize {
    params.iter_mut().fold(n, |n, p| p.next(n).min(n))
}

/// Places a sound between the speakers: -1 is left, 0 the middle, 1 right.
/// Constant power, so it doesn't dip in the middle: a sound in the middle is
/// as it was, one at the side is 3 dB louder on that side.
pub struct Pan {
    child: Box<dyn Node>,
    position: Param,
}

impl Pan {
    pub fn new(child: Box<dyn Node>, position: Param) -> Self {
        Self { child, position }
    }
}

impl Node for Pan {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let Self { child, position } = self;
        chunked(out, &mut [position], |chunk, params| {
            let n = child.process(chunk);
            let n = next_all(params, n);
            for (i, frame) in chunk[..n].iter_mut().enumerate() {
                let angle = (params[0].get(i).clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
                frame[0] *= angle.cos() * std::f32::consts::SQRT_2;
                frame[1] *= angle.sin() * std::f32::consts::SQRT_2;
            }
            n
        })
    }

    fn reset(&mut self) {
        self.child.reset();
        self.position.reset();
    }
}

/// Saturation: `tanh(drive * x) / tanh(drive)`. Full scale stays full scale,
/// but the louder a sound is, the more it's squashed, which adds overtones: on
/// a bass, ones small speakers can play. A drive of 1 barely touches it, 4
/// (12db) is warm, 16 (24db) is fuzz.
pub struct Drive {
    child: Box<dyn Node>,
    drive: Param,
}

impl Drive {
    pub fn new(child: Box<dyn Node>, drive: Param) -> Self {
        Self { child, drive }
    }
}

impl Node for Drive {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let Self { child, drive } = self;
        chunked(out, &mut [drive], |chunk, params| {
            let n = child.process(chunk);
            let n = next_all(params, n);
            for (i, frame) in chunk[..n].iter_mut().enumerate() {
                let drive = params[0].get(i).max(0.01);
                let norm = drive.tanh();
                for s in frame.iter_mut() {
                    *s = (*s * drive).tanh() / norm;
                }
            }
            n
        })
    }

    fn reset(&mut self) {
        self.child.reset();
        self.drive.reset();
    }
}

/// The two chorus voices on each side: base delay and depth (seconds), and
/// LFO rate (Hz). The right side runs half a cycle behind the left, so the
/// two sides always differ: that difference is the width.
const SPREAD_VOICES: [(f32, f32, f32); 2] = [(0.007, 0.003, 0.53), (0.016, 0.004, 0.31)];
const SPREAD_MAX_DELAY: f32 = 0.03;

/// Widens a sound: a stereo chorus whose left and right voices are modulated
/// in opposite directions. `amount` (0..1) is both how deep and how loud the
/// voices are, so 0 is the dry sound.
pub struct Spread {
    child: Box<dyn Node>,
    child_done: bool,
    amount: Param,
    buf: Vec<Frame>,
    write: usize,
    /// The LFOs' phases (0..1).
    phase: [f32; 2],
    sample_rate: f32,
    tail: usize,
}

impl Spread {
    pub fn new(child: Box<dyn Node>, amount: Param, sample_rate: u32) -> Self {
        let max = (SPREAD_MAX_DELAY * sample_rate as f32) as usize + 4;
        Self {
            child,
            child_done: false,
            amount,
            buf: vec![[0.0; 2]; max.next_power_of_two()],
            write: 0,
            phase: [0.0, 0.0],
            sample_rate: sample_rate as f32,
            tail: max,
        }
    }
}

impl Node for Spread {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let Self {
            child,
            child_done,
            amount,
            buf,
            write,
            phase,
            sample_rate,
            tail,
        } = self;
        let mask = buf.len() - 1;
        chunked(out, &mut [amount], |chunk, params| {
            let len = chunk.len();
            let mut n = if *child_done { 0 } else { child.process(chunk) };
            if n < len {
                // The child is done: let the delay lines play out.
                *child_done = true;
                let more = (len - n).min(*tail);
                chunk[n..n + more].fill([0.0; 2]);
                *tail -= more;
                n += more;
            }
            let n = next_all(params, n);
            for (i, frame) in chunk[..n].iter_mut().enumerate() {
                let amount = params[0].get(i).clamp(0.0, 1.0);
                buf[*write] = *frame;
                let mut wet = [0.0f32; 2];
                for (v, &(base, depth, _)) in SPREAD_VOICES.iter().enumerate() {
                    for (c, w) in wet.iter_mut().enumerate() {
                        let p = phase[v] + 0.5 * c as f32;
                        let lfo = (p * std::f32::consts::TAU).sin();
                        let delay = (base + depth * amount * lfo) * *sample_rate;
                        *w += read_behind(buf, *write, delay, c) * 0.5;
                    }
                }
                for (c, s) in frame.iter_mut().enumerate() {
                    *s = *s * (1.0 - 0.3 * amount) + wet[c] * 0.9 * amount;
                }
                *write = (*write + 1) & mask;
                for (v, &(_, _, rate)) in SPREAD_VOICES.iter().enumerate() {
                    phase[v] = (phase[v] + rate / *sample_rate).fract();
                }
            }
            n
        })
    }

    fn reset(&mut self) {
        self.child.reset();
        self.child_done = false;
        self.amount.reset();
        self.buf.fill([0.0; 2]);
        self.write = 0;
        self.phase = [0.0, 0.0];
        self.tail = (SPREAD_MAX_DELAY * self.sample_rate) as usize + 4;
    }
}

/// Below this, an echo has died away.
const SILENT: f32 = 1e-5;

/// Echoes of a sound, each one fed back into the delay line. The repeats pass
/// through a highpass and a lowpass (`low` and `high`, pitches like a filter
/// cutoff) every time round, so they get thinner and darker as they fade,
/// like tape or Ableton's Echo. Ping-pong bounces them between the sides,
/// starting on the left.
pub struct Echo {
    child: Box<dyn Node>,
    child_done: bool,
    /// Exactly the delay time long.
    buf: Vec<Frame>,
    pos: usize,
    feedback: Param,
    mix: Param,
    low: Param,
    high: Param,
    /// Per channel: highpass, lowpass.
    filters: [[Svf; 2]; 2],
    tuned: (f32, f32),
    pingpong: bool,
    /// Frames in a row that were (nearly) silent, after the child ended.
    quiet: usize,
    sample_rate: f32,
}

pub struct EchoParams {
    pub feedback: Param,
    pub mix: Param,
    pub low: Param,
    pub high: Param,
    pub pingpong: bool,
}

impl Echo {
    pub fn new(child: Box<dyn Node>, frames: usize, params: EchoParams, sample_rate: u32) -> Self {
        Self {
            child,
            child_done: false,
            buf: vec![[0.0; 2]; frames.max(1)],
            pos: 0,
            feedback: params.feedback,
            mix: params.mix,
            low: params.low,
            high: params.high,
            filters: [[Svf::default(); 2]; 2],
            tuned: (f32::NAN, f32::NAN),
            pingpong: params.pingpong,
            quiet: 0,
            sample_rate: sample_rate as f32,
        }
    }
}

impl Node for Echo {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let Self {
            child,
            child_done,
            buf,
            pos,
            feedback,
            mix,
            low,
            high,
            filters,
            tuned,
            pingpong,
            quiet,
            sample_rate,
        } = self;
        chunked(out, &mut [feedback, mix, low, high], |chunk, params| {
            let len = chunk.len();
            let n = if *child_done { 0 } else { child.process(chunk) };
            if n < len {
                *child_done = true;
                chunk[n..].fill([0.0; 2]);
            }
            let m = next_all(params, len);
            for (i, frame) in chunk[..m].iter_mut().enumerate() {
                if *child_done && i >= n && *quiet > buf.len() {
                    return i;
                }
                let (feedback, mix) = (
                    params[0].get(i).clamp(0.0, 1.0),
                    params[1].get(i).clamp(0.0, 1.0),
                );
                let cutoffs = (params[2].get(i), params[3].get(i));
                if cutoffs != *tuned {
                    *tuned = cutoffs;
                    for [hp, lp] in filters.iter_mut() {
                        hp.tune(
                            note_to_hz(cutoffs.0),
                            std::f32::consts::FRAC_1_SQRT_2,
                            *sample_rate,
                        );
                        lp.tune(
                            note_to_hz(cutoffs.1),
                            std::f32::consts::FRAC_1_SQRT_2,
                            *sample_rate,
                        );
                    }
                }
                let delayed = buf[*pos];
                let mut wet = [0.0f32; 2];
                for (c, w) in wet.iter_mut().enumerate() {
                    let [hp, lp] = &mut filters[c];
                    *w = lp.process(Kind::Lowpass, hp.process(Kind::Highpass, delayed[c]));
                }
                buf[*pos] = if *pingpong {
                    [
                        (frame[0] + frame[1]) * 0.5 + feedback * wet[1],
                        feedback * wet[0],
                    ]
                } else {
                    [frame[0] + feedback * wet[0], frame[1] + feedback * wet[1]]
                };
                let loudest = wet
                    .iter()
                    .chain(&buf[*pos])
                    .fold(0f32, |m, s| m.max(s.abs()));
                *quiet = if loudest < SILENT { *quiet + 1 } else { 0 };
                *pos = (*pos + 1) % buf.len();
                for (c, s) in frame.iter_mut().enumerate() {
                    *s = *s * (1.0 - mix) + wet[c] * mix;
                }
            }
            m
        })
    }

    fn reset(&mut self) {
        self.child.reset();
        self.child_done = false;
        self.buf.fill([0.0; 2]);
        self.pos = 0;
        for p in [
            &mut self.feedback,
            &mut self.mix,
            &mut self.low,
            &mut self.high,
        ] {
            p.reset();
        }
        self.filters.iter_mut().flatten().for_each(Svf::clear);
        self.quiet = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nodes::{SampleData, Sampler};
    use std::sync::Arc;

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
        let mut buf = [[0.0; 2]; 300];
        loop {
            let n = node.process(&mut buf);
            all.extend_from_slice(&buf[..n]);
            if n < buf.len() || all.len() > 48_000 * 60 {
                return all;
            }
        }
    }

    #[test]
    fn pan_moves_between_the_sides() {
        let at = |p: f32| render(&mut Pan::new(source(vec![[1.0, 1.0]; 10]), Param::Const(p)))[0];
        assert_eq!(at(0.0).map(|s| (s * 1000.0).round()), [1000.0, 1000.0]);
        let left = at(-1.0);
        assert!((left[0] - std::f32::consts::SQRT_2).abs() < 1e-5 && left[1].abs() < 1e-5);
        let right = at(1.0);
        assert!(right[0].abs() < 1e-5 && right[1] > 1.4);
    }

    #[test]
    fn drive_keeps_full_scale_and_squashes_below_it() {
        let out = render(&mut Drive::new(
            source(vec![[1.0, -1.0], [0.25, 0.0]]),
            Param::Const(4.0),
        ));
        assert!((out[0][0] - 1.0).abs() < 1e-5 && (out[0][1] + 1.0).abs() < 1e-5);
        // 0.25 comes out much closer to full scale: that's the squashing.
        assert!(out[1][0] > 0.7 && out[1][0] < 1.0, "{}", out[1][0]);
        assert_eq!(out[1][1], 0.0);
    }

    /// A mono sine, one second.
    fn sine() -> Vec<Frame> {
        (0..48_000)
            .map(|i| {
                let s = (i as f32 * 440.0 / 48_000.0 * std::f32::consts::TAU).sin() * 0.5;
                [s, s]
            })
            .collect()
    }

    fn side(frames: &[Frame]) -> f32 {
        frames.iter().map(|f| (f[0] - f[1]).abs()).sum::<f32>() / frames.len() as f32
    }

    #[test]
    fn spread_widens_and_rings_out() {
        let dry = render(&mut Spread::new(source(sine()), Param::Const(0.0), 48_000));
        assert!(side(&dry) < 1e-6, "no spread, no width");
        let wide = render(&mut Spread::new(source(sine()), Param::Const(1.0), 48_000));
        assert!(side(&wide[4800..]) > 0.05, "{}", side(&wide));
        // The delay lines play out after the sound ends.
        assert!(wide.len() > 48_000 && wide.len() < 48_000 + 2048);
    }

    #[test]
    fn echoes_repeat_and_fade() {
        // A short burst rather than a single-sample click, so the (open)
        // filters in the loop don't round it off.
        let click = vec![[1.0, 1.0]; 200];
        let params = |feedback, pingpong| EchoParams {
            feedback: Param::Const(feedback),
            mix: Param::Const(0.5),
            // Wide open, so the repeats are (nearly) the click itself.
            low: Param::Const(0.0),
            high: Param::Const(150.0),
            pingpong,
        };
        let out = render(&mut Echo::new(
            source(click.clone()),
            1000,
            params(0.5, false),
            48_000,
        ));
        let peak = |from: usize, c: usize| {
            out[from..from + 200]
                .iter()
                .fold(0f32, |m, f| m.max(f[c].abs()))
        };
        assert!((out[0][0] - 0.5).abs() < 1e-6, "dry half");
        assert!(
            (peak(1000, 0) - 0.5).abs() < 0.1,
            "first echo {}",
            peak(1000, 0)
        );
        assert!(
            (peak(2000, 0) - 0.25).abs() < 0.06,
            "second {}",
            peak(2000, 0)
        );
        // It ends once the repeats have died away.
        assert!(out.len() > 10_000 && out.len() < 48_000, "{}", out.len());

        let out = render(&mut Echo::new(
            source(click),
            1000,
            params(0.5, true),
            48_000,
        ));
        let peak = |from: usize, c: usize| {
            out[from + 50..from + 150]
                .iter()
                .fold(0f32, |m, f| m.max(f[c].abs()))
        };
        assert!(
            peak(1000, 0) > 0.2 && peak(1000, 1) < 1e-3,
            "first echo on the left"
        );
        // (The left isn't quite silent: the low cut rings a little after the
        // first echo.)
        assert!(
            peak(2000, 1) > 0.1 && peak(2000, 0) < 0.3 * peak(2000, 1),
            "then on the right"
        );
    }

    #[test]
    fn echo_filters_darken_the_repeats() {
        let noise = || {
            let mut n = crate::noise::Noise::new(crate::noise::Color::White, 3);
            let mut f = vec![[0.0; 2]; 4800];
            n.process(&mut f);
            f
        };
        let brightness = |high: f32| {
            let mut echo = Echo::new(
                source(noise()),
                4800,
                EchoParams {
                    feedback: Param::Const(0.5),
                    mix: Param::Const(1.0),
                    low: Param::Const(0.0),
                    high: Param::Const(high),
                    pingpong: false,
                },
                48_000,
            );
            let out = render(&mut echo);
            // Mean absolute difference between neighbours: high for bright.
            let first = &out[4800..9600];
            first
                .windows(2)
                .map(|w| (w[1][0] - w[0][0]).abs())
                .sum::<f32>()
                / first.iter().map(|f| f[0].abs()).sum::<f32>()
        };
        assert!(brightness(crate::lang::hz_to_note(1000.0) as f32) < 0.5 * brightness(150.0));
    }
}
