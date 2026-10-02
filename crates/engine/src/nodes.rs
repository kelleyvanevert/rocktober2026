//! Audio nodes: the stateful objects that actually produce sound.
//!
//! Everything in here runs on the audio thread, so `process` must never allocate,
//! lock a mutex, print, or touch the filesystem. Nodes are built on the application
//! thread and handed to the audio thread fully constructed.

use std::collections::VecDeque;
use std::sync::Arc;

/// One stereo frame: [left, right].
pub type Frame = [f32; 2];

/// Decoded audio, shared (via `Arc`) between every node that plays it.
pub struct SampleData {
    pub frames: Vec<Frame>,
    pub sample_rate: u32,
}

pub trait Node: Send {
    /// Overwrite up to `out.len()` frames. Returns how many frames were written;
    /// returning fewer than `out.len()` means the node has finished.
    fn process(&mut self, out: &mut [Frame]) -> usize;

    /// Rewind to the beginning so the node can play again (used by `Repeat`).
    fn reset(&mut self);
}

/// Plays a sample once, resampling on the fly (linear interpolation) if the file's
/// sample rate differs from the output device's.
pub struct Sampler {
    data: Arc<SampleData>,
    pos: f64,
    step: f64,
}

impl Sampler {
    pub fn new(data: Arc<SampleData>, output_rate: u32) -> Self {
        let step = data.sample_rate as f64 / output_rate as f64;
        Self {
            data,
            pos: 0.0,
            step,
        }
    }
}

impl Node for Sampler {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let frames = &self.data.frames;
        let mut written = 0;
        for slot in out.iter_mut() {
            let i = self.pos as usize;
            if i >= frames.len() {
                break;
            }
            let t = (self.pos - i as f64) as f32;
            let a = frames[i];
            let b = frames.get(i + 1).copied().unwrap_or([0.0; 2]);
            *slot = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
            self.pos += self.step;
            written += 1;
        }
        written
    }

    fn reset(&mut self) {
        self.pos = 0.0;
    }
}

/// Forces its child to an exact length: pads with silence if the child is shorter,
/// cuts it off if it's longer. The last `fade` frames are faded out linearly so a
/// cut never produces a click.
pub struct Fit {
    child: Box<dyn Node>,
    child_done: bool,
    len: usize,
    fade: usize,
    pos: usize,
}

impl Fit {
    pub fn new(child: Box<dyn Node>, len: usize, fade: usize) -> Self {
        Self {
            child,
            child_done: false,
            len,
            fade: fade.min(len),
            pos: 0,
        }
    }
}

impl Node for Fit {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let n = out.len().min(self.len - self.pos);
        let out = &mut out[..n];

        let written = if self.child_done {
            0
        } else {
            self.child.process(out)
        };
        if written < n {
            self.child_done = true;
            out[written..].fill([0.0; 2]);
        }

        let fade_start = self.len - self.fade;
        if self.pos + n > fade_start {
            for (i, frame) in out.iter_mut().enumerate() {
                let p = self.pos + i;
                if p >= fade_start {
                    let gain = (self.len - p) as f32 / self.fade as f32;
                    frame[0] *= gain;
                    frame[1] *= gain;
                }
            }
        }

        self.pos += n;
        n
    }

    fn reset(&mut self) {
        self.child.reset();
        self.child_done = false;
        self.pos = 0;
    }
}

/// Plays its child `times` times back to back, by rewinding it each time it ends.
/// Nothing is pre-rendered: memory use is the same for `repeat(x, 4)` and
/// `repeat(x, 4_000_000)`.
pub struct Repeat {
    child: Box<dyn Node>,
    times: usize,
    done: usize,
    /// Frames produced in the current cycle; guards against spinning forever on an
    /// empty child.
    cycle_frames: usize,
}

impl Repeat {
    pub fn new(child: Box<dyn Node>, times: usize) -> Self {
        Self {
            child,
            times,
            done: 0,
            cycle_frames: 0,
        }
    }
}

