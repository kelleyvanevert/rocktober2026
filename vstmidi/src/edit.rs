// Editing commands for the code editor, on plain text so they're easy to test.

use std::ops::Range;

/// Cmd+/: comment out the lines touched by the selection, or uncomment them if they all already
/// are. `selection` and the returned selection are in chars (that's what egui uses), not bytes.
pub fn toggle_comment(text: &str, selection: Range<usize>) -> (String, Range<usize>) {
    let byte = |chars: usize| text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i);
    let (sel_start, mut sel_end) = (byte(selection.start), byte(selection.end));
    // A selection ending at the very start of a line (after a triple click, say) doesn't include it.
    if sel_end > sel_start && text[..sel_end].ends_with('\n') {
        sel_end -= 1;
    }
    let start = text[..sel_start].rfind('\n').map_or(0, |i| i + 1);
    let end = text[sel_end..].find('\n').map_or(text.len(), |i| sel_end + i);

    let lines: Vec<&str> = text[start..end].split('\n').collect();
    let blank = |line: &str| line.trim().is_empty();
    let all_blank = lines.iter().all(|l| blank(l));
    // Blank lines are left alone, unless that's all there is.
    let touched = |line: &str| all_blank || !blank(line);
    let indent = |line: &str| line.len() - line.trim_start().len();

    let commented = !all_blank && lines.iter().filter(|l| touched(l)).all(|l| l.trim_start().starts_with("--"));
    let new_lines: Vec<String> = if commented {
        lines
            .iter()
            .map(|line| {
                let (ws, rest) = line.split_at(indent(line));
                match rest.strip_prefix("--") {
                    Some(rest) => format!("{ws}{}", rest.strip_prefix(' ').unwrap_or(rest)),
                    None => line.to_string(),
                }
            })
            .collect()
    } else {
        // Line the comment markers up at the least indented line, like most editors do.
        let column = lines.iter().filter(|l| touched(l)).map(|l| indent(l)).min().unwrap_or(0);
        lines
            .iter()
            .map(|line| match touched(line) {
                true => format!("{}-- {}", &line[..column], &line[column..]),
                false => line.to_string(),
            })
            .collect()
    };
    let block = new_lines.join("\n");
    let new_text = format!("{}{block}{}", &text[..start], &text[end..]);

    let chars = |s: &str| s.chars().count();
    let new_selection = if selection.is_empty() {
        // Keep the cursor where it was in its line, shifted by what changed before it.
        let first_old = lines[0];
        let first_new = &new_lines[0];
        let column = chars(&text[start..sel_start]);
        let shift = chars(first_new) as isize - chars(first_old) as isize;
        let column = (column as isize + shift).clamp(0, chars(first_new) as isize) as usize;
        let at = chars(&text[..start]) + column;
        at..at
    } else {
        // Select the whole lines that changed.
        let from = chars(&text[..start]);
        from..from + chars(&block)
    };
    (new_text, new_selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toggle(text: &str, selection: Range<usize>) -> String {
        toggle_comment(text, selection).0
    }

    #[test]
    fn comments_and_uncomments_one_line() {
        assert_eq!(toggle("every 4 { note c3 }", 3..3), "-- every 4 { note c3 }");
        assert_eq!(toggle("-- every 4 { note c3 }", 3..3), "every 4 { note c3 }");
        assert_eq!(toggle("--every 4", 0..0), "every 4");
        assert_eq!(toggle("a\nb\nc", 2..2), "a\n-- b\nc");
    }

    #[test]
    fn selections_across_lines() {
        let code = "every 4 {\n  note c3\n\n  note d3\n}";
        // From the middle of line 2 into line 4: the blank line stays blank.
        assert_eq!(toggle(code, 14..23), "every 4 {\n  -- note c3\n\n  -- note d3\n}");
        // Markers go at the least indented line.
        assert_eq!(toggle(code, 0..code.len()), "-- every 4 {\n--   note c3\n\n--   note d3\n-- }");
        // Mixed: comment everything.
        assert_eq!(toggle("-- a\nb", 0..6), "-- -- a\n-- b");
        // Round trip.
        let once = toggle_comment(code, 0..code.len());
        assert_eq!(toggle(&once.0, once.1), code);
    }

    #[test]
    fn selection_ending_at_line_start_skips_that_line() {
        assert_eq!(toggle("a\nb\n", 0..2), "-- a\nb\n");
    }

    #[test]
    fn cursor_stays_put() {
        assert_eq!(toggle_comment("ab\n  cd", 6..6), ("ab\n  -- cd".into(), 9..9));
        assert_eq!(toggle_comment("ab\n  -- cd", 9..9), ("ab\n  cd".into(), 6..6));
        // Cursor inside the "-- " that's being removed.
        assert_eq!(toggle_comment("-- x", 1..1), ("x".into(), 0..0));
    }

    #[test]
    fn non_ascii() {
        assert_eq!(toggle_comment("é\nb", 2..2), ("é\n-- b".into(), 5..5));
    }
}
