//! Audio nodes: the stateful objects that actually produce sound.
//!
//! Everything in here runs on the audio thread, so `process` must never allocate,
//! lock a mutex, print, or touch the filesystem. Nodes are built on the application
//! thread and handed to the audio thread fully constructed.

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
}