impl Node for Repeat {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let mut written = 0;
        while written < out.len() && self.done < self.times {
            let n = self.child.process(&mut out[written..]);
            written += n;
            self.cycle_frames += n;
            if written < out.len() {
                // Child finished mid-buffer: start the next cycle right here, so
                // the repeats are sample-accurate.
                self.done += 1;
                if self.cycle_frames == 0 {
                    self.done = self.times;
                }
                self.cycle_frames = 0;
                self.child.reset();
            }
        }
        written
    }

    fn reset(&mut self) {
        self.child.reset();
        self.done = 0;
        self.cycle_frames = 0;
    }
}

/// Size of the scratch buffers nodes allocate up front. Blocks bigger than this
/// are processed in pieces.
const SCRATCH_FRAMES: usize = 1024;

/// Mixes any number of children. Finishes when the longest child finishes.
pub struct Add {
    children: Vec<Box<dyn Node>>,
    done: Vec<bool>,
    scratch: Vec<Frame>,
}

impl Add {
    pub fn new(children: Vec<Box<dyn Node>>) -> Self {
        let done = vec![false; children.len()];
        Self {
            children,
            done,
            scratch: vec![[0.0; 2]; SCRATCH_FRAMES],
        }
    }
}

impl Node for Add {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        out.fill([0.0; 2]);
        let mut written = 0;
        for (start, chunk) in (0..)
            .step_by(SCRATCH_FRAMES)
            .zip(out.chunks_mut(SCRATCH_FRAMES))
        {
            let mut longest = 0;
            for (child, done) in self.children.iter_mut().zip(&mut self.done) {
                if *done {
                    continue;
                }
                let scratch = &mut self.scratch[..chunk.len()];
                let n = child.process(scratch);
                for (o, s) in chunk.iter_mut().zip(&scratch[..n]) {
                    o[0] += s[0];
                    o[1] += s[1];
                }
                *done = n < chunk.len();
                longest = longest.max(n);
            }
            written = start + longest;
            if longest < chunk.len() {
                break;
            }
        }
        written
    }

    fn reset(&mut self) {
        self.children.iter_mut().for_each(|c| c.reset());
        self.done.fill(false);
    }
}

/// Plays its children one after another, sample-accurately.
pub struct Seq {
    children: Vec<Box<dyn Node>>,
    current: usize,
}

impl Seq {
    pub fn new(children: Vec<Box<dyn Node>>) -> Self {
        Self {
            children,
            current: 0,
        }
    }
}

impl Node for Seq {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let mut written = 0;
        while written < out.len() && self.current < self.children.len() {
            written += self.children[self.current].process(&mut out[written..]);
            if written < out.len() {
                self.current += 1;
            }
        }
        written
    }

    fn reset(&mut self) {
        self.children.iter_mut().for_each(|c| c.reset());
        self.current = 0;
    }
}

/// Multiplies its child by a constant.
pub struct Gain {
    child: Box<dyn Node>,
    amount: f32,
}

impl Gain {
    pub fn new(amount: f32, child: Box<dyn Node>) -> Self {
        Self { child, amount }
    }
}

impl Node for Gain {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let n = self.child.process(out);
        for frame in &mut out[..n] {
            frame[0] *= self.amount;
            frame[1] *= self.amount;
        }
        n
    }

    fn reset(&mut self) {
        self.child.reset();
    }
}

/// A lookahead peak limiter: keeps the output below `ceiling` without clipping.
///
/// The input is delayed by `lookahead` frames, so the gain can start coming down
/// *before* a peak arrives. Per frame:
///
/// 1. the gain the current frame needs: `min(1, ceiling / peak)`;
/// 2. the minimum of that over the last `lookahead + 1` frames, so a peak is
///    "seen" for the whole time it travels through the delay line;
/// 3. instant attack, exponential release back towards 1;
/// 4. a moving average over `lookahead` frames, which turns the gain drop into a
///    smooth ramp. Every value in that average already accounts for the peak, so
///    the ramp reaches the needed gain exactly when the peak is output.
///
/// Both channels share one gain, so the stereo image doesn't shift. After the
/// child ends, the delay line is flushed, so the output is `lookahead` longer.
pub struct Limit {
    child: Box<dyn Node>,
    child_done: bool,
    ceiling: f32,
    lookahead: usize,
    release: f32,
    delay: VecDeque<Frame>,
    /// (frame index, needed gain), increasing in both: the sliding-window minimum.
    window: VecDeque<(u64, f32)>,
    averaged: VecDeque<f32>,
    sum: f64,
    released: f32,
    tail: usize,
    t: u64,
    scratch: Vec<Frame>,
}

