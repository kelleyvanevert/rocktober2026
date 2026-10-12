// Turns the host's transport into sixteenth-note steps, runs the program on each step, and
// keeps track of note-offs. Knows nothing about nih-plug, so it can be tested on its own.

use crate::lang::{self, Pos, Program, Rng};

/// What the host told us about one audio block.
#[derive(Debug, Clone, Copy)]
pub struct Block {
    pub playing: bool,
    /// Song position at the start of the block, in quarter notes.
    pub pos_beats: f64,
    pub tempo: f64,
    pub sample_rate: f32,
    pub samples: u32,
    pub time_sig: (u32, u32),
    /// Start of the current bar in quarter notes and its 0-based number, if the host knows.
    /// Without these we assume the time signature never changed since the start of the song.
    pub bar: Option<(f64, i32)>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Event {
    /// `ch` is 1-based, `timing` is the sample offset within the block.
    On { timing: u32, ch: u8, note: u8, vel: u8 },
    Off { timing: u32, ch: u8, note: u8 },
}

struct Held {
    off_step: i64,
    ch: u8,
    note: u8,
}

/// More simultaneous notes than this are dropped, so the audio thread never allocates.
const MAX_HELD: usize = 256;

/// If a block doesn't start where the previous one ended (give or take), the playhead jumped.
const JUMP_TOLERANCE_BEATS: f64 = 1e-3;

pub struct Sequencer {
    /// The first step we haven't played yet, or `None` when stopped.
    next_step: Option<i64>,
    /// Where we expect the next block to start, to detect jumps and loops.
    expected_pos: f64,
    held: Vec<Held>,
    rng: Rng,
}

impl Default for Sequencer {
    fn default() -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos() as u64);
        Self { next_step: None, expected_pos: 0.0, held: Vec::with_capacity(MAX_HELD), rng: Rng::new(seed) }
    }
}

impl Sequencer {
    /// Play all steps that start in this block. Returns the position at the start of the block
    /// (for display), or `None` when the transport is stopped.
    pub fn process(&mut self, program: &Program, block: &Block, emit: &mut impl FnMut(Event)) -> Option<Pos> {
        if !block.playing || block.tempo <= 0.0 {
            self.release_all(emit);
            self.next_step = None;
            return None;
        }

        let samples_per_beat = block.sample_rate as f64 * 60.0 / block.tempo;
        let start = block.pos_beats;
        let end = start + block.samples as f64 / samples_per_beat;

        let jumped = (start - self.expected_pos).abs() > JUMP_TOLERANCE_BEATS;
        if self.next_step.is_none() || jumped {
            // Started, looped or relocated: let go of everything and pick up from here.
            self.release_all(emit);
            self.next_step = Some((start * 4.0 - 1e-9).ceil() as i64);
        }
        self.expected_pos = end;

        let mut step = self.next_step.unwrap_or_default();
        while (step as f64) / 4.0 < end {
            let offset = ((step as f64 / 4.0 - start) * samples_per_beat).round();
            let timing = (offset.max(0.0) as u32).min(block.samples.saturating_sub(1));

            // Note-offs first, so a note that ends where the next one starts is retriggered.
            self.held.retain(|h| {
                let done = h.off_step <= step;
                if done {
                    emit(Event::Off { timing, ch: h.ch, note: h.note });
                }
                !done
            });

            let pos = position(step, block);
            let held = &mut self.held;
            lang::run(program, &pos, &mut self.rng, &mut |n| {
                // The same note again while it's still sounding: end the old one first.
                if let Some(i) = held.iter().position(|h| h.ch == n.ch && h.note == n.note) {
                    held.swap_remove(i);
                    emit(Event::Off { timing, ch: n.ch, note: n.note });
                }
                if held.len() < MAX_HELD {
                    held.push(Held { off_step: step + n.dur as i64, ch: n.ch, note: n.note });
                    emit(Event::On { timing, ch: n.ch, note: n.note, vel: n.vel });
                }
            });
            step += 1;
        }
        self.next_step = Some(step);

        Some(position((start * 4.0).floor() as i64, block))
    }

    fn release_all(&mut self, emit: &mut impl FnMut(Event)) {
        for h in self.held.drain(..) {
            emit(Event::Off { timing: 0, ch: h.ch, note: h.note });
        }
    }
}

