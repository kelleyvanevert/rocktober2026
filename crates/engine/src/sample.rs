//! Decoding audio files into memory (application thread only).

use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::Time;

use crate::nodes::{Frame, SampleData};

/// Fade applied where a window cuts into the audio, so the cut doesn't click.
const EDGE_FADE_SECONDS: f64 = 0.003;

/// How far before `start` to seek. Decoders need a few frames to warm up after a
/// seek (MP3 borrows bits from earlier frames, AAC overlaps them), and their
/// first output is silence or garbage; we decode and drop this much first.
const SEEK_PREROLL_SECONDS: f64 = 0.5;

/// Decode a whole file to stereo f32 frames. Mono is duplicated to both sides;
/// beyond two channels, only the first two are kept.
pub fn load(path: &Path) -> Result<SampleData, String> {
    load_range(path, 0.0, None)
}

/// Decode only `start..end` seconds of a file (`end: None` means to the end of
/// the file). The decoder seeks to `start` instead of decoding everything before
/// it, so a short window of a long recording is fast and small. Edges that cut
/// into the audio get a short fade.
pub fn load_range(path: &Path, start: f64, end: Option<f64>) -> Result<SampleData, String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let mut frames: Vec<Frame> = Vec::new();
    let (sample_rate, cut_end) = decode(path, start, end, |frame| frames.push(frame))?;

    if frames.is_empty() {
        return Err(match (start, end) {
            (0.0, None) => fail(&"file contains no audio"),
            _ => fail(&"no audio in that range (is the file shorter?)"),
        });
    }

    let fade = ((EDGE_FADE_SECONDS * sample_rate as f64) as usize).min(frames.len() / 2);
    if start > 0.0 {
        for (i, frame) in frames[..fade].iter_mut().enumerate() {
            let gain = (i + 1) as f32 / fade as f32;
            frame[0] *= gain;
            frame[1] *= gain;
        }
    }
    // Only fade the end if it cut the audio off (i.e. we stopped before the file did).
    if cut_end {
        let len = frames.len();
        for (i, frame) in frames[len - fade..].iter_mut().enumerate() {
            let gain = (fade - i) as f32 / fade as f32;
            frame[0] *= gain;
            frame[1] *= gain;
        }
    }

    Ok(SampleData {
        frames,
        sample_rate,
    })
}

/// Frames per peak in an `Overview`: a little over 1 ms at 48 kHz, fine enough
/// to draw a short sample's transient, while ten minutes are still only 3.6 MB.
pub const OVERVIEW_BLOCK: usize = 64;

/// A whole file summarized for drawing its waveform.
pub struct Overview {
    pub sample_rate: u32,
    /// Length of the file, in frames.
    pub frames: usize,
    /// The lowest and highest sample (of either channel) in each block of
    /// `OVERVIEW_BLOCK` frames.
    pub peaks: Vec<(f32, f32)>,
}

impl Overview {
    pub fn seconds(&self) -> f64 {
        self.frames as f64 / self.sample_rate as f64
    }
}

/// Summarize a file for drawing. It's decoded as a stream, so even an hour-long
/// recording never needs to be in memory as a whole.
pub fn overview(path: &Path) -> Result<Overview, String> {
    let mut peaks = Vec::new();
    let mut frames = 0;
    let empty = (f32::MAX, f32::MIN);
    let mut peak = empty;
    let (sample_rate, _) = decode(path, 0.0, None, |[l, r]| {
        peak = (peak.0.min(l.min(r)), peak.1.max(l.max(r)));
        frames += 1;
        if frames % OVERVIEW_BLOCK == 0 {
            peaks.push(peak);
            peak = empty;
        }
    })?;
    if frames == 0 {
        return Err(format!("{}: file contains no audio", path.display()));
    }
    if frames % OVERVIEW_BLOCK != 0 {
        peaks.push(peak);
    }
    Ok(Overview {
        sample_rate,
        frames,
        peaks,
    })
}

