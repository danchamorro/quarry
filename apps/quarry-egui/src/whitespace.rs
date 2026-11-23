//! View-only whitespace rendering built on top of `quarry_workbench::whitespace`.
use eframe::egui::{self, Color32, Response, Shape, Stroke, TextFormat, text::LayoutJob};
#[cfg(test)]
use quarry_workbench::whitespace::analyze;
pub(super) use quarry_workbench::whitespace::{
    Cache, CellId, MAX_LAYOUT_SECTIONS, Preferences, Summary, scalar,
};
#[cfg(test)]
use std::sync::atomic::AtomicBool;

pub(super) fn finish_frame(cache: &mut Cache, ctx: &egui::Context) {
    let repaint = ctx.clone();
    if cache.finish_frame_with_notify(move || repaint.request_repaint()) {
        ctx.request_repaint_after(std::time::Duration::from_millis(20));
    }
}

pub(super) struct Rendered {
    pub job: LayoutJob,
    pub visible_marks: bool,
}

/// Preserve the scalar/output bound and cap shaping sections after visual-mark expansion.
pub(super) fn render(
    bytes: &[u8],
    summary: Option<Summary>,
    all: bool,
    max_chars: usize,
    text_color: Color32,
    mark_color: Color32,
) -> Rendered {
    let mut job = LayoutJob::default();
    let normal = TextFormat {
        font_id: super::theme::grid_font(),
        color: text_color,
        ..Default::default()
    };
    let marked = TextFormat {
        color: mark_color,
        ..normal.clone()
    };
    let mut offset = 0;
    let mut output_chars = 0;
    let mut leading = true;
    let mut previous_space = false;
    let mut visible_marks = false;
    let mut run = String::new();
    let mut run_marked = false;
    let mut preserve_mark_format = true;
    while offset < bytes.len() && output_chars < max_chars {
        let (c, width) = scalar(bytes, offset);
        let repeated = c == ' ' && (previous_space || bytes.get(offset + width) == Some(&b' '));
        let edge = summary.is_some_and(|s| offset < s.leading_end || offset >= s.trailing_start);
        let mark = c.is_whitespace()
            && (all || c != ' ' || leading || repeated || edge || offset + width == bytes.len());
        let text = if mark {
            visible_marks = true;
            match c {
                ' ' => "·".into(),
                '\t' => "[TAB]".into(),
                '\r' => "[CR]".into(),
                '\n' => "[LF]".into(),
                '\u{a0}' => "[NBSP]".into(),
                _ => format!("[U+{:04X}]", c as u32),
            }
        } else {
            c.to_string()
        };
        let count = text.chars().count();
        if output_chars + count > max_chars {
            break;
        }
        if !run.is_empty() && mark != run_marked && preserve_mark_format {
            if job.sections.len() < MAX_LAYOUT_SECTIONS.saturating_sub(3) {
                job.append(
                    &run,
                    0.0,
                    if run_marked {
                        marked.clone()
                    } else {
                        normal.clone()
                    },
                );
                run.clear();
                run_marked = mark;
            } else {
                // Keep the textual marks but flatten the remaining format so
                // attacker-controlled transitions cannot add more sections.
                if run_marked {
                    job.append(&run, 0.0, marked.clone());
                    run.clear();
                }
                preserve_mark_format = false;
                run_marked = false;
            }
        }
        if run.is_empty() {
            run_marked = mark && preserve_mark_format;
        }
        run.push_str(&text);
        output_chars += count;
        leading &= c.is_whitespace();
        previous_space = c == ' ';
        offset += width;
    }
    if !run.is_empty() {
        job.append(
            &run,
            0.0,
            if run_marked {
                marked.clone()
            } else {
                normal.clone()
            },
        );
    }
    if offset < bytes.len() {
        let retained = max_chars.saturating_sub(3);
        let end = job
            .text
            .char_indices()
            .nth(retained)
            .map_or(job.text.len(), |(i, _)| i);
        job.text.truncate(end);
        job.sections.retain(|s| s.byte_range.start < end);
        for section in &mut job.sections {
            section.byte_range.end = section.byte_range.end.min(end);
        }
        job.append(&".".repeat(max_chars.min(3)), 0.0, normal);
    }
    Rendered { job, visible_marks }
}

