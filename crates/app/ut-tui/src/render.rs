//! Converts `ut-grid` state into `ratatui` widgets. Kept separate from
//! `ut-renderer` (which stays toolkit-agnostic, producing ANSI text) since
//! this module's whole job is being ratatui-specific.

use ratatui::style::{Color as RColor, Modifier, Style as RStyle};
use ratatui::text::{Line, Span, Text};
use ut_grid::{Color, Grid, Style};

/// Convert a `ut-grid` [`Grid`] into a ratatui [`Text`] — one [`Line`] per
/// row, each made of the minimal [`Span`] runs needed to reproduce every
/// style change left to right (mirrors `ut_renderer::render_row_ansi`'s
/// coalescing, just targeting ratatui spans instead of ANSI bytes).
pub fn grid_to_text(grid: &Grid) -> Text<'static> {
    let lines: Vec<Line<'static>> = (0..grid.rows).map(|r| row_to_line(grid, r)).collect();
    Text::from(lines)
}

fn row_to_line(grid: &Grid, row: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut current_style: Option<Style> = None;
    let mut current_text = String::new();

    for cell in grid.row(row) {
        if cell.width == 0 {
            continue; // wide-char placeholder
        }
        if current_style != Some(cell.style) {
            if let Some(style) = current_style.take() {
                spans.push(Span::styled(std::mem::take(&mut current_text), to_ratatui_style(&style)));
            }
            current_style = Some(cell.style);
        }
        current_text.push_str(&cell.text);
    }
    if let Some(style) = current_style {
        spans.push(Span::styled(current_text, to_ratatui_style(&style)));
    }
    Line::from(spans)
}

fn to_ratatui_style(style: &Style) -> RStyle {
    let mut s = RStyle::default();
    if !matches!(style.fg, Color::Default) {
        s = s.fg(to_ratatui_color(style.fg));
    }
    if !matches!(style.bg, Color::Default) {
        s = s.bg(to_ratatui_color(style.bg));
    }
    let mut modifiers = Modifier::empty();
    if style.bold {
        modifiers |= Modifier::BOLD;
    }
    if style.faint {
        modifiers |= Modifier::DIM;
    }
    if style.italic {
        modifiers |= Modifier::ITALIC;
    }
    if style.underline || style.double_underline {
        modifiers |= Modifier::UNDERLINED;
    }
    if style.blink {
        modifiers |= Modifier::SLOW_BLINK;
    }
    if style.reverse {
        modifiers |= Modifier::REVERSED;
    }
    if style.hidden {
        modifiers |= Modifier::HIDDEN;
    }
    if style.strikethrough {
        modifiers |= Modifier::CROSSED_OUT;
    }
    s.add_modifier(modifiers)
}

fn to_ratatui_color(color: Color) -> RColor {
    match color {
        Color::Default => RColor::Reset,
        Color::Indexed(i) => match i {
            0 => RColor::Black,
            1 => RColor::Red,
            2 => RColor::Green,
            3 => RColor::Yellow,
            4 => RColor::Blue,
            5 => RColor::Magenta,
            6 => RColor::Cyan,
            7 => RColor::Gray,
            8 => RColor::DarkGray,
            9 => RColor::LightRed,
            10 => RColor::LightGreen,
            11 => RColor::LightYellow,
            12 => RColor::LightBlue,
            13 => RColor::LightMagenta,
            14 => RColor::LightCyan,
            15 => RColor::White,
            other => RColor::Indexed(other),
        },
        Color::Rgb(r, g, b) => RColor::Rgb(r, g, b),
    }
}

/// Border color for a pane: highlighted when it's the focused one — a
/// small piece of "Pane activity indicators" / focus styling.
pub fn pane_border_style(focused: bool) -> RStyle {
    if focused {
        RStyle::default().fg(RColor::Cyan).add_modifier(Modifier::BOLD)
    } else {
        RStyle::default().fg(RColor::DarkGray)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ut_grid::Terminal;

    #[test]
    fn plain_text_produces_single_span_per_line() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"hello");
        let text = grid_to_text(t.grid());
        assert_eq!(text.lines.len(), 1);
        assert_eq!(text.lines[0].spans.len(), 1);
        // Full row width is preserved (not trimmed) so ratatui repaints
        // stale content instead of leaving it behind.
        assert_eq!(text.lines[0].spans[0].content, "hello     ");
    }

    #[test]
    fn colored_text_gets_styled_span() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[31mred\x1b[0m");
        let text = grid_to_text(t.grid());
        let red_span = text.lines[0].spans.iter().find(|s| s.content.contains("red")).unwrap();
        assert_eq!(red_span.style.fg, Some(RColor::Red));
    }

    #[test]
    fn style_change_mid_row_splits_into_multiple_spans() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[1mB\x1b[22mN");
        let text = grid_to_text(t.grid());
        assert!(text.lines[0].spans.len() >= 2);
    }

    #[test]
    fn true_color_maps_to_rgb() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[38;2;10;20;30mX");
        let text = grid_to_text(t.grid());
        let span = &text.lines[0].spans[0];
        assert_eq!(span.style.fg, Some(RColor::Rgb(10, 20, 30)));
    }

    #[test]
    fn bold_sets_bold_modifier() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[1mbold");
        let text = grid_to_text(t.grid());
        assert!(text.lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn wide_char_placeholder_not_double_rendered() {
        let mut t = Terminal::new(10, 1);
        t.feed("中".as_bytes());
        let text = grid_to_text(t.grid());
        let joined: String = text.lines[0].spans.iter().map(|s| s.content.to_string()).collect();
        assert_eq!(joined.matches('中').count(), 1);
    }

    #[test]
    fn focused_border_differs_from_unfocused() {
        assert_ne!(pane_border_style(true), pane_border_style(false));
    }

    fn dummy_grid() -> Grid {
        Grid::new(5, 1)
    }

    #[test]
    fn empty_grid_produces_blank_line() {
        let g = dummy_grid();
        let text = grid_to_text(&g);
        assert_eq!(text.lines.len(), 1);
    }
}
