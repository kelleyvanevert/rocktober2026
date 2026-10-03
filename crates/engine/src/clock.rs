//! Musical time: tempo, beats and bars, mapped onto the audio thread's frame
//! counter.

/// Beats per bar. Everything is in 4/4 for now.
pub const BEATS_PER_BAR: f64 = 4.0;

#[derive(Clone, Debug)]
pub struct Clock {
    sample_rate: f64,
    bpm: f64,
    /// The tempo last changed at this frame, which was this beat.
    origin_frame: u64,
    origin_beat: f64,
}

impl Clock {
    pub const DEFAULT_BPM: f64 = 120.0;

    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate: sample_rate as f64,
            bpm: Self::DEFAULT_BPM,
            origin_frame: 0,
            origin_beat: 0.0,
        }
    }

    pub fn bpm(&self) -> f64 {
        self.bpm
    }

    /// Change the tempo from `frame` on.
    pub fn set_bpm(&mut self, bpm: f64, frame: u64) {
        self.origin_beat = self.beat_at(frame);
        self.origin_frame = frame;
        self.bpm = bpm;
    }

    pub fn beat_at(&self, frame: u64) -> f64 {
        let seconds = (frame as f64 - self.origin_frame as f64) / self.sample_rate;
        self.origin_beat + seconds * self.bpm / 60.0
    }

    /// The frame a beat falls on (at the current tempo).
    pub fn frame_at(&self, beat: f64) -> u64 {
        let seconds = (beat - self.origin_beat) * 60.0 / self.bpm;
        (self.origin_frame as f64 + seconds * self.sample_rate)
            .round()
            .max(0.0) as u64
    }

    /// How long a number of beats takes, at the current tempo.
    pub fn seconds(&self, beats: f64) -> f64 {
        beats * 60.0 / self.bpm
    }

    /// The first bar line at or after `frame`.
    pub fn next_bar(&self, frame: u64) -> f64 {
        (self.beat_at(frame) / BEATS_PER_BAR - 1e-9).ceil() * BEATS_PER_BAR
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beats_and_frames() {
        let mut clock = Clock::new(48_000);
        assert_eq!(clock.beat_at(24_000), 1.0);
        assert_eq!(clock.frame_at(4.0), 96_000);
        assert_eq!(clock.next_bar(1), 4.0);
        assert_eq!(clock.next_bar(96_000), 4.0);
        // Twice as fast from beat 4 on.
        clock.set_bpm(240.0, 96_000);
        assert_eq!(clock.beat_at(96_000 + 12_000), 5.0);
        assert_eq!(clock.frame_at(8.0), 96_000 + 48_000);
        assert_eq!(clock.seconds(2.0), 0.5);
    }
}
