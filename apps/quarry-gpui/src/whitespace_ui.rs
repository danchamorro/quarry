//! View-only whitespace rendering for GPUI built on top of `quarry_workbench::whitespace`.
//!
//! Raw cell bytes remain authoritative for all data operations.

use std::ops::Range;

use gpui_kit::{HighlightStyle, Hsla, StyledText};
use quarry_workbench::whitespace::{MAX_LAYOUT_SECTIONS, Summary, scalar};

/// Rendered cell/header preview text together with bounded byte ranges for whitespace marks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedWhitespace {
    pub text: String,
    pub mark_ranges: Vec<Range<usize>>,
    pub visible_marks: bool,
    pub prefix_invalid_utf8: bool,
}

impl RenderedWhitespace {
    pub fn into_styled_text(self, mark_color: Hsla) -> StyledText {
        if self.mark_ranges.is_empty() {
            StyledText::new(self.text)
        } else {
            let style = HighlightStyle {
                color: Some(mark_color),
                ..HighlightStyle::default()
            };
            StyledText::new(self.text).with_highlights(
                self.mark_ranges
                    .into_iter()
                    .map(move |range| (range, style)),
            )
        }
    }
}

/// Formats raw field bytes into a bounded single-line display string with subtle whitespace marks.
///
/// Normal mode (`all = false`) leaves ordinary single spaces between words unmarked and marks
/// leading/trailing spaces, repeated space runs, tabs (`[TAB]`), carriage returns (`[CR]`),
/// line feeds (`[LF]`), non-breaking spaces (`[NBSP]`), and other Unicode whitespace (`[U+XXXX]`).
/// Show-all mode (`all = true`) also marks ordinary single spaces (`·`).
///
/// Highlight runs are bounded by `MAX_LAYOUT_SECTIONS` so hostile alternating whitespace cannot
/// produce thousands of text runs in a single cell.
pub fn render_whitespace(
    bytes: &[u8],
    summary: Option<Summary>,
    all: bool,
    max_chars: usize,
) -> RenderedWhitespace {
    let mut text = String::new();
    let mut mark_ranges: Vec<Range<usize>> = Vec::new();
    let mut offset = 0;
    let mut output_chars = 0;
    let mut leading = true;
    let mut previous_space = false;
    let mut visible_marks = false;
    let mut prefix_invalid_utf8 = false;
    let mut current_mark_start: Option<usize> = None;
    let mut preserve_mark_format = true;
    let max_mark_runs = MAX_LAYOUT_SECTIONS.saturating_sub(3) / 2;

    while offset < bytes.len() && output_chars < max_chars {
        let (c, width) = scalar(bytes, offset);
        if c == '\u{fffd}' && width == 1 {
            prefix_invalid_utf8 = true;
        }
        let repeated = c == ' ' && (previous_space || bytes.get(offset + width) == Some(&b' '));
        let edge = summary.is_some_and(|s| offset < s.leading_end || offset >= s.trailing_start);
        let mark = c.is_whitespace()
            && (all || c != ' ' || leading || repeated || edge || offset + width == bytes.len());
        let piece: std::borrow::Cow<'static, str> = if mark {
            visible_marks = true;
            match c {
                ' ' => "·".into(),
                '\t' => "[TAB]".into(),
                '\r' => "[CR]".into(),
                '\n' => "[LF]".into(),
                '\u{a0}' => "[NBSP]".into(),
                _ => format!("[U+{:04X}]", c as u32).into(),
            }
        } else {
            c.to_string().into()
        };
        let count = piece.chars().count();
        if output_chars + count > max_chars {
            break;
        }
        let start = text.len();
        if mark && preserve_mark_format {
            if current_mark_start.is_none() {
                if mark_ranges.len() < max_mark_runs {
                    current_mark_start = Some(start);
                } else {
                    // Keep the textual marks but flatten remaining highlights so hostile
                    // alternating whitespace cannot create unbounded text runs.
                    preserve_mark_format = false;
                }
            }
        } else if let Some(mark_start) = current_mark_start.take() {
            mark_ranges.push(mark_start..start);
        }
        text.push_str(&piece);
        output_chars += count;
        leading &= c.is_whitespace();
        previous_space = c == ' ';
        offset += width;
    }

    if let Some(mark_start) = current_mark_start.take() {
        mark_ranges.push(mark_start..text.len());
    }

    if offset < bytes.len() {
        let retained = max_chars.saturating_sub(3);
        let end = text
            .char_indices()
            .nth(retained)
            .map_or(text.len(), |(idx, _)| idx);
        text.truncate(end);
        mark_ranges.retain_mut(|range| {
            if range.start >= end {
                return false;
            }
            range.end = range.end.min(end);
            range.start < range.end
        });
        for _ in 0..max_chars.min(3) {
            text.push('.');
        }
    }

    RenderedWhitespace {
        text,
        mark_ranges,
        visible_marks,
        prefix_invalid_utf8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use quarry_workbench::whitespace::analyze;
    use std::sync::atomic::AtomicBool;

    fn summary(bytes: &[u8]) -> Summary {
        analyze(bytes, &AtomicBool::new(false)).unwrap()
    }

    #[test]
    fn normal_word_spaces_stay_subtle_and_all_mode_reveals_them() {
        let value = b"New York City";
        let s = summary(value);
        let normal = render_whitespace(value, Some(s), false, 100);
        assert_eq!(normal.text, "New York City");
        assert!(!normal.visible_marks);
        assert!(normal.mark_ranges.is_empty());

        let all = render_whitespace(value, Some(s), true, 100);
        assert_eq!(all.text, "New·York·City");
        assert!(all.visible_marks);
        assert_eq!(all.mark_ranges.len(), 2);
    }

    #[test]
    fn edges_repeated_runs_and_control_whitespace_are_marked() {
        let r = render_whitespace(b"  New York  ", Some(summary(b"  New York  ")), false, 100);
        assert_eq!(r.text, "··New York··");
        assert_eq!(r.mark_ranges.len(), 2);

        let value = "\tA\r\nB\u{a0}C\u{202f}D ";
        let r2 = render_whitespace(
            value.as_bytes(),
            Some(summary(value.as_bytes())),
            false,
            200,
        );
        assert_eq!(r2.text, "[TAB]A[CR][LF]B[NBSP]C[U+202F]D·");
        assert!(r2.visible_marks);
        for range in &r2.mark_ranges {
            assert!(r2.text.get(range.clone()).is_some());
        }
    }

    #[test]
    fn bounds_sections_and_preserves_utf8_boundaries() {
        let value = "a\t".repeat(10_000);
        let r = render_whitespace(
            value.as_bytes(),
            Some(summary(value.as_bytes())),
            false,
            240,
        );
        assert!(r.text.chars().count() <= 240);
        assert!(r.text.ends_with("..."));
        assert!(r.mark_ranges.len() * 2 < MAX_LAYOUT_SECTIONS);
        for range in &r.mark_ranges {
            assert!(r.text.get(range.clone()).is_some());
        }
    }
}
