mod engine;
mod eval;
mod lang;
mod nodes;
mod sample;

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use engine::{Command, Engine, MAX_BLOCK, Status};
use nodes::{Frame, Node};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device")?;
    let supported = device.default_output_config()?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    let sample_rate = config.sample_rate;
    println!(
        "output: {} ({} Hz, {} ch, {format})",
        device.description()?,
        sample_rate,
        config.channels
    );

    // Two lock-free single-producer/single-consumer queues: commands go to the
    // audio thread, finished nodes come back to be freed.
    let (mut commands, commands_rx) = rtrb::RingBuffer::<Command>::new(256);
    let (garbage_tx, mut garbage) = rtrb::RingBuffer::<Box<dyn Node>>::new(1024);
    let status = Arc::new(Status::default());
    let engine = Engine::new(commands_rx, garbage_tx, status.clone(), sample_rate);

    let stream = match format {
        SampleFormat::F32 => build_stream::<f32>(&device, &config, engine)?,
        SampleFormat::I16 => build_stream::<i16>(&device, &config, engine)?,
        SampleFormat::I32 => build_stream::<i32>(&device, &config, engine)?,
        other => return Err(format!("unsupported sample format {other}").into()),
    };
    stream.play()?;

    std::thread::spawn(move || {
        loop {
            while let Ok(node) = garbage.pop() {
                drop(node);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });

    let mut evaluator = eval::Evaluator::new(
        sample_rate,
        vec![PathBuf::from("."), PathBuf::from("samples")],
    );
    let mut sent: u64 = 0;
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    loop {
        print!("> ");
        std::io::stdout().flush()?;
        let Some(line) = lines.next() else { break };
        let line = line?;

        let result = lang::parse(&line).and_then(|program| evaluator.run(&program));
        match result {
            Ok(cmds) => {
                for cmd in cmds {
                    if commands.push(cmd).is_err() {
                        eprintln!("command queue full, dropped a command");
                    } else {
                        sent += 1;
                    }
                }
            }
            Err(e) => {
                // "> " prompt is two characters wide.
                eprintln!("  {}^", " ".repeat(e.pos));
                eprintln!("error: {e}");
            }
        }
    }

    // End of input (e.g. piped from a file): let whatever is playing finish.
    println!();
    while status.commands_received.load(Ordering::Acquire) < sent
        || status.voices.load(Ordering::Relaxed) > 0
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
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
        config.clone(),
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
