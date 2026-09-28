//! Converts `ut-grid` state into `egui` text layout. The counterpart of
//! `ut-tui`'s `render.rs`, targeting `egui::text::LayoutJob` instead of
//! ratatui spans — both are plain data structures, buildable and
//! inspectable without a window, which is what keeps this testable.

use egui::{Color32, FontId, TextFormat};
use ut_grid::{Color, Grid, Style};

/// Convert a `ut-grid` [`Grid`] into one [`egui::text::LayoutJob`] per row,
/// each made of the minimal styled runs needed to reproduce every style
/// change left to right.
pub fn grid_to_rows(grid: &Grid, font_size: f32) -> Vec<egui::text::LayoutJob> {
    (0..grid.rows).map(|r| row_to_layout_job(grid, r, font_size)).collect()
}

/// Convert the whole grid into a single [`egui::text::LayoutJob`] with rows
/// joined by newlines — what a pane widget actually renders (one `egui::Label`
/// draws a whole pane in one pass instead of one per row).
pub fn grid_to_layout_job(grid: &Grid, font_size: f32) -> egui::text::LayoutJob {
    let mut combined = egui::text::LayoutJob::default();
    let font = FontId::monospace(font_size);
    for r in 0..grid.rows {
        let row_job = row_to_layout_job(grid, r, font_size);
        for section in row_job.sections {
            let text = &row_job.text[section.byte_range.clone()];
            combined.append(text, 0.0, section.format);
        }
        if r + 1 < grid.rows {
            combined.append("\n", 0.0, TextFormat { font_id: font.clone(), ..Default::default() });
        }
    }
    combined
}

fn row_to_layout_job(grid: &Grid, row: usize, font_size: f32) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let mut current_style: Option<Style> = None;
    let mut current_text = String::new();
    let font = FontId::monospace(font_size);

    let flush = |job: &mut egui::text::LayoutJob, text: &mut String, style: &Style, font: &FontId| {
        if text.is_empty() {
            return;
        }
        job.append(text, 0.0, to_text_format(style, font.clone()));
        text.clear();
    };

    for cell in grid.row(row) {
        if cell.width == 0 {
            continue; // wide-char placeholder
        }
        if current_style != Some(cell.style) {
            if let Some(style) = current_style {
                flush(&mut job, &mut current_text, &style, &font);
            }
            current_style = Some(cell.style);
        }
        current_text.push_str(&cell.text);
    }
    if let Some(style) = current_style {
        flush(&mut job, &mut current_text, &style, &font);
    }
    job
}

fn to_text_format(style: &Style, font: FontId) -> TextFormat {
    let mut fmt = TextFormat { font_id: font, ..Default::default() };
    fmt.color = to_color32(style.fg, Color32::from_gray(220));
    if !matches!(style.bg, Color::Default) {
        fmt.background = to_color32(style.bg, Color32::TRANSPARENT);
    }
    if style.reverse {
        std::mem::swap(&mut fmt.color, &mut fmt.background);
        if fmt.background == Color32::TRANSPARENT {
            fmt.background = Color32::from_gray(220);
        }
    }
    if style.underline || style.double_underline {
        fmt.underline = egui::Stroke::new(1.0, fmt.color);
    }
    if style.strikethrough {
        fmt.strikethrough = egui::Stroke::new(1.0, fmt.color);
    }
    if style.italic {
        fmt.italics = true;
    }
    if style.faint {
        fmt.color = fmt.color.gamma_multiply(0.6);
    }
    fmt
}

fn to_color32(color: Color, default: Color32) -> Color32 {
    match color {
        Color::Default => default,
        Color::Indexed(i) => indexed_to_color32(i),
        Color::Rgb(r, g, b) => Color32::from_rgb(r, g, b),
    }
}

fn indexed_to_color32(i: u8) -> Color32 {
    const BASE16: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (229, 229, 229),
    ];
    if (i as usize) < 16 {
        let (r, g, b) = BASE16[i as usize];
        Color32::from_rgb(r, g, b)
    } else if i < 232 {
        // 6x6x6 color cube (indices 16..=231).
        let i = i - 16;
        let r = i / 36;
        let g = (i % 36) / 6;
        let b = i % 6;
        let scale = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        Color32::from_rgb(scale(r), scale(g), scale(b))
    } else {
        // Grayscale ramp (indices 232..=255).
        let level = 8 + (i - 232) * 10;
        Color32::from_gray(level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ut_grid::Terminal;

    #[test]
    fn plain_text_produces_single_section() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"hello");
        let rows = grid_to_rows(t.grid(), 14.0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].sections.len(), 1);
        assert_eq!(rows[0].text, "hello     ");
    }

    #[test]
    fn colored_text_maps_to_ansi_red() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[31mred\x1b[0m");
        let rows = grid_to_rows(t.grid(), 14.0);
        let red_section = rows[0].sections.iter().find(|s| rows[0].text[s.byte_range.clone()].contains("red")).unwrap();
        assert_eq!(red_section.format.color, Color32::from_rgb(205, 49, 49));
    }

    #[test]
    fn style_change_splits_into_multiple_sections() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[1mB\x1b[22mN");
        let rows = grid_to_rows(t.grid(), 14.0);
        assert!(rows[0].sections.len() >= 2);
    }

    #[test]
    fn true_color_maps_directly() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[38;2;10;20;30mX");
        let rows = grid_to_rows(t.grid(), 14.0);
        assert_eq!(rows[0].sections[0].format.color, Color32::from_rgb(10, 20, 30));
    }

    #[test]
    fn reverse_video_swaps_fg_and_bg() {
        let mut t = Terminal::new(10, 1);
        t.feed(b"\x1b[31;7mX");
        let rows = grid_to_rows(t.grid(), 14.0);
        let fmt = &rows[0].sections[0].format;
        assert_eq!(fmt.background, Color32::from_rgb(205, 49, 49));
    }

    #[test]
    fn wide_char_placeholder_not_double_rendered() {
        let mut t = Terminal::new(10, 1);
        t.feed("中".as_bytes());
        let rows = grid_to_rows(t.grid(), 14.0);
        assert_eq!(rows[0].text.matches('中').count(), 1);
    }

    #[test]
    fn indexed_256_color_cube_produces_distinct_colors() {
        assert_ne!(indexed_to_color32(196), indexed_to_color32(21));
    }

    #[test]
    fn grayscale_ramp_is_monotonic() {
        let low = indexed_to_color32(232);
        let high = indexed_to_color32(255);
        assert!(low.r() < high.r());
    }
}
