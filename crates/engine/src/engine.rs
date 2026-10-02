//! The audio-thread side: receives commands, mixes the playing voices.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rtrb::{Consumer, Producer};

use crate::nodes::{Frame, Node};

/// The largest block `Engine::process` handles; the audio callback splits bigger
/// device buffers into chunks of this size.
pub const MAX_BLOCK: usize = 1024;
const MAX_VOICES: usize = 256;

/// Messages from the application thread to the audio thread.
pub enum Command {
    Play(Box<dyn Node>),
    StopAll,
    /// Copy the output (interleaved stereo) into this queue until told to stop.
    StartRecording(Producer<f32>),
    StopRecording,
}

/// Feedback from the audio thread, readable from anywhere.
#[derive(Default)]
pub struct Status {
    pub voices: AtomicUsize,
    pub commands_received: AtomicU64,
    /// Samples the recorder couldn't keep up with (should stay 0).
    pub recording_dropped: AtomicU64,
}

struct Voice {
    node: Box<dyn Node>,
    /// Frames left in a stop fade-out, if the voice is being stopped.
    stopping: Option<usize>,
}

pub struct Engine {
    commands: Consumer<Command>,
    /// Finished nodes go back to the app thread to be freed there: `free()` can
    /// take a lock inside the allocator, which we never want on the audio thread.
    garbage: Producer<Box<dyn Node>>,
    status: Arc<Status>,
    voices: Vec<Voice>,
    scratch: Vec<Frame>,
    stop_fade: usize,
    recorder: Option<Producer<f32>>,
}

impl Engine {
    pub fn new(
        commands: Consumer<Command>,
        garbage: Producer<Box<dyn Node>>,
        status: Arc<Status>,
        sample_rate: u32,
    ) -> Self {
        Self {
            commands,
            garbage,
            status,
            // Allocated up front, so the audio thread never has to grow them.
            voices: Vec::with_capacity(MAX_VOICES),
            scratch: vec![[0.0; 2]; MAX_BLOCK],
            stop_fade: (sample_rate as usize / 100).max(1), // 10 ms
            recorder: None,
        }
    }

    /// Render one block (at most `MAX_BLOCK` frames) into `out`.
    pub fn process(&mut self, out: &mut [Frame]) {
        let mut received = 0;
        while let Ok(cmd) = self.commands.pop() {
            received += 1;
            match cmd {
                Command::Play(node) => {
                    if self.voices.len() < MAX_VOICES {
                        self.voices.push(Voice {
                            node,
                            stopping: None,
                        });
                    } else {
                        self.retire(node);
                    }
                }
                Command::StopAll => {
                    for v in &mut self.voices {
                        v.stopping.get_or_insert(self.stop_fade);
                    }
                }
                Command::StartRecording(producer) => self.recorder = Some(producer),
                // Dropping our end tells the writer thread to finish the file.
                Command::StopRecording => self.recorder = None,
            }
        }

        out.fill([0.0; 2]);
        let len = out.len();
        let mut i = 0;
        while i < self.voices.len() {
            let voice = &mut self.voices[i];
            let scratch = &mut self.scratch[..len];
            let n = voice.node.process(scratch);
            let mut finished = n < len;
            for (o, s) in out.iter_mut().zip(&scratch[..n]) {
                let gain = match &mut voice.stopping {
                    None => 1.0,
                    Some(0) => {
                        finished = true;
                        break;
                    }
                    Some(left) => {
                        *left -= 1;
                        *left as f32 / self.stop_fade as f32
                    }
                };
                o[0] += s[0] * gain;
                o[1] += s[1] * gain;
            }
            if finished {
                let voice = self.voices.swap_remove(i);
                self.retire(voice.node);
            } else {
                i += 1;
            }
        }

        // Clip here rather than in the device callback, so recordings get
        // exactly what's heard.
        for frame in out.iter_mut() {
            frame[0] = frame[0].clamp(-1.0, 1.0);
            frame[1] = frame[1].clamp(-1.0, 1.0);
        }
        if let Some(recorder) = &mut self.recorder {
            let mut dropped = 0;
            for &[l, r] in out.iter() {
                if recorder.slots() < 2 {
                    dropped += 2;
                    continue;
                }
                let _ = recorder.push(l);
                let _ = recorder.push(r);
            }
            if dropped > 0 {
                self.status
                    .recording_dropped
                    .fetch_add(dropped, Ordering::Relaxed);
            }
        }

        // Store voices before commands_received, so a reader that sees the latest
        // command count also sees a voice count that includes it.
        self.status
            .voices
            .store(self.voices.len(), Ordering::Relaxed);
        self.status
            .commands_received
            .fetch_add(received, Ordering::Release);
    }

    fn retire(&mut self, node: Box<dyn Node>) {
        // If the garbage queue is full, dropping here is the lesser evil.
        let _ = self.garbage.push(node);
    }
}
