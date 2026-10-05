//! Patterns: rhythms of notes, written as a string of steps.
//!
//! ```text
//! notes("c2 e2 _ g2 . x", 0.25b)
//! ```
//!
//! Each step lasts the same number of beats: a note (`c2`, `f#3`), `x` (a hit
//! without a pitch, for drums), `.` or `~` (a rest), or `_` (the previous note
//! holds on through this step too). A pattern loops.
//!
//! Played with a sound that glides `:legato` (`sound:note(glide(?note):legato)`),
//! notes that follow each other without a rest are one voice, sliding from
//! note to note, and a rest ends the phrase, so the next note starts a new
//! voice (with its envelope's attack). Like a mono synth with glide in legato
//! mode.

use crate::lang;

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Rest,
    /// A note (or a hit without one), held for `length` steps.
    Hit {
        note: Option<f64>,
        length: usize,
    },
    /// The previous hit holding on.
    Tie,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Pattern {
    pub steps: Vec<Step>,
    /// The length of one step, in beats.
    pub step: f64,
    /// How many times it plays (`usize::MAX`: forever).
    pub times: usize,
}

impl Pattern {
    /// Parse the steps; errors say which step is wrong.
    pub fn parse(text: &str, step: f64) -> Result<Self, String> {
        let mut steps: Vec<Step> = Vec::new();
        let mut last_hit: Option<usize> = None;
        for word in text.split_whitespace() {
            let parsed = match word {
                "." | "~" => {
                    last_hit = None;
                    Step::Rest
                }
                "_" => match last_hit {
                    Some(i) => {
                        if let Step::Hit { length, .. } = &mut steps[i] {
                            *length += 1;
                        }
                        Step::Tie
                    }
                    None => Step::Rest,
                },
                "x" => Step::Hit {
                    note: None,
                    length: 1,
                },
                word => match lang::note(word) {
                    Some(note) => Step::Hit {
                        note: Some(note),
                        length: 1,
                    },
                    None => {
                        return Err(format!(
                            "'{word}' isn't a step (use notes like c2 or f#3, x, . or _)"
                        ));
                    }
                },
            };
            if matches!(parsed, Step::Hit { .. }) {
                last_hit = Some(steps.len());
            }
            steps.push(parsed);
        }
        if steps.is_empty() {
            return Err("a pattern needs at least one step".into());
        }
        Ok(Self {
            steps,
            step,
            times: usize::MAX,
        })
    }

    /// The length of one pass, in beats.
    pub fn beats(&self) -> f64 {
        self.steps.len() as f64 * self.step
    }

    pub fn has_notes(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s, Step::Hit { note: Some(_), .. }))
    }

    /// For a legato pattern: the phrase starting at (absolute) step `at`,
    /// counting across passes, if a phrase starts there. A phrase is a run
    /// of hits and ties up to the next rest, and its length is `None` if it
    /// never ends (no rests, playing forever). Within a phrase, hits don't
    /// start voices of their own.
    pub fn phrase(&self, at: usize) -> Option<Option<usize>> {
        let len = self.steps.len();
        let total = self.times.saturating_mul(len);
        let is_rest = |k: usize| matches!(self.steps[k % len], Step::Rest);
        if is_rest(at) || (at > 0 && !is_rest(at - 1)) {
            return None;
        }
        if !self.steps.iter().any(|s| matches!(s, Step::Rest)) {
            return Some((total != usize::MAX).then(|| total - at));
        }
        let end = (at..total).find(|&k| is_rest(k)).unwrap_or(total);
        Some(Some(end - at))
    }

    pub fn has_unpitched_hits(&self) -> bool {
        self.steps
            .iter()
            .any(|s| matches!(s, Step::Hit { note: None, .. }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps() {
        let p = Pattern::parse("c4 _ _ . x ~ _ e4", 0.25).unwrap();
        let hit = |note, length| Step::Hit { note, length };
        assert_eq!(
            p.steps,
            [
                hit(Some(60.0), 3),
                Step::Tie,
                Step::Tie,
                Step::Rest,
                hit(None, 1),
                Step::Rest,
                Step::Rest,
                hit(Some(64.0), 1),
            ]
        );
        assert_eq!(p.beats(), 2.0);
        assert!(p.has_notes() && p.has_unpitched_hits());
        assert_eq!(
            Pattern::parse("c4 q2", 1.0).unwrap_err(),
            "'q2' isn't a step (use notes like c2 or f#3, x, . or _)"
        );
        assert!(Pattern::parse("  ", 1.0).is_err());
    }

    #[test]
    fn phrases() {
        let p = Pattern::parse("c4 e4 _ . g4 x", 0.25).unwrap();
        assert_eq!(p.phrase(0), Some(Some(3)));
        assert_eq!(p.phrase(1), None, "inside a phrase");
        assert_eq!(p.phrase(3), None, "a rest");
        // Runs on into the next pass, up to its rest.
        assert_eq!(p.phrase(4), Some(Some(5)));
        let mut once = Pattern::parse("e4 . c4", 0.25).unwrap();
        assert_eq!(once.phrase(2), Some(Some(2)));
        // ...unless there's no next pass.
        once.times = 1;
        assert_eq!(once.phrase(2), Some(Some(1)));
        let drone = Pattern::parse("c4 _ e4", 0.25).unwrap();
        assert_eq!(drone.phrase(0), Some(None));
        assert_eq!(drone.phrase(3), None);
    }
}
