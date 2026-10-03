//! A modulation: one value (0..1) drawn over a fixed length of time, like an
//! Ableton clip envelope. Stored as `modulations/<name>.json`.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::curve::{self, load_json, save_json};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Modulation {
    /// In seconds.
    pub length: f64,
    /// Sorted by position. Before the first point the value is the first
    /// point's, after the last it's the last point's. Never empty.
    pub points: Vec<Point>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    /// Position as a fraction of the length (0..1), so that changing the length
    /// stretches the shape.
    pub at: f64,
    pub value: f64,
    /// How the segment from this point to the next one bends (see `curve::bend`).
    #[serde(default)]
    pub curve: f64,
}

impl Point {
    pub fn new(at: f64, value: f64) -> Self {
        Self {
            at,
            value,
            curve: 0.0,
        }
    }
}

impl Default for Modulation {
    /// A two-second ramp up.
    fn default() -> Self {
        Self {
            length: 2.0,
            points: vec![Point::new(0.0, 0.0), Point::new(1.0, 1.0)],
        }
    }
}

impl Modulation {
    pub const MIN_LENGTH: f64 = 0.001;

    pub fn load(path: &Path) -> Result<Self, String> {
        let mut modulation: Self = load_json(path)?;
        if modulation.points.is_empty() {
            return Err(format!("{}: a modulation needs points", path.display()));
        }
        modulation.normalize();
        Ok(modulation)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        save_json(self, path)
    }

    /// Clamp everything into range and sort the points.
    pub fn normalize(&mut self) {
        if self.length.is_nan() || self.length < Self::MIN_LENGTH {
            self.length = Self::MIN_LENGTH;
        }
        for p in &mut self.points {
            p.at = p.at.clamp(0.0, 1.0);
            p.value = p.value.clamp(0.0, 1.0);
            p.curve = p.curve.clamp(-1.0, 1.0);
        }
        self.points.sort_by(|a, b| a.at.total_cmp(&b.at));
    }

    /// The value `seconds` in, when played `times` times and then held.
    pub fn value_after(&self, seconds: f64, times: usize) -> f64 {
        let passes = seconds / self.length;
        if passes >= times as f64 {
            self.value_at(1.0)
        } else {
            self.value_at(passes.fract())
        }
    }

    /// The value at `fraction` (0..1) of the length.
    pub fn value_at(&self, fraction: f64) -> f64 {
        let points = &self.points;
        // The number of points at or before `fraction`.
        let i = points.partition_point(|p| p.at <= fraction);
        if i == 0 {
            return points[0].value;
        }
        if i == points.len() {
            return points[i - 1].value;
        }
        let (a, b) = (points[i - 1], points[i]);
        let u = (fraction - a.at) / (b.at - a.at);
        curve::segment(a.value, b.value, u, a.curve)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_between_and_around_points() {
        let m = Modulation {
            length: 1.0,
            points: vec![
                Point::new(0.25, 0.2),
                Point {
                    at: 0.5,
                    value: 1.0,
                    curve: 0.5,
                },
                Point::new(0.75, 0.0),
            ],
        };
        assert_eq!(m.value_at(0.0), 0.2);
        assert_eq!(m.value_at(0.25), 0.2);
        assert!((m.value_at(0.375) - 0.6).abs() < 1e-12);
        assert_eq!(m.value_at(0.5), 1.0);
        // The bent segment falls slowly first.
        assert!(m.value_at(0.625) > 0.5);
        assert_eq!(m.value_at(1.0), 0.0);
    }

    #[test]
    fn two_points_at_the_same_place_make_a_jump() {
        let m = Modulation {
            length: 1.0,
            points: vec![
                Point::new(0.0, 0.0),
                Point::new(0.5, 0.0),
                Point::new(0.5, 1.0),
                Point::new(1.0, 1.0),
            ],
        };
        assert_eq!(m.value_at(0.49), 0.0);
        assert_eq!(m.value_at(0.5), 1.0);
    }

    #[test]
    fn round_trips_through_json() {
        let dir = std::env::temp_dir().join(format!("rocktober-mod-{}", std::process::id()));
        let path = dir.join("modulations/sweep.json");
        let m = Modulation::default();
        m.save(&path).unwrap();
        assert_eq!(Modulation::load(&path).unwrap(), m);
        // Hand-written files may leave out curves and be out of order.
        std::fs::write(
            &path,
            r#"{"length": 4, "points": [{"at": 1, "value": 2}, {"at": 0, "value": 0}]}"#,
        )
        .unwrap();
        let loaded = Modulation::load(&path).unwrap();
        assert_eq!(loaded.points, [Point::new(0.0, 0.0), Point::new(1.0, 1.0)]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