/// Decode `start..end` seconds of a file, handing each stereo frame to `emit`.
/// Returns the sample rate and whether decoding stopped at `end` (rather than
/// at the end of the file). Mono is duplicated to both sides.
fn decode(
    path: &Path,
    start: f64,
    end: Option<f64>,
    mut emit: impl FnMut(Frame),
) -> Result<(u32, bool), String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());

    let file = File::open(path).map_err(|e| fail(&e))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format = symphonia::default::get_probe()
        .probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )
        .map_err(|e| fail(&e))?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or_else(|| fail(&"no audio track"))?;
    let track_id = track.id;
    let time_base = track.time_base;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| fail(&"no audio codec parameters"))?;
    let mut sample_rate = params.sample_rate.unwrap_or(0);
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| fail(&e))?;

    // Seeking needs packet timestamps to know where we landed. If the file can't
    // seek, we decode from the beginning and drop what comes before `start`.
    let seek_to = start - SEEK_PREROLL_SECONDS;
    if seek_to > 0.0
        && time_base.is_some()
        && let Some(time) = Time::try_from_secs_f64(seek_to)
    {
        let to = SeekTo::Time {
            time,
            track_id: Some(track_id),
        };
        if format.seek(SeekMode::Accurate, to).is_ok() {
            decoder.reset();
        }
    }

    let mut interleaved: Vec<f32> = Vec::new();
    // Time (in seconds) of the next decoded packet's first frame.
    let mut t = 0.0;
    // Whether we stopped because of `end`, rather than the file ending.
    let mut cut_end = false;

    while let Some(packet) = format.next_packet().map_err(|e| fail(&e))? {
        if packet.track_id != track_id {
            continue;
        }
        // `pts` includes the encoder-delay frames the decoder trims off the start.
        if let Some(tb) = time_base {
            let ticks = packet.pts.get() + packet.trim_start.get() as i64;
            t = ticks as f64 * tb.numer.get() as f64 / tb.denom.get() as f64;
        }
        if end.is_some_and(|end| t >= end) {
            cut_end = true;
            break;
        }
        let buf = match decoder.decode(&packet) {
            Ok(buf) => buf,
            Err(Error::DecodeError(_)) => continue, // skip a corrupt packet
            Err(e) => return Err(fail(&e)),
        };
        sample_rate = buf.spec().rate();
        let channels = buf.spec().channels().count().max(1);
        interleaved.resize(buf.samples_interleaved(), 0.0);
        buf.copy_to_slice_interleaved(&mut interleaved);

        let rate = sample_rate as f64;
        let decoded = interleaved.len() / channels;
        // Which of this packet's frames fall inside the window.
        let first = ((start - t) * rate).round().clamp(0.0, decoded as f64) as usize;
        let last = match end {
            Some(end) => ((end - t) * rate).round().clamp(0.0, decoded as f64) as usize,
            None => decoded,
        };
        cut_end |= last < decoded;
        if first < last {
            for f in interleaved[first * channels..last * channels].chunks_exact(channels) {
                emit(match f {
                    [mono] => [*mono, *mono],
                    [l, r, ..] => [*l, *r],
                    [] => unreachable!(),
                });
            }
        }
        t += decoded as f64 / rate;
    }
    Ok((sample_rate, cut_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_matches_the_decoded_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../samples/kick.mp3");
        let data = load(&path).unwrap();
        let overview = overview(&path).unwrap();
        assert_eq!(overview.frames, data.frames.len());
        assert_eq!(overview.sample_rate, data.sample_rate);
        assert_eq!(
            overview.peaks.len(),
            data.frames.len().div_ceil(OVERVIEW_BLOCK)
        );
        let lowest = data.frames.iter().flatten().fold(0f32, |m, s| m.min(*s));
        let low = overview.peaks.iter().fold(0f32, |m, p| m.min(p.0));
        assert_eq!(low, lowest);
    }
}