impl Limit {
    /// `release_frames` is the time constant for the gain to recover.
    pub fn new(child: Box<dyn Node>, ceiling: f32, lookahead: usize, release_frames: f32) -> Self {
        let lookahead = lookahead.max(1);
        let mut limit = Self {
            child,
            child_done: false,
            ceiling,
            lookahead,
            release: 1.0 - (-1.0 / release_frames.max(1.0)).exp(),
            delay: VecDeque::with_capacity(lookahead + 1),
            window: VecDeque::with_capacity(lookahead + 2),
            averaged: VecDeque::with_capacity(lookahead + 1),
            sum: 0.0,
            released: 1.0,
            tail: 0,
            t: 0,
            scratch: vec![[0.0; 2]; SCRATCH_FRAMES],
        };
        limit.reset_state();
        limit
    }

    /// Prime the delay line with silence and every filter with unity gain.
    /// (Only touches preallocated capacity, so it's fine on the audio thread.)
    fn reset_state(&mut self) {
        self.child_done = false;
        self.delay.clear();
        self.delay
            .extend(std::iter::repeat_n([0.0; 2], self.lookahead));
        self.window.clear();
        self.averaged.clear();
        self.averaged
            .extend(std::iter::repeat_n(1.0, self.lookahead));
        self.sum = self.lookahead as f64;
        self.released = 1.0;
        self.tail = self.lookahead;
        self.t = 0;
    }

    fn step(&mut self, input: Frame) -> Frame {
        let peak = input[0].abs().max(input[1].abs());
        let needed = if peak > self.ceiling {
            self.ceiling / peak
        } else {
            1.0
        };

        while self.window.back().is_some_and(|&(_, g)| g >= needed) {
            self.window.pop_back();
        }
        self.window.push_back((self.t, needed));
        while self
            .window
            .front()
            .is_some_and(|&(i, _)| i + (self.lookahead as u64) < self.t)
        {
            self.window.pop_front();
        }
        let min = self.window.front().unwrap().1;

        self.released = if min < self.released {
            min
        } else {
            self.released + (min - self.released) * self.release
        };

        self.sum += self.released as f64 - self.averaged.pop_front().unwrap() as f64;
        self.averaged.push_back(self.released);
        let gain = (self.sum / self.lookahead as f64) as f32;

        self.delay.push_back(input);
        let delayed = self.delay.pop_front().unwrap();
        self.t += 1;
        [delayed[0] * gain, delayed[1] * gain]
    }
}

impl Node for Limit {
    fn process(&mut self, out: &mut [Frame]) -> usize {
        let mut written = 0;
        while written < out.len() {
            let len = (out.len() - written).min(SCRATCH_FRAMES);
            let mut n = if self.child_done {
                0
            } else {
                self.child.process(&mut self.scratch[..len])
            };
            if n < len {
                self.child_done = true;
                // Feed silence to push the last `lookahead` frames out.
                let flush = (len - n).min(self.tail);
                self.scratch[n..n + flush].fill([0.0; 2]);
                self.tail -= flush;
                n += flush;
            }
            for i in 0..n {
                out[written + i] = self.step(self.scratch[i]);
            }
            written += n;
            if n < len {
                break;
            }
        }
        written
    }

