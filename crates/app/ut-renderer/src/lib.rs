//! `ut-renderer`: turns [`ut_grid::Grid`] state into something drawable,
//! without assuming any particular drawing backend (Section 17
//! "Performance and reliability": "Render batching", "Virtualized
//! scrollback", "Separate parser and renderer threads" — this crate is the
//! renderer side of that split, consuming `Grid` snapshots produced on
//! whatever thread runs `ut-grid`).
//!
//! Two things live here:
//! - [`render_row_ansi`]: reproduce one row as a minimal ANSI/SGR string
//!   (only emitting an escape when the style actually changes cell to
//!   cell). Useful for a text-mode fallback renderer, for "Save output as
//!   text/HTML" style export, and for tests that want to assert on styled
//!   output without a GPU.
//! - [`Damage`]: diff two `Grid` snapshots and report which rows actually
//!   changed, so a real (GPU or terminal-in-terminal) renderer only
//!   repaints what moved — "Render batching" / "Output throttling".

use ut_grid::{Cell, Color, Grid, Style};

/// Render one row of `grid` as plain text followed by the minimal set of
/// SGR escapes needed to reproduce each style change left to right, ending
/// with a reset if any styling was emitted.
pub fn render_row_ansi(grid: &Grid, row: usize) -> String {
    let mut out = String::new();
    // Start "as if" default style is already active, so a row that never
    // leaves the default style emits no escapes at all.
    let mut current: Option<Style> = Some(Style::default());
    let mut styled_anything = false;

    for cell in grid.row(row) {
        if cell.width == 0 {
            continue; // wide-char placeholder, already covered by the previous cell
        }
        if current != Some(cell.style) {
            out.push_str(&sgr_for(&cell.style));
            current = Some(cell.style);
            styled_anything = true;
        }
        out.push_str(&cell.text);
    }

    if styled_anything {
        out.push_str("\x1b[0m");
    }
    out
}

/// Render the whole visible grid as ANSI text, one line per row.
pub fn render_screen_ansi(grid: &Grid) -> String {
    (0..grid.rows).map(|r| render_row_ansi(grid, r)).collect::<Vec<_>>().join("\r\n")
}

fn sgr_for(style: &Style) -> String {
    let mut codes: Vec<String> = vec!["0".to_string()]; // always reset first so codes don't accumulate
    if style.bold {
        codes.push("1".into());
    }
    if style.faint {
        codes.push("2".into());
    }
    if style.italic {
        codes.push("3".into());
    }
    if style.underline {
        codes.push("4".into());
    }
    if style.blink {
        codes.push("5".into());
    }
    if style.reverse {
        codes.push("7".into());
    }
    if style.hidden {
        codes.push("8".into());
    }
    if style.strikethrough {
        codes.push("9".into());
    }
    push_color_codes(&mut codes, style.fg, true);
    push_color_codes(&mut codes, style.bg, false);
    format!("\x1b[{}m", codes.join(";"))
}

fn push_color_codes(codes: &mut Vec<String>, color: Color, is_fg: bool) {
    match color {
        Color::Default => {}
        Color::Indexed(i) => {
            if i < 8 {
                codes.push((if is_fg { 30 + i } else { 40 + i }).to_string());
            } else if i < 16 {
                codes.push((if is_fg { 90 + (i - 8) } else { 100 + (i - 8) }).to_string());
            } else {
                codes.push(if is_fg { "38".into() } else { "48".into() });
                codes.push("5".into());
                codes.push(i.to_string());
            }
        }
        Color::Rgb(r, g, b) => {
            codes.push(if is_fg { "38".into() } else { "48".into() });
            codes.push("2".into());
            codes.push(r.to_string());
            codes.push(g.to_string());
            codes.push(b.to_string());
        }
    }
}

/// The set of rows that differ between two grid snapshots of the same size.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Damage {
    pub dirty_rows: Vec<usize>,
}

impl Damage {
    pub fn is_empty(&self) -> bool {
        self.dirty_rows.is_empty()
    }
}

/// Compare two grids row by row (they must be the same dimensions — a
/// resize is its own full-repaint event, not a diff) and report which rows changed.
pub fn diff(previous: &Grid, current: &Grid) -> Damage {
    if previous.cols != current.cols || previous.rows != current.rows {
        return Damage { dirty_rows: (0..current.rows).collect() };
    }
    let mut dirty_rows = Vec::new();
    for r in 0..current.rows {
        if !rows_equal(previous.row(r), current.row(r)) {
            dirty_rows.push(r);
        }
    }
    Damage { dirty_rows }
}

fn rows_equal(a: &[Cell], b: &[Cell]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x == y)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ut_grid::Terminal;

    #[test]
    fn plain_text_row_has_no_escapes() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"hello");
        let rendered = render_row_ansi(t.grid(), 0);
        assert!(!rendered.contains('\x1b'));
        assert!(rendered.starts_with("hello"));
    }

    #[test]
    fn colored_row_emits_sgr_and_reset() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[31mred\x1b[0m");
        let rendered = render_row_ansi(t.grid(), 0);
        assert!(rendered.contains("\x1b[0;31m"));
        assert!(rendered.ends_with("\x1b[0m"));
        assert!(rendered.contains("red"));
    }

    #[test]
    fn style_change_mid_row_emits_two_escapes() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[1mB\x1b[22mN");
        let rendered = render_row_ansi(t.grid(), 0);
        // one SGR for bold "B", one SGR to drop back to normal for "N"
        assert_eq!(rendered.matches('\x1b').count(), 3); // 2 SGR + trailing reset
    }

    #[test]
    fn wide_char_placeholder_not_double_rendered() {
        let mut t = Terminal::new(10, 1);
        t.feed("中".as_bytes());
        let rendered = render_row_ansi(t.grid(), 0);
        assert_eq!(rendered.matches('中').count(), 1);
    }

    #[test]
    fn true_color_round_trips_through_sgr() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[38;2;10;20;30mX");
        let rendered = render_row_ansi(t.grid(), 0);
        assert!(rendered.contains("38;2;10;20;30"));
    }

    #[test]
    fn diff_reports_no_dirty_rows_for_identical_grids() {
        let mut t1 = Terminal::new(10, 2);
        t1.feed(b"same");
        let mut t2 = Terminal::new(10, 2);
        t2.feed(b"same");
        let d = diff(t1.grid(), t2.grid());
        assert!(d.is_empty());
    }

    #[test]
    fn diff_reports_only_changed_rows() {
        let mut t1 = Terminal::new(10, 3);
        t1.feed(b"a\r\nb\r\nc");
        let mut t2 = Terminal::new(10, 3);
        t2.feed(b"a\r\nB\r\nc"); // middle row differs
        let d = diff(t1.grid(), t2.grid());
        assert_eq!(d.dirty_rows, vec![1]);
    }

    #[test]
    fn diff_size_mismatch_marks_everything_dirty() {
        let t1 = Terminal::new(5, 2);
        let t2 = Terminal::new(10, 2);
        let d = diff(t1.grid(), t2.grid());
        assert_eq!(d.dirty_rows, vec![0, 1]);
    }

    #[test]
    fn render_screen_joins_rows_with_crlf() {
        let mut t = Terminal::new(5, 2);
        t.feed(b"ab\r\ncd");
        let screen = render_screen_ansi(t.grid());
        assert!(screen.contains("ab"));
        assert!(screen.contains("cd"));
        assert!(screen.contains("\r\n"));
    }
}
