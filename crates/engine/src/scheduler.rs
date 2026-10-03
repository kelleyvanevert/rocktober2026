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

use crate::clock::Clock;
use crate::engine::Slot;
use crate::eval::Sound;
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

    fn lookahead(&self) -> u64 {
        (LOOKAHEAD_SECONDS * self.sample_rate as f64) as u64
    }

    /// The engine slot for a slot name (0 for none).
    pub fn slot(&mut self, name: Option<&str>) -> Slot {
        let Some(name) = name else {
            return 0;
        };
        let next = self.slots.len() as Slot + 1;
        *self.slots.entry(name.to_string()).or_insert(next)
    }

    /// The beat something started now starts on: the first bar line after
    /// twice the lookahead, so it's clear of anything already sent.
    pub fn start_beat(&self, now: u64) -> f64 {
        self.clock.next_bar(now + 2 * self.lookahead())
    }

    pub fn add(&mut self, pattern: Arc<Pattern>, instrument: Sound, slot: Slot, start: f64) {
        self.running.push(Running {
            pattern,
            instrument,
            slot,
            start,
            next: 0,
            end: None,
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
                if let Step::Hit { note, length } = steps[r.next % steps.len()] {
                    let gate = clock.seconds(length as f64 * step);
                    let since = clock.seconds(r.next as f64 * step);
                    let voice = r.instrument.for_note(note, gate, since);
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
    use crate::eval::Evaluator;
    use crate::lang::parse;
    use crate::resource::Resources;

    /// An instrument (a wavetable with a `?note` hole) and a scheduler.
    fn setup() -> (Sound, Scheduler) {
        let mut ev = Evaluator::new(
            48_000,
            Resources {
                root: ".".into(),
                sample_dirs: vec![],
            },
        );
        let src = r#"notes("c4", 1b).play(wavetable("basic", 0, 0, ?note))"#;
        let actions = ev.run(&parse(src).unwrap()).unwrap();
        let Some(crate::eval::Action::Pattern { instrument, .. }) = actions.into_iter().next()
        else {
            panic!()
        };
        (instrument, Scheduler::new(48_000))
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
    fn plays_start_on_the_next_bar_after_the_lookahead() {
        let s = Scheduler::new(48_000);
        assert_eq!(s.start_beat(0), 4.0);
        // Just before bar 2 (beat 8 = frame 192000): too close, so bar 3.
        assert_eq!(s.start_beat(190_000), 12.0);
    }
}
