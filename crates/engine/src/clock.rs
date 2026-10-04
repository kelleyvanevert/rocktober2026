//! Musical time: tempo, beats and bars, mapped onto the audio thread's frame
//! counter.

/// Beats per bar. Everything is in 4/4 for now.
pub const BEATS_PER_BAR: f64 = 4.0;

/// Where something may start: every `every` beats, shifted by `offset` beats
/// (`at 5b + 2` is beats 2, 7, 12, ...). Counted from beat 0, so everything
/// on the same grid stays in step, however late it's started.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    pub every: f64,
    pub offset: f64,
}

impl Grid {
    pub fn new(every: f64) -> Self {
        Self { every, offset: 0.0 }
    }

    /// The first beat on the grid at or after `beat`.
    pub fn next(&self, beat: f64) -> f64 {
        let offset = self.offset.rem_euclid(self.every);
        ((beat - offset) / self.every - 1e-9).ceil() * self.every + offset
    }
}

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beats_and_frames() {
        let mut clock = Clock::new(48_000);
        assert_eq!(clock.beat_at(24_000), 1.0);
        assert_eq!(clock.frame_at(4.0), 96_000);
        // Twice as fast from beat 4 on.
        clock.set_bpm(240.0, 96_000);
        assert_eq!(clock.beat_at(96_000 + 12_000), 5.0);
        assert_eq!(clock.frame_at(8.0), 96_000 + 48_000);
        assert_eq!(clock.seconds(2.0), 0.5);
    }

    #[test]
    fn grids() {
        let bar = Grid::new(4.0);
        assert_eq!(bar.next(0.0), 0.0);
        assert_eq!(bar.next(0.1), 4.0);
        assert_eq!(bar.next(4.0), 4.0);
        let fives = Grid {
            every: 5.0,
            offset: 2.0,
        };
        assert_eq!(fives.next(0.0), 2.0);
        assert_eq!(fives.next(2.5), 7.0);
        assert_eq!(fives.next(12.0), 12.0);
        // An offset past the grid wraps around.
        let late = Grid {
            every: 4.0,
            offset: 6.0,
        };
        assert_eq!(late.next(0.5), 2.0);
    }
}