pub(super) fn paint_indicator(ui: &egui::Ui, response: &Response, indicated: bool, pending: bool) {
    let visible = response.rect.intersect(ui.clip_rect());
    if visible.width() < 9.0 || visible.height() < 9.0 {
        return;
    }
    let center = egui::pos2(visible.right() - 4.5, visible.top() + 4.5);
    let palette = super::theme::ui_palette(ui);
    let painter = ui
        .painter()
        .with_clip_rect(response.rect.intersect(ui.clip_rect()));
    if indicated {
        let points = vec![
            center + egui::vec2(-3.0, -3.0),
            center + egui::vec2(3.0, -3.0),
            center + egui::vec2(3.0, 3.0),
        ];
        painter.add(Shape::convex_polygon(
            points,
            palette.indicator_fill,
            Stroke::new(0.7_f32, palette.indicator_stroke),
        ));
    } else if pending {
        painter.circle_stroke(center, 2.0, Stroke::new(0.7_f32, palette.mark));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn summary(bytes: &[u8]) -> Summary {
        analyze(bytes, &AtomicBool::new(false)).unwrap()
    }
    fn text(bytes: &[u8], all: bool, bound: usize) -> String {
        render(
            bytes,
            Some(summary(bytes)),
            all,
            bound,
            Color32::BLACK,
            Color32::GRAY,
        )
        .job
        .text
    }

    #[test]
    fn normal_word_spaces_stay_subtle_and_all_mode_reveals_them() {
        let value = b"New York City";
        let s = summary(value);
        assert_eq!(s.spaces, 2);
        assert!(!s.likely());
        assert!(s.indicated(true));
        assert_eq!(text(value, false, 100), "New York City");
        assert_eq!(text(value, true, 100), "New·York·City");
        assert!(s.description(false).contains("Ordinary single spaces"));
    }

    #[test]
    fn edges_repeated_runs_and_whitespace_only_values_are_marked() {
        assert_eq!(text(b"  New York  ", false, 100), "··New York··");
        assert_eq!(text(b"a  b   c", false, 100), "a··b···c");
        assert_eq!(text(b"   ", false, 100), "···");
        let s = summary(b"  a  b   ");
        assert_eq!((s.leading, s.trailing, s.repeated_spaces), (2, 3, 7));
        assert_eq!(summary(b"").leading, 0);
        assert!(!summary(b"").any());
    }

    #[test]
    fn tabs_line_breaks_and_special_unicode_spaces_are_distinguishable() {
        let value = "\tA\r\nB\u{a0}C\u{202f}D\u{2003} ";
        let s = summary(value.as_bytes());
        assert_eq!((s.tabs, s.cr, s.lf, s.nbsp, s.other), (1, 1, 1, 1, 2));
        assert_eq!((s.leading, s.trailing), (1, 2));
        assert_eq!(
            text(value.as_bytes(), false, 200),
            "[TAB]A[CR][LF]B[NBSP]C[U+202F]D[U+2003]·"
        );
        assert_eq!(text(br"\t \n", false, 100), r"\t \n");
        assert!(!summary("zero\u{200b}width".as_bytes()).any());
    }

    #[test]
    fn unicode_whitespace_table_and_invalid_bytes_keep_exact_positions() {
        for c in [
            '\t', '\n', '\u{b}', '\u{c}', '\r', ' ', '\u{85}', '\u{a0}', '\u{1680}', '\u{2000}',
            '\u{2001}', '\u{2002}', '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}',
            '\u{2008}', '\u{2009}', '\u{200a}', '\u{2028}', '\u{2029}', '\u{202f}', '\u{205f}',
            '\u{3000}',
        ] {
            let value = format!("{c}a{c}");
            let s = summary(value.as_bytes());
            assert_eq!((s.leading, s.trailing), (1, 1), "{c:?}");
            assert!(s.likely());
        }
        let value = b"\xff \xe2\x80 a ";
        let s = summary(value);
        assert_eq!((s.leading, s.trailing, s.spaces), (0, 1, 3));
        assert_eq!(text(value, false, 100), "� �� a·");
        assert_eq!(value, b"\xff \xe2\x80 a ");
    }

    #[test]
    fn rendering_bounds_include_expanded_marks_and_preserve_utf8() {
        for bound in 0..=128 {
            for value in ["🙂".repeat(100), "\t ".repeat(100), "a\u{202f}".repeat(100)] {
                let rendered = render(
                    value.as_bytes(),
                    Some(summary(value.as_bytes())),
                    true,
                    bound,
                    Color32::BLACK,
                    Color32::GRAY,
                );
                assert!(rendered.job.text.chars().count() <= bound);
                for section in &rendered.job.sections {
                    assert!(rendered.job.text.get(section.byte_range.clone()).is_some());
                }
            }
        }
        assert_eq!(text("é界🙂".as_bytes(), false, 3), "é界🙂");
        assert_eq!(text(b"abcdef", false, 5), "ab...");
    }

    #[test]
    fn rendering_bounds_sections_without_hiding_whitespace_marks() {
        for (value, all, mark) in [
            ("a\t".repeat(20_000), false, "[TAB]"),
            ("a ".repeat(20_000), true, "·"),
        ] {
            let rendered = render(
                value.as_bytes(),
                Some(summary(value.as_bytes())),
                all,
                16 * 1024,
                Color32::BLACK,
                Color32::GRAY,
            );
            assert!(rendered.job.sections.len() <= MAX_LAYOUT_SECTIONS);
            assert!(rendered.job.text.matches(mark).count() > MAX_LAYOUT_SECTIONS);
            assert!(rendered.job.text.ends_with("..."));
            assert!(rendered.visible_marks);
            for section in &rendered.job.sections {
                assert!(rendered.job.text.get(section.byte_range.clone()).is_some());
            }
        }
        let ordinary = render(
            b"A\tB",
            Some(summary(b"A\tB")),
            false,
            100,
            Color32::BLACK,
            Color32::GRAY,
        );
        assert_eq!(ordinary.job.text, "A[TAB]B");
        assert_eq!(ordinary.job.sections.len(), 3);
        assert_eq!(
            ordinary
                .job
                .sections
                .iter()
                .map(|section| section.format.color)
                .collect::<Vec<_>>(),
            [Color32::BLACK, Color32::GRAY, Color32::BLACK]
        );
    }

    #[test]
    fn full_value_summary_finds_whitespace_beyond_preview() {
        let mut value = vec![b'x'; 1024 * 1024];
        value.extend_from_slice(b" \t");
        let s = summary(&value);
        assert_eq!((s.spaces, s.tabs, s.trailing), (1, 1, 2));
        assert!(s.likely());
        let r = render(&value, Some(s), false, 120, Color32::BLACK, Color32::GRAY);
        assert_eq!(r.job.text, "x".repeat(117) + "...");
        assert!(!r.visible_marks);
        assert!(s.description(false).contains("outside the visible cell"));
    }
}