/// bar.beat.sixteenth for a step, 0-based (Ableton shows 1.1.1 for 0.0.0). A "beat" is the time
/// signature's denominator, so in 6/8 a bar has 6 beats of 2 sixteenths each.
fn position(step: i64, block: &Block) -> Pos {
    let (numerator, denominator) = block.time_sig;
    let steps_per_beat = (16 / denominator.clamp(1, 16)) as i64;
    let steps_per_bar = (numerator.max(1) as i64 * steps_per_beat).max(1);

    let (bar_start_step, bar_number) = match block.bar {
        Some((start_beats, number)) => ((start_beats * 4.0).round() as i64, number as i64),
        None => (0, 0),
    };
    let since_bar = step - bar_start_step;
    let in_bar = since_bar.rem_euclid(steps_per_bar);
    Pos {
        step,
        bar: bar_number + since_bar.div_euclid(steps_per_bar),
        beat: (in_bar / steps_per_beat) as u32,
        sixteenth: (in_bar % steps_per_beat) as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::parse;

    /// 120 bpm at 48 kHz: a sixteenth is 6000 samples. Blocks of 512 don't line up with that.
    fn block(pos_beats: f64) -> Block {
        Block {
            playing: true,
            pos_beats,
            tempo: 120.0,
            sample_rate: 48_000.0,
            samples: 512,
            time_sig: (4, 4),
            bar: None,
        }
    }

    /// Play `beats` quarter notes from `from` in consecutive blocks; events get absolute sample times.
    fn play(seq: &mut Sequencer, program: &Program, from: f64, beats: f64) -> Vec<(u64, Event)> {
        let mut out = Vec::new();
        let mut pos = from;
        let mut sample = 0u64;
        while pos < from + beats {
            let b = block(pos);
            seq.process(program, &b, &mut |e| {
                let t = match e { Event::On { timing, .. } | Event::Off { timing, .. } => timing };
                out.push((sample + t as u64, e));
            });
            sample += b.samples as u64;
            pos += b.samples as f64 / 24_000.0;
        }
        out
    }

    #[test]
    fn steps_land_on_the_right_samples() {
        let program = parse("every 4 { note c3 }").unwrap();
        let events = play(&mut Sequencer::default(), &program, 0.0, 1.9);
        let ons: Vec<u64> = events.iter().filter(|(_, e)| matches!(e, Event::On { .. })).map(|e| e.0).collect();
        assert_eq!(ons, [0, 24_000]); // one per quarter note, a quarter note is 24000 samples
        let offs: Vec<u64> = events.iter().filter(|(_, e)| matches!(e, Event::Off { .. })).map(|e| e.0).collect();
        assert_eq!(offs, [6_000, 30_000]); // dur 1 = one sixteenth later
    }

    #[test]
    fn positions_follow_the_time_signature() {
        let b = Block { time_sig: (6, 8), ..block(0.0) };
        // 6/8: 12 sixteenths per bar, 2 per beat
        assert_eq!(position(0, &b), Pos { step: 0, bar: 0, beat: 0, sixteenth: 0 });
        assert_eq!(position(3, &b), Pos { step: 3, bar: 0, beat: 1, sixteenth: 1 });
        assert_eq!(position(12, &b), Pos { step: 12, bar: 1, beat: 0, sixteenth: 0 });

        // A host-reported bar start wins: bar 4 starts at quarter note 14.
        let b = Block { bar: Some((14.0, 4)), ..block(0.0) };
        assert_eq!(position(14 * 4 + 5, &b), Pos { step: 61, bar: 4, beat: 1, sixteenth: 1 });
    }

    #[test]
    fn jumping_releases_held_notes() {
        let program = parse("every 16 { note c3 dur 8 }").unwrap();
        let mut seq = Sequencer::default();
        let mut events = Vec::new();
        seq.process(&program, &block(0.0), &mut |e| events.push(e));
        assert_eq!(events, [Event::On { timing: 0, ch: 1, note: 60, vel: 100 }]);

        events.clear();
        seq.process(&program, &block(8.5), &mut |e| events.push(e)); // relocate mid-bar
        assert_eq!(events, [Event::Off { timing: 0, ch: 1, note: 60 }]);

        events.clear();
        seq.process(&program, &Block { playing: false, ..block(9.0) }, &mut |e| events.push(e));
        assert!(events.is_empty());
    }

    #[test]
    fn retriggering_a_held_note() {
        let program = parse("every 1 { note c3 dur 4 }").unwrap();
        let events = play(&mut Sequencer::default(), &program, 0.0, 0.5);
        let at_6000: Vec<Event> = events.iter().filter(|e| e.0 == 6_000).map(|e| e.1).collect();
        assert_eq!(
            at_6000,
            [Event::Off { timing: 368, ch: 1, note: 60 }, Event::On { timing: 368, ch: 1, note: 60, vel: 100 }]
        );
    }
}
