//! A running audio engine plus the evaluator that feeds it: the one object a
//! frontend (REPL, editor app, ...) needs to hold on to.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};
use rtrb::Producer;

use crate::engine::{Command, Engine, MAX_BLOCK, Status};
use crate::eval::Evaluator;
use crate::lang;
use crate::nodes::{Frame, Node};

pub struct Session {
    pub device_name: String,
    pub sample_rate: u32,
    pub channels: u16,
    commands: Producer<Command>,
    status: Arc<Status>,
    evaluator: Evaluator,
    sent: u64,
    _output: Output,
}

// The fields are only held, never read.
#[allow(dead_code)]
enum Output {
    // Dropping the stream stops audio, so the session owns it.
    Device(cpal::Stream),
    // No device: the engine is kept but never run (see `Session::without_output`).
    None(Engine),
}

impl Session {
    /// Open the default output device and start the audio thread.
    pub fn start(sample_dirs: Vec<PathBuf>) -> Result<Self, String> {
        let host = cpal::default_host();
        let device = host.default_output_device().ok_or("no output device")?;
        let supported = device.default_output_config().map_err(|e| e.to_string())?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let device_name = device
            .description()
            .map_or_else(|_| "unknown device".to_string(), |d| d.to_string());

        // Two lock-free single-producer/single-consumer queues: commands go to the
        // audio thread, finished nodes come back to be freed.
        let (commands, commands_rx) = rtrb::RingBuffer::<Command>::new(256);
        let (garbage_tx, mut garbage) = rtrb::RingBuffer::<Box<dyn Node>>::new(1024);
        let status = Arc::new(Status::default());
        let engine = Engine::new(commands_rx, garbage_tx, status.clone(), config.sample_rate);

        let stream = match format {
            SampleFormat::F32 => build_stream::<f32>(&device, &config, engine),
            SampleFormat::I16 => build_stream::<i16>(&device, &config, engine),
            SampleFormat::I32 => build_stream::<i32>(&device, &config, engine),
            other => return Err(format!("unsupported sample format {other}")),
        }
        .map_err(|e| e.to_string())?;
        stream.play().map_err(|e| e.to_string())?;

        std::thread::spawn(move || {
            loop {
                while let Ok(node) = garbage.pop() {
                    drop(node);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });

        Ok(Self {
            device_name,
            sample_rate: config.sample_rate,
            channels: config.channels,
            commands,
            status,
            evaluator: Evaluator::new(config.sample_rate, sample_dirs),
            sent: 0,
            _output: Output::Device(stream),
        })
    }

    /// A session that evaluates code but plays nothing, for tests. Commands are
    /// queued but never consumed, so don't call `wait_until_idle` on it.
    pub fn without_output(sample_rate: u32, sample_dirs: Vec<PathBuf>) -> Self {
        let (commands, commands_rx) = rtrb::RingBuffer::<Command>::new(256);
        let (garbage_tx, _) = rtrb::RingBuffer::<Box<dyn Node>>::new(1);
        let status = Arc::new(Status::default());
        let engine = Engine::new(commands_rx, garbage_tx, status.clone(), sample_rate);
        Self {
            device_name: "no output".to_string(),
            sample_rate,
            channels: 2,
            commands,
            status,
            evaluator: Evaluator::new(sample_rate, sample_dirs),
            sent: 0,
            _output: Output::None(engine),
        }
    }

    /// Parse and evaluate `src`, sending the resulting commands to the audio thread.
    pub fn eval(&mut self, src: &str) -> Result<(), lang::Error> {
        let program = lang::parse(src)?;
        for cmd in self.evaluator.run(&program)? {
            self.send(cmd);
        }
        Ok(())
    }

    /// Fade out everything that's playing.
    pub fn stop_all(&mut self) {
        self.send(Command::StopAll);
    }

    /// Number of voices currently playing.
    pub fn voices(&self) -> usize {
        self.status.voices.load(Ordering::Relaxed)
    }

    /// Block until every command sent so far has been picked up and all voices
    /// have finished.
    pub fn wait_until_idle(&self) {
        while self.status.commands_received.load(Ordering::Acquire) < self.sent || self.voices() > 0
        {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn send(&mut self, cmd: Command) {
        // The queue only fills up if the audio thread has stalled; dropping the
        // command is better than blocking the UI.
        if self.commands.push(cmd).is_ok() {
            self.sent += 1;
        }
    }
}

fn build_stream<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    mut engine: Engine,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let mut mix: Vec<Frame> = vec![[0.0; 2]; MAX_BLOCK];

    device.build_output_stream(
        *config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            // The device buffer is interleaved with `channels` channels and can be
            // any size, so render in blocks of at most MAX_BLOCK frames.
            for chunk in data.chunks_mut(MAX_BLOCK * channels) {
                let frames = chunk.len() / channels;
                let mix = &mut mix[..frames];
                engine.process(mix);
                for (out, &[l, r]) in chunk.chunks_mut(channels).zip(mix.iter()) {
                    let (l, r) = (l.clamp(-1.0, 1.0), r.clamp(-1.0, 1.0));
                    match out {
                        [mono] => *mono = T::from_sample((l + r) * 0.5),
                        [left, right, rest @ ..] => {
                            *left = T::from_sample(l);
                            *right = T::from_sample(r);
                            rest.fill(T::EQUILIBRIUM);
                        }
                        [] => {}
                    }
                }
            }
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    )
}
