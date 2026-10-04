//! Finding `-- comments`, the only thing the editor highlights for now, and
//! toggling them on and off.

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

/// Commenting or uncommenting lines: replace `range` with `text`, then
/// select `selection`.
#[derive(Debug, PartialEq)]
pub struct Toggle {
    pub range: Range<usize>,
    pub text: String,
    pub selection: Range<usize>,
}

/// Comment out the lines `selection` touches, or uncomment them if they all
/// are comments already, like cmd-/ in other editors. Blank lines are left
/// alone, unless there's nothing else (so cmd-/ on an empty line starts a
/// comment). A selection that ends at the start of a line leaves that line out.
pub fn toggle(text: &str, selection: Range<usize>) -> Toggle {
    let start = text[..selection.start].rfind('\n').map_or(0, |n| n + 1);
    let last = if selection.end > selection.start && text[..selection.end].ends_with('\n') {
        selection.end - 1
    } else {
        selection.end
    };
    let end = text[last..].find('\n').map_or(text.len(), |n| last + n);

    // (offset in `text`, bytes removed, text inserted), in order.
    let mut edits: Vec<(usize, usize, &str)> = Vec::new();
    let mut lines = Vec::new();
    let mut at = start;
    for line in text[start..end].split('\n') {
        lines.push((at, line));
        at += line.len() + 1;
    }
    let indent = |line: &str| line.len() - line.trim_start().len();
    let all_blank = lines.iter().all(|(_, line)| line.trim().is_empty());
    let filled = lines
        .iter()
        .filter(|(_, line)| all_blank || !line.trim().is_empty());
    if !all_blank
        && filled
            .clone()
            .all(|(_, line)| line.trim_start().starts_with("--"))
    {
        for &(at, line) in filled {
            let rest = line.trim_start();
            let len = if rest.starts_with("-- ") { 3 } else { 2 };
            edits.push((at + indent(line), len, ""));
        }
    } else {
        let column = filled
            .clone()
            .map(|(_, line)| indent(line))
            .min()
            .unwrap_or(0);
        for &(at, _) in filled {
            edits.push((at + column, 0, "-- "));
        }
    }

    let mut new = String::new();
    let mut copied = start;
    for &(at, removed, inserted) in &edits {
        new.push_str(&text[copied..at]);
        new.push_str(inserted);
        copied = at + removed;
    }
    new.push_str(&text[copied..end]);

    // Where `offset` ends up. `after` puts it after text inserted right there.
    let map = |offset: usize, after: bool| {
        let mut moved = offset as isize;
        for &(at, removed, inserted) in &edits {
            if offset < at || (offset == at && removed == 0 && !after) {
                break;
            } else if offset < at + removed {
                moved -= (offset - at) as isize;
            } else {
                moved += inserted.len() as isize - removed as isize;
            }
        }
        moved as usize
    };
    let selection = if selection.is_empty() {
        let cursor = map(selection.start, true);
        cursor..cursor
    } else {
        map(selection.start, false)..map(selection.end, true)
    };
    Toggle {
        range: start..end,
        text: new,
        selection,
    }
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

    /// Toggle with the selection marked by `[` and `]` (or just `|` for a
    /// cursor), and return the whole text with the new selection marked.
    fn toggled(marked: &str) -> String {
        let (text, selection) = if let Some(at) = marked.find('|') {
            (marked.replace('|', ""), at..at)
        } else {
            let start = marked.find('[').unwrap();
            let end = marked.find(']').unwrap() - 1;
            (marked.replace(['[', ']'], ""), start..end)
        };
        let t = toggle(&text, selection);
        let mut text = text;
        text.replace_range(t.range, &t.text);
        if t.selection.is_empty() {
            text.insert(t.selection.start, '|');
        } else {
            text.insert(t.selection.end, ']');
            text.insert(t.selection.start, '[');
        }
        text
    }

    #[test]
    fn toggles_the_line_under_the_cursor() {
        assert_eq!(toggled("a\nb|c\nd"), "a\n-- b|c\nd");
        assert_eq!(toggled("a\n-- b|c\nd"), "a\nb|c\nd");
        assert_eq!(toggled("a\n--b|c\nd"), "a\nb|c\nd");
        assert_eq!(toggled("|"), "-- |");
    }

    #[test]
    fn toggles_selected_lines_at_their_least_indent() {
        assert_eq!(
            toggled("x\n[seq(\n  a,\n\n)]\ny"),
            "x\n[-- seq(\n--   a,\n\n-- )]\ny"
        );
        assert_eq!(
            toggled("x\n[-- seq(\n--   a,\n\n-- )]\ny"),
            "x\n[seq(\n  a,\n\n)]\ny"
        );
        assert_eq!(toggled("  [a\n    b]"), "  [-- a\n  --   b]");
    }

    #[test]
    fn comments_out_a_mix() {
        assert_eq!(toggled("[-- a\nb]"), "[-- -- a\n-- b]");
    }

    #[test]
    fn leaves_out_a_line_the_selection_only_reaches_the_start_of() {
        assert_eq!(toggled("[a\n]b"), "[-- a\n]b");
    }
}
