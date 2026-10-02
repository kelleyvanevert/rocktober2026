//! Finding `-- comments`, the only thing the editor highlights for now.

use std::ops::Range;

/// Byte ranges of every comment, from `--` to the end of its line. A `--`
/// inside a string literal doesn't start a comment.
pub fn comment_ranges(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut in_string = false;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\n' => in_string = false,
            b'"' => in_string = !in_string,
            b'-' if !in_string && bytes.get(i + 1) == Some(&b'-') => {
                let end = text[i..].find('\n').map_or(text.len(), |n| i + n);
                ranges.push(i..end);
                i = end;
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_comments_outside_strings() {
        let text = "-- a\nplay(sample(\"x--y\")) -- b\n";
        let found: Vec<&str> = comment_ranges(text).into_iter().map(|r| &text[r]).collect();
        assert_eq!(found, ["-- a", "-- b"]);
    }
}
