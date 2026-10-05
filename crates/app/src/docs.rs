//! What the documentation panel looks up: the name under the cursor.

use std::ops::Range;

/// A name to look up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    /// A node, function or `let`: `wavetable`, `notes`, `wub`.
    Name(String),
    /// A param: after a `:` (`:note`) or a `?` (`?note`).
    Param(String),
}

/// The name the cursor at byte `offset` is on (or right after), unless it's
/// in a comment or a string, or isn't a name (a number, a time).
pub fn lookup_at(text: &str, offset: usize) -> Option<Lookup> {
    let range = word_at(text, offset)?;
    let line_start = text[..range.start].rfind('\n').map_or(0, |i| i + 1);
    let before = &text[line_start..range.start];
    if in_comment_or_string(before) {
        return None;
    }
    let word = &text[range.clone()];
    if !word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        return None;
    }
    Some(match before.chars().last() {
        Some(':' | '?') => Lookup::Param(word.to_string()),
        _ => Lookup::Name(word.to_string()),
    })
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '#'
}

/// The run of name characters around `offset`.
fn word_at(text: &str, offset: usize) -> Option<Range<usize>> {
    let offset = offset.min(text.len());
    let start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, c)| is_word(*c))
        .last()
        .map_or(offset, |(i, _)| i);
    let end = text[offset..]
        .char_indices()
        .find(|(_, c)| !is_word(*c))
        .map_or(text.len(), |(i, _)| offset + i);
    (start < end).then_some(start..end)
}

/// Whether the end of `line` (up to the word) is inside a string or a
/// comment. Strings have no escapes.
fn in_comment_or_string(line: &str) -> bool {
    let mut in_string = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' => in_string = !in_string,
            '-' if !in_string && chars.peek() == Some(&'-') => return true,
            _ => {}
        }
    }
    in_string
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Option<Lookup> {
        let offset = text.find('|').unwrap();
        let text = text.replace('|', "");
        lookup_at(&text, offset)
    }

    fn name(s: &str) -> Option<Lookup> {
        Some(Lookup::Name(s.into()))
    }

    fn param(s: &str) -> Option<Lookup> {
        Some(Lookup::Param(s.into()))
    }

    #[test]
    fn finds_names_and_params() {
        assert_eq!(at("wave|table:pos(0.3)"), name("wavetable"));
        assert_eq!(at("|wavetable"), name("wavetable"));
        assert_eq!(at("wavetable|"), name("wavetable"));
        assert_eq!(at("wavetable:p|os(0.3)"), param("pos"));
        assert_eq!(at("x:note(?no|te + 12)"), param("note"));
        assert_eq!(at("a\nlet wub = |kick * 2"), name("kick"));
        assert_eq!(at("x.pl|ay()"), name("play"));
    }

    #[test]
    fn skips_comments_strings_and_numbers() {
        assert_eq!(at("-- a wave|table"), None);
        assert_eq!(at("sine -- a wave|table"), None);
        assert_eq!(at("sample(\"ki|ck.mp3\")"), None);
        assert_eq!(at("sample(\"kick.mp3\") * dri|ve"), name("drive"));
        assert_eq!(at("x * 0.|5"), None);
        assert_eq!(at("x |  y"), None);
        assert_eq!(at("|"), None);
    }
}
