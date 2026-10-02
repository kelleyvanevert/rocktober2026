//! Writing the output to a WAV file as it plays (on its own thread).

use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rtrb::{Consumer, Producer, RingBuffer};

/// Seconds of audio the queue between the audio thread and the writer can hold,
/// in case the disk stalls for a moment.
const BUFFER_SECONDS: usize = 4;
/// How often the WAV header is brought up to date, so that a crash loses at
/// most this much.
const FLUSH_EVERY: Duration = Duration::from_secs(1);

pub struct Recording {
    pub path: PathBuf,
    pub started: Instant,
    writer: JoinHandle<Result<(), String>>,
}

impl Recording {
    /// Create the file and start the writer thread. Returns the producer the
    /// audio thread should push interleaved stereo samples into; dropping it
    /// ends the recording.
    pub fn start(path: &Path, sample_rate: u32) -> Result<(Self, Producer<f32>), String> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let wav =
            hound::WavWriter::create(path, spec).map_err(|e| format!("{}: {e}", path.display()))?;
        let (producer, consumer) = RingBuffer::new(sample_rate as usize * 2 * BUFFER_SECONDS);
        let writer = std::thread::spawn(move || write(wav, consumer));
        let recording = Self {
            path: path.to_path_buf(),
            started: Instant::now(),
            writer,
        };
        Ok((recording, producer))
    }

    pub fn is_finished(&self) -> bool {
        self.writer.is_finished()
    }

    /// Wait for the writer to finish the file (after the producer was dropped).
    pub fn finish(self) -> Result<PathBuf, String> {
        match self.writer.join() {
            Ok(Ok(())) => Ok(self.path),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("recording thread panicked".into()),
        }
    }
}

fn write(
    mut wav: hound::WavWriter<std::io::BufWriter<std::fs::File>>,
    mut samples: Consumer<f32>,
) -> Result<(), String> {
    let mut last_flush = Instant::now();
    loop {
        // Check before draining: once abandoned, nothing new arrives, so after
        // this drain we're done.
        let finished = samples.is_abandoned();
        if let Ok(chunk) = samples.read_chunk(samples.slots()) {
            let (a, b) = chunk.as_slices();
            for &sample in a.iter().chain(b) {
                wav.write_sample(sample).map_err(|e| e.to_string())?;
            }
            chunk.commit_all();
        }
        if finished {
            return wav.finalize().map_err(|e| e.to_string());
        }
        if last_flush.elapsed() >= FLUSH_EVERY {
            wav.flush().map_err(|e| e.to_string())?;
            last_flush = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_samples_to_a_valid_wav() {
        let dir = std::env::temp_dir().join(format!("rocktober-test-{}", std::process::id()));
        let path = dir.join("sub/take.wav");
        let (recording, mut producer) = Recording::start(&path, 48_000).unwrap();
        for i in 0..96_000 {
            while producer.push(i as f32 / 96_000.0).is_err() {
                std::thread::yield_now();
            }
        }
        drop(producer);
        let path = recording.finish().unwrap();

        let mut reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 2);
        assert_eq!(reader.duration(), 48_000);
        let samples: Vec<f32> = reader.samples::<f32>().map(Result::unwrap).collect();
        assert_eq!(samples[1], 1.0 / 96_000.0);
        assert_eq!(samples.len(), 96_000);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
