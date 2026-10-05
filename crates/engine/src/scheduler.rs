//! The application-thread half of timing: musical time, and the patterns that
//! are playing.
//!
//! The audio thread only knows frames. This side decides which frame things
//! happen on: it turns beats into frames with the `Clock`, and every few
//! milliseconds (see `Session`) it builds the voices for the notes that start
//! within the next `LOOKAHEAD_SECONDS` and sends them ahead as timed commands.
//! The lookahead absorbs any hiccups on this side; the audio thread then starts
//! each voice on exactly its frame. (SuperCollider, Tidal and Strudel all work
//! like this.)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::clock::{Clock, Grid};
use crate::desc::{Control, Sound};
use crate::engine::Slot;
use crate::nodes::Node;
use crate::pattern::{Pattern, Step};

/// How far ahead notes are sent to the audio thread.
pub const LOOKAHEAD_SECONDS: f64 = 0.1;
/// How often the scheduler looks for notes to send.
pub const TICK: Duration = Duration::from_millis(10);

/// A pattern that's playing.
struct Running {
    pattern: Arc<Pattern>,
    instrument: Sound,
    slot: Slot,
    /// The beat it started on.
    start: f64,
    /// The next step to send, counting from the start (so across passes).
    next: usize,
    /// The beat it stops at, once it's been replaced.
    end: Option<f64>,
    /// Whether it plays a voice per phrase (see `Sound::is_legato`).
    legato: bool,
    /// The last note it played, for a glide to slide in from.
    last: Option<f64>,
}

/// A voice to start at frame `at`.
pub struct Event {
    pub at: u64,
    pub node: Box<dyn Node>,
    pub slot: Slot,
}

pub struct Scheduler {
    pub clock: Clock,
    sample_rate: u32,
    running: Vec<Running>,
    slots: HashMap<String, Slot>,
}

