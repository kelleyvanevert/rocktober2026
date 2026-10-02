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
}

/// Feedback from the audio thread, readable from anywhere.
#[derive(Default)]
pub struct Status {
    pub voices: AtomicUsize,
    pub commands_received: AtomicU64,
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
