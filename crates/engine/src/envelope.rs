//! An envelope: attack, decay, sustain and release, each stage with its own
//! curve, like Ableton's. Stored as `envelopes/<name>.json`.
//!
//! Unlike a modulation, an envelope isn't one stretch of time: attack and decay
//! run from note-on, the sustain level holds for as long as the note lasts
//! (which isn't known until it's played), and the release runs from note-off,
//! starting from wherever the envelope was at that moment.

use serde::{Deserialize, Serialize};

use crate::curve;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Envelope {
    pub attack: Stage,
    pub decay: Stage,
    /// The level held while the note lasts, 0..1.
    pub sustain: f64,
    pub release: Stage,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stage {
    /// In seconds.
    pub time: f64,
    /// See `curve::bend`.
    #[serde(default)]
    pub curve: f64,
}

impl Stage {
    fn new(time: f64, curve: f64) -> Self {
        Self { time, curve }
    }
}

impl Default for Envelope {
    /// A quick attack, then a natural-sounding decay to half level.
    fn default() -> Self {
        Self {
            attack: Stage::new(0.001, 0.0),
            decay: Stage::new(0.6, -0.5),
            sustain: 0.5,
            release: Stage::new(0.6, -0.5),
        }
    }
}

impl Envelope {
    /// The longest a stage can take, in seconds.
    pub const MAX_TIME: f64 = 60.0;

    /// Read a JSON file's contents; `name` is for messages.
    pub fn from_json(name: &str, data: &[u8]) -> Result<Self, String> {
        let mut envelope: Self = curve::from_json(name, data)?;
        envelope.normalize();
        Ok(envelope)
    }

    pub fn to_json(&self) -> Vec<u8> {
        curve::to_json(self)
    }

    /// Clamp everything into range.
    pub fn normalize(&mut self) {
        for stage in [&mut self.attack, &mut self.decay, &mut self.release] {
            stage.time = if stage.time.is_nan() {
                0.0
            } else {
                stage.time.clamp(0.0, Self::MAX_TIME)
            };
            stage.curve = stage.curve.clamp(-1.0, 1.0);
        }
        self.sustain = self.sustain.clamp(0.0, 1.0);
    }

    /// The level `t` seconds after note-on, while the note is held.
    pub fn level_held(&self, t: f64) -> f64 {
        let (a, d) = (self.attack, self.decay);
        if t < a.time {
            curve::segment(0.0, 1.0, t / a.time, a.curve)
        } else if t < a.time + d.time {
            curve::segment(1.0, self.sustain, (t - a.time) / d.time, d.curve)
        } else {
            self.sustain
        }
    }

    /// The level `t` seconds after note-off, released from level `from`.
    pub fn level_released(&self, from: f64, t: f64) -> f64 {
        let r = self.release;
        if t < r.time {
            curve::segment(from, 0.0, t / r.time, r.curve)
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages() {
        let env = Envelope {
            attack: Stage::new(1.0, 0.0),
            decay: Stage::new(2.0, 0.0),
            sustain: 0.25,
            release: Stage::new(1.0, 0.0),
        };
        assert_eq!(env.level_held(0.0), 0.0);
        assert_eq!(env.level_held(0.5), 0.5);
        assert_eq!(env.level_held(1.0), 1.0);
        assert_eq!(env.level_held(2.0), 0.625);
        assert_eq!(env.level_held(100.0), 0.25);
        // Released halfway through the attack: down from where it was.
        assert_eq!(env.level_released(0.5, 0.0), 0.5);
        assert_eq!(env.level_released(0.5, 0.5), 0.25);
        assert_eq!(env.level_released(0.5, 1.0), 0.0);
    }

    #[test]
    fn zero_time_stages_jump() {
        let env = Envelope {
            attack: Stage::new(0.0, 0.0),
            decay: Stage::new(0.0, 0.0),
            sustain: 0.5,
            release: Stage::new(0.0, 0.0),
        };
        assert_eq!(env.level_held(0.0), 0.5);
        assert_eq!(env.level_released(0.5, 0.0), 0.0);
    }

    #[test]
    fn round_trips_through_json() {
        let env = Envelope::default();
        assert_eq!(Envelope::from_json("pluck", &env.to_json()).unwrap(), env);
    }
}