impl Scheduler {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            clock: Clock::new(sample_rate),
            sample_rate,
            running: Vec::new(),
            slots: HashMap::new(),
        }
    }

    /// The engine slot for a slot name (0 for none).
    pub fn slot(&mut self, name: Option<&str>) -> Slot {
        let Some(name) = name else {
            return 0;
        };
        let next = self.slots.len() as Slot + 1;
        *self.slots.entry(name.to_string()).or_insert(next)
    }

    /// The beat something starts on if it can start at frame `earliest`:
    /// right then, or on the next point of its grid.
    pub fn start_beat(&self, earliest: u64, at: Option<Grid>) -> f64 {
        let beat = self.clock.beat_at(earliest);
        at.map_or(beat, |grid| grid.next(beat))
    }

    pub fn add(&mut self, pattern: Arc<Pattern>, instrument: Sound, slot: Slot, start: f64) {
        self.running.push(Running {
            legato: instrument.is_legato(),
            pattern,
            instrument,
            slot,
            start,
            next: 0,
            end: None,
            last: None,
        });
    }

    /// Stop the patterns in a slot from `beat` on.
    pub fn end_slot(&mut self, slot: Slot, beat: f64) {
        for r in self.running.iter_mut().filter(|r| r.slot == slot) {
            r.end = Some(r.end.map_or(beat, |end| end.min(beat)));
        }
    }

    pub fn clear(&mut self) {
        self.running.clear();
    }

    pub fn patterns(&self) -> usize {
        self.running.len()
    }

    /// The voices for every note that starts before frame `horizon` and
    /// hasn't been sent yet.
    pub fn due(&mut self, horizon: u64) -> Vec<Event> {
        let mut events = Vec::new();
        let clock = &self.clock;
        for r in &mut self.running {
            let steps = &r.pattern.steps;
            let step = r.pattern.step;
            loop {
                if r.next / steps.len() >= r.pattern.times {
                    r.end = Some(f64::NEG_INFINITY);
                    break;
                }
                let beat = r.start + r.next as f64 * step;
                if r.end.is_some_and(|end| beat >= end) {
                    r.end = Some(f64::NEG_INFINITY);
                    break;
                }
                let at = clock.frame_at(beat);
                if at >= horizon {
                    break;
                }
                let since = clock.seconds(r.next as f64 * step);
                let voice = match (r.legato, &steps[r.next % steps.len()]) {
                    // Legato: one voice per phrase, its notes changing along
                    // it (and the glide sliding between them).
                    (true, _) => r.pattern.phrase(r.next).map(|length| {
                        let path = Control::NotePath {
                            pattern: r.pattern.clone(),
                            start: r.next,
                            step: clock.seconds(step),
                        };
                        let note = r.pattern.has_notes().then_some(path);
                        let gate = length.map(|n| clock.seconds(n as f64 * step));
                        r.instrument.for_note(note, gate, since, None)
                    }),
                    (false, Step::Hit { note, length }) => {
                        let gate = clock.seconds(*length as f64 * step);
                        let voice = r.instrument.for_note(
                            note.map(Control::Constant),
                            Some(gate),
                            since,
                            r.last,
                        );
                        r.last = note.or(r.last);
                        Some(voice)
                    }
                    (false, _) => None,
                };
                if let Some(voice) = voice {
                    events.push(Event {
                        at,
                        node: voice.instantiate(self.sample_rate),
                        slot: r.slot,
                    });
                }
                r.next += 1;
            }
        }
        // Finished ones are marked with an end of -inf.
        self.running
            .retain(|r| r.end.is_none_or(|end| end > f64::NEG_INFINITY));
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::Bundle;
    use crate::eval::Evaluator;
    use crate::lang::parse;

    /// An instrument (a wavetable, its note free) and a scheduler.
    fn setup() -> (Sound, Scheduler) {
        setup_with("wavetable")
    }

    fn setup_with(instrument: &str) -> (Sound, Scheduler) {
        let mut ev = Evaluator::new(48_000, Bundle::default());
        let src = format!(r#"notes("c4", 1b).play({instrument})"#);
        let actions = ev.run(&parse(&src).unwrap()).unwrap();
        let Some(crate::eval::Action::Pattern { instrument, .. }) = actions.into_iter().next()
        else {
            panic!()
        };
        (instrument, Scheduler::new(48_000))
    }

    /// Upward zero crossings in `frames`.
    fn crossings(frames: &[[f32; 2]]) -> usize {
        frames
            .windows(2)
            .filter(|w| w[0][0] < 0.0 && w[1][0] >= 0.0)
            .count()
    }

    fn pattern(steps: &str) -> Arc<Pattern> {
        Arc::new(Pattern::parse(steps, 0.5).unwrap())
    }

    #[test]
    fn notes_are_sent_ahead_on_their_frames() {
        let (lead, mut s) = setup();
        // At 120 bpm a beat is 24000 frames, so half-beat steps are 12000.
        s.add(pattern("c4 . e4 _"), lead, 0, 4.0);
        assert!(s.due(96_000).is_empty());
        let at: Vec<u64> = s.due(136_000).iter().map(|e| e.at).collect();
        assert_eq!(at, [96_000, 120_000]);
        // The next pass.
        let at: Vec<u64> = s.due(190_000).iter().map(|e| e.at).collect();
        assert_eq!(at, [144_000, 168_000]);
    }

    #[test]
    fn notes_end_at_their_gate() {
        let (lead, mut s) = setup();
        s.add(pattern("c4 e4 _ ."), lead, 0, 0.0);
        let mut events = s.due(48_000);
        assert_eq!(events.len(), 2);
        // The wavetable never ends by itself, so the first note is cut off
        // after its one step (12000 frames), the tied one after two.
        let mut lengths = events.iter_mut().map(|e| {
            let mut buf = vec![[0.0; 2]; 100_000];
            e.node.process(&mut buf)
        });
        assert_eq!(lengths.next(), Some(12_000));
        assert_eq!(lengths.next(), Some(24_000));
    }

    #[test]
    fn replaced_and_finished_patterns_stop() {
        let (lead, mut s) = setup();
        let slot = s.slot(Some("bass"));
        assert_eq!(slot, s.slot(Some("bass")));
        assert_ne!(slot, s.slot(Some("drums")));
        s.add(pattern("c4 c4 c4 c4"), lead.clone(), slot, 0.0);
        s.end_slot(slot, 1.0);
        assert_eq!(s.due(1_000_000).len(), 2);
        assert_eq!(s.patterns(), 0);
        let mut once = (*pattern("c4 c4")).clone();
        once.times = 2;
        s.add(Arc::new(once), lead, 0, 0.0);
        assert_eq!(s.due(1_000_000).len(), 4);
        assert_eq!(s.patterns(), 0);
    }

    #[test]
    fn plays_start_right_away_or_on_their_grid() {
        let s = Scheduler::new(48_000);
        // 120 bpm: a beat is 24000 frames.
        assert_eq!(s.start_beat(36_000, None), 1.5);
        assert_eq!(s.start_beat(36_000, Some(Grid::new(4.0))), 4.0);
        assert_eq!(s.start_beat(0, Some(Grid::new(4.0))), 0.0);
        let fives = Grid {
            every: 5.0,
            offset: 2.0,
        };
        assert_eq!(s.start_beat(24_000 * 3, Some(fives)), 7.0);
    }

    #[test]
    fn legato_glides_play_a_voice_per_phrase() {
        let (lead, mut s) = setup_with("wavetable:note(glide(?note):dur(50ms):legato)");
        let mut legato = (*pattern("c4 e4 _ . g4 . c4 c4")).clone();
        legato.times = 1;
        s.add(Arc::new(legato), lead, 0, 0.0);
        let mut events = s.due(1_000_000);
        let at: Vec<u64> = events.iter().map(|e| e.at).collect();
        assert_eq!(at, [0, 48_000, 72_000]);
        // The first phrase lasts its three steps, gliding from c4 up to e4.
        let mut buf = vec![[0.0; 2]; 100_000];
        assert_eq!(events[0].node.process(&mut buf), 36_000);
        // c4 is 261.6 Hz, e4 329.6: a quarter second of each.
        assert!((64..=67).contains(&crossings(&buf[0..12_000])));
        assert!((81..=84).contains(&crossings(&buf[18_000..30_000])));
    }

    #[test]
    fn glides_slide_each_note_in_from_the_last() {
        let (lead, mut s) = setup_with("wavetable:note(glide(?note):dur(250ms))");
        s.add(pattern("c4 c5"), lead, 0, 0.0);
        let mut events = s.due(20_000);
        assert_eq!(events.len(), 2, "a voice per note");
        let mut first = vec![[0.0; 2]; 12_000];
        let mut second = vec![[0.0; 2]; 12_000];
        events[0].node.process(&mut first);
        events[1].node.process(&mut second);
        // The first note has none before it: c4 all along (261.6 Hz, so 65
        // cycles in a quarter second). The second slides up an octave from
        // c4 over its quarter second, evenly in pitch: 1 / ln 2 times as many.
        assert!((64..=66).contains(&crossings(&first)));
        assert!(
            (92..=97).contains(&crossings(&second)),
            "{}",
            crossings(&second)
        );
    }
}