    fn reset(&mut self) {
        self.child.reset();
        self.reset_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ones(n: usize) -> Box<dyn Node> {
        let data = SampleData {
            frames: vec![[1.0, 1.0]; n],
            sample_rate: 48_000,
        };
        Box::new(Sampler::new(Arc::new(data), 48_000))
    }

    /// Render a node to completion in small, awkward block sizes.
    fn render(node: &mut dyn Node) -> Vec<Frame> {
        let mut all = Vec::new();
        let mut buf = [[0.0; 2]; 7];
        loop {
            let n = node.process(&mut buf);
            all.extend_from_slice(&buf[..n]);
            if n < buf.len() {
                return all;
            }
        }
    }

    #[test]
    fn fit_pads_with_silence() {
        let out = render(&mut Fit::new(ones(10), 25, 0));
        assert_eq!(out.len(), 25);
        assert!(out[..10].iter().all(|f| f[0] == 1.0));
        assert!(out[10..].iter().all(|f| f[0] == 0.0));
    }

    #[test]
    fn fit_clips_and_fades() {
        let out = render(&mut Fit::new(ones(100), 20, 4));
        assert_eq!(out.len(), 20);
        assert_eq!(out[15][0], 1.0);
        let tail: Vec<f32> = out[16..].iter().map(|f| f[0]).collect();
        assert_eq!(tail, [1.0, 0.75, 0.5, 0.25]);
    }

    #[test]
    fn repeat_is_seamless() {
        let out = render(&mut Repeat::new(Box::new(Fit::new(ones(3), 5, 0)), 4));
        let pattern: Vec<f32> = out.iter().map(|f| f[0]).collect();
        assert_eq!(pattern, [1., 1., 1., 0., 0.].repeat(4));
    }

    #[test]
    fn repeat_of_empty_terminates() {
        let out = render(&mut Repeat::new(ones(0), usize::MAX));
        assert!(out.is_empty());
    }

    #[test]
    fn sampler_resamples() {
        let data = SampleData {
            frames: vec![[0.5, 0.5]; 441],
            sample_rate: 44_100,
        };
        let out = render(&mut Sampler::new(Arc::new(data), 48_000));
        assert!((480..=481).contains(&out.len()), "{}", out.len());
    }

    fn constant(value: f32, n: usize) -> Box<dyn Node> {
        let data = SampleData {
            frames: vec![[value, value]; n],
            sample_rate: 48_000,
        };
        Box::new(Sampler::new(Arc::new(data), 48_000))
    }

    #[test]
    fn add_mixes_until_longest_child_ends() {
        let out = render(&mut Add::new(vec![constant(0.25, 10), constant(0.5, 4)]));
        let left: Vec<f32> = out.iter().map(|f| f[0]).collect();
        assert_eq!(left, [[0.75; 4].as_slice(), &[0.25; 6]].concat());
    }

    #[test]
    fn add_of_nothing_is_empty() {
        assert!(render(&mut Add::new(vec![])).is_empty());
    }

    #[test]
    fn seq_plays_children_in_order() {
        let mut seq = Seq::new(vec![constant(0.25, 3), constant(0.5, 0), constant(1.0, 2)]);
        for _ in 0..2 {
            let left: Vec<f32> = render(&mut seq).iter().map(|f| f[0]).collect();
            assert_eq!(left, [0.25, 0.25, 0.25, 1.0, 1.0]);
            seq.reset();
        }
        assert!(render(&mut Seq::new(vec![])).is_empty());
    }

    #[test]
    fn gain_scales() {
        let out = render(&mut Gain::new(0.5, ones(3)));
        assert_eq!(out, vec![[0.5, 0.5]; 3]);
    }

    #[test]
    fn limit_never_exceeds_ceiling() {
        // Quiet, then a loud burst, then quiet again.
        let mut frames = vec![[0.1, -0.1]; 200];
        frames.extend(vec![[2.0, -3.0]; 50]);
        frames.extend(vec![[0.1, -0.1]; 2000]);
        let len = frames.len();
        let child = Box::new(Sampler::new(
            Arc::new(SampleData {
                frames,
                sample_rate: 48_000,
            }),
            48_000,
        ));
        let out = render(&mut Limit::new(child, 0.5, 16, 100.0));

        assert_eq!(out.len(), len + 16, "delay line is flushed");
        let peak = out
            .iter()
            .flat_map(|f| f.iter())
            .fold(0f32, |m, s| m.max(s.abs()));
        assert!(peak <= 0.5 + 1e-6, "peak {peak}");
        // Quiet parts before the burst's lookahead are untouched, and the gain
        // recovers well after it.
        assert!((out[16 + 100][0] - 0.1).abs() < 1e-6);
        assert!((out[len][0] - 0.1).abs() < 1e-3);
    }
}
