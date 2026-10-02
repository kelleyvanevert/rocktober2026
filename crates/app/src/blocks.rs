//! Finding the code to run when nothing is selected.

use std::ops::Range;

/// The block around `offset`: the run of non-blank lines containing it, like
/// Tidal and Strudel. Returns an empty range if `offset` is on a blank line.
pub fn block_at(text: &str, offset: usize) -> Range<usize> {
    let mut lines = Vec::new();
    let mut start = 0;
    for (i, c) in text.char_indices() {
        if c == '\n' {
            lines.push(start..i);
            start = i + 1;
        }
    }
    lines.push(start..text.len());

    let blank = |i: usize| text[lines[i].clone()].trim().is_empty();
    let current = lines
        .iter()
        .position(|line| offset <= line.end)
        .unwrap_or(lines.len() - 1);
    if blank(current) {
        return offset..offset;
    }

    let mut first = current;
    while first > 0 && !blank(first - 1) {
        first -= 1;
    }
    let mut last = current;
    while last + 1 < lines.len() && !blank(last + 1) {
        last += 1;
    }
    lines[first].start..lines[last].end
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "a\nb\n\n  \nc\nd\ne";

    fn block(offset: usize) -> &'static str {
        &TEXT[block_at(TEXT, offset)]
    }

    #[test]
    fn finds_surrounding_block() {
        assert_eq!(block(0), "a\nb");
        assert_eq!(block(3), "a\nb"); // end of "b"
        assert_eq!(block(9), "c\nd\ne");
        assert_eq!(block(TEXT.len()), "c\nd\ne");
    }

    #[test]
    fn blank_line_is_empty() {
        assert_eq!(block(4), "");
        assert_eq!(block(6), "");
    }
}
