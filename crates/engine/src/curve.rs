//! What envelopes and modulations have in common: curved segments, and being
//! stored as small JSON files.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;

/// How strongly a curve of ±1 bends: at the extremes, the middle of a segment
/// is within 5% of one of its ends.
const STEEPNESS: f64 = 6.0;

/// Bend a position `u` (0..1) along a segment. `curve` is -1..1: 0 is a
/// straight line, positive starts slow and ends fast (like `e^x`), negative
/// starts fast and ends slow (like a natural decay).
pub fn bend(u: f64, curve: f64) -> f64 {
    let k = curve.clamp(-1.0, 1.0) * STEEPNESS;
    if k.abs() < 1e-6 {
        return u;
    }
    ((k * u).exp() - 1.0) / (k.exp() - 1.0)
}

/// The value at `u` (0..1) along a segment from `from` to `to`.
pub fn segment(from: f64, to: f64, u: f64, curve: f64) -> f64 {
    from + (to - from) * bend(u.clamp(0.0, 1.0), curve)
}

pub(crate) fn load_json<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

pub(crate) fn save_json<T: Serialize>(value: &T, path: &Path) -> Result<(), String> {
    let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| fail(&e))?;
    }
    let mut text = serde_json::to_string_pretty(value).map_err(|e| fail(&e))?;
    text.push('\n');
    std::fs::write(path, text).map_err(|e| fail(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bend_keeps_the_ends_and_bends_the_middle() {
        for curve in [-1.0, -0.3, 0.0, 0.5, 1.0] {
            assert!(bend(0.0, curve).abs() < 1e-12);
            assert!((bend(1.0, curve) - 1.0).abs() < 1e-12);
        }
        assert_eq!(bend(0.5, 0.0), 0.5);
        assert!(bend(0.5, 1.0) < 0.05);
        assert!(bend(0.5, -1.0) > 0.95);
        // Opposite curves mirror each other.
        assert!((bend(0.3, 0.4) - (1.0 - bend(0.7, -0.4))).abs() < 1e-12);
    }
}
