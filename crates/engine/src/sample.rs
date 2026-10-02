//! Decoding audio files into memory (application thread only).

use std::fs::File;
use std::path::Path;

use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

use crate::nodes::{Frame, SampleData};

/// Decode a whole file to stereo f32 frames. Mono is duplicated to both sides;
/// beyond two channels, only the first two are kept.
pub fn load(path: &Path) -> Result<SampleData, String> {
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
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or_else(|| fail(&"no audio codec parameters"))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(params, &AudioDecoderOptions::default())
        .map_err(|e| fail(&e))?;

    let mut frames: Vec<Frame> = Vec::new();
    let mut sample_rate = 0;
    let mut interleaved: Vec<f32> = Vec::new();

    while let Some(packet) = format.next_packet().map_err(|e| fail(&e))? {
        if packet.track_id != track_id {
            continue;
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
        frames.extend(interleaved.chunks_exact(channels).map(|f| match f {
            [mono] => [*mono, *mono],
            [l, r, ..] => [*l, *r],
            [] => unreachable!(),
        }));
    }

    if sample_rate == 0 {
        return Err(fail(&"file contains no audio"));
    }
    Ok(SampleData {
        frames,
        sample_rate,
    })
}
