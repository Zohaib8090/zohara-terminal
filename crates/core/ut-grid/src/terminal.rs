//! [`Terminal`]: cursor + modes + two [`Grid`]s (primary/alternate),
//! updated by applying [`AnsiEvent`]s from `ut-ansi`.

use crate::grid::{Cell, Color, Grid, Style};
use ut_ansi::{AnsiEvent, AnsiParser, CsiEvent};
use unicode_width::UnicodeWidthChar;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct SavedCursor {
    col: usize,
    row: usize,
    style: Style,
}

/// A headless terminal: applies ANSI events and maintains grid + cursor + modes.
pub struct Terminal {
    parser: AnsiParser,
    primary: Grid,
    alternate: Grid,
    using_alternate: bool,
    pub cursor_col: usize,
    pub cursor_row: usize,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    style: Style,
    saved_cursor: Option<SavedCursor>,
    scroll_top: usize,
    scroll_bottom: usize,
    /// DECAWM: auto-wrap at right margin.
    autowrap: bool,
    /// Pending-wrap flag: the cursor logically sits past the last column
    /// and the next printed character should wrap first.
    wrap_pending: bool,
    pub insert_mode: bool,
    pub origin_mode: bool,
    pub bracketed_paste: bool,
    pub mouse_reporting: MouseMode,
    pub application_cursor_keys: bool,
    pub application_keypad: bool,
    pub title: String,
    pub icon_name: String,
    /// OSC 7: `file://host/path` reported by the shell for the cwd.
    pub reported_cwd: Option<String>,
    pub bell_count: u64,
    pub synchronized_output: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MouseMode {
    #[default]
    Off,
    X10,
    Normal,
    ButtonEvent,
    AnyEvent,
}

impl Terminal {
    pub fn new(cols: usize, rows: usize) -> Self {
        Terminal {
            parser: AnsiParser::new(),
            primary: Grid::new(cols, rows),
            alternate: Grid::new(cols, rows),
            using_alternate: false,
            cursor_col: 0,
            cursor_row: 0,
            cursor_visible: true,
            cursor_shape: CursorShape::default(),
            cursor_blink: true,
            style: Style::default(),
            saved_cursor: None,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            autowrap: true,
            wrap_pending: false,
            insert_mode: false,
            origin_mode: false,
            bracketed_paste: false,
            mouse_reporting: MouseMode::Off,
            application_cursor_keys: false,
            application_keypad: false,
            title: String::new(),
            icon_name: String::new(),
            reported_cwd: None,
            bell_count: 0,
            synchronized_output: false,
        }
    }

    /// Parse `bytes` (e.g. freshly read from a PTY) and apply the resulting events.
    pub fn feed(&mut self, bytes: &[u8]) {
        let events = self.parser.feed(bytes);
        for event in &events {
            self.apply(event);
        }
    }

    /// Apply a single already-parsed event. Exposed so events can come from
    /// somewhere other than this terminal's own parser (a recording, a
    /// test, another `ut-ansi` instance shared across panes, etc.).
    pub fn apply(&mut self, event: &AnsiEvent) {
        match event {
            AnsiEvent::Print(c) => self.put_char(*c),
            AnsiEvent::Execute(byte) => self.execute(*byte),
            AnsiEvent::Csi(csi) => self.csi_dispatch(csi),
            AnsiEvent::Esc { intermediates, byte, .. } => self.esc_dispatch(intermediates, *byte),
            AnsiEvent::Osc(osc) => self.osc_dispatch(osc),
            AnsiEvent::DcsHook(_) | AnsiEvent::DcsPut(_) | AnsiEvent::DcsUnhook => {
                // Device control strings (e.g. Sixel, terminfo queries) are
                // recognized by ut-ansi but not interpreted here yet.
            }
        }
    }

    pub fn grid(&self) -> &Grid {
        if self.using_alternate { &self.alternate } else { &self.primary }
    }

    fn grid_mut(&mut self) -> &mut Grid {
        if self.using_alternate { &mut self.alternate } else { &mut self.primary }
    }

    pub fn cols(&self) -> usize {
        self.grid().cols
    }

    pub fn rows(&self) -> usize {
        self.grid().rows
    }

    pub fn is_alternate_screen(&self) -> bool {
        self.using_alternate
    }

    /// Render the visible screen as plain text lines (no styling), useful
    /// for tests and simple debugging/snapshotting.
    pub fn plain_text(&self) -> Vec<String> {
        let g = self.grid();
        (0..g.rows)
            .map(|r| {
                let mut s = String::new();
                for c in g.row(r) {
                    if c.width == 0 {
                        continue;
                    }
                    s.push_str(&c.text);
                }
                s.trim_end_matches(' ').to_string()
            })
            .collect()
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.primary.resize(cols, rows, Style::default());
        self.alternate.resize(cols, rows, Style::default());
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        self.wrap_pending = false;
    }

    fn effective_top(&self) -> usize {
        if self.origin_mode { self.scroll_top } else { 0 }
    }

    #[allow(dead_code)] // symmetric with effective_top(); used by future origin-mode-aware handlers
    fn effective_bottom(&self) -> usize {
        if self.origin_mode { self.scroll_bottom } else { self.grid().rows - 1 }
    }

    fn move_cursor_to(&mut self, col: usize, row: usize) {
        let cols = self.cols();
        let rows = self.rows();
        self.cursor_col = col.min(cols.saturating_sub(1));
        self.cursor_row = row.min(rows.saturating_sub(1));
        self.wrap_pending = false;
    }

    fn newline(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            let (top, bottom) = (self.scroll_top, self.scroll_bottom);
            let style = self.style;
            let save = bottom == self.grid().rows - 1 && !self.using_alternate;
            self.grid_mut().scroll_up(top, bottom, 1, style, save);
        } else if self.cursor_row < self.grid().rows - 1 {
            self.cursor_row += 1;
        }
        self.wrap_pending = false;
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == self.scroll_top {
            let (top, bottom) = (self.scroll_top, self.scroll_bottom);
            let style = self.style;
            self.grid_mut().scroll_down(top, bottom, 1, style);
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
        }
        self.wrap_pending = false;
    }

    fn put_char(&mut self, ch: char) {
        let width = UnicodeWidthChar::width(ch).unwrap_or(1).max(1) as u8;
        let cols = self.cols();

        if self.wrap_pending {
            if self.autowrap {
                self.cursor_col = 0;
                self.newline();
            }
            self.wrap_pending = false;
        }

        if self.cursor_col + (width as usize) > cols {
            if self.autowrap {
                self.cursor_col = 0;
                self.newline();
            } else {
                self.cursor_col = cols.saturating_sub(width as usize);
            }
        }

        if self.insert_mode {
            self.shift_right_from_cursor(width as usize);
        }

        let style = self.style;
        let col = self.cursor_col;
        let row = self.cursor_row;
        self.grid_mut().set(col, row, Cell { text: ch.to_string(), style, width });
        if width == 2 && col + 1 < cols {
            self.grid_mut().set(col + 1, row, Cell::placeholder(style));
        }

        if col + (width as usize) >= cols {
            self.cursor_col = cols.saturating_sub(1);
            self.wrap_pending = true;
        } else {
            self.cursor_col += width as usize;
        }
    }

    fn shift_right_from_cursor(&mut self, n: usize) {
        let cols = self.cols();
        let row = self.cursor_row;
        let col = self.cursor_col;
        let style = self.style;
        for _ in 0..n {
            let mut i = cols - 1;
            while i > col {
                let prev = self.grid().get(i - 1, row).clone();
                self.grid_mut().set(i, row, prev);
                i -= 1;
            }
            self.grid_mut().set(col, row, Cell::blank_with_style(style));
        }
    }

    fn erase_in_display(&mut self, mode: u16) {
        let style = self.style;
        let (row, col) = (self.cursor_row, self.cursor_col);
        let rows = self.rows();
        let cols = self.cols();
        match mode {
            0 => {
                self.erase_in_line_from(col, row, style);
                for r in row + 1..rows {
                    self.grid_mut().clear_row(r, style);
                }
            }
            1 => {
                for r in 0..row {
                    self.grid_mut().clear_row(r, style);
                }
                for c in 0..=col.min(cols.saturating_sub(1)) {
                    self.grid_mut().set(c, row, Cell::blank_with_style(style));
                }
            }
            2 | 3 => {
                for r in 0..rows {
                    self.grid_mut().clear_row(r, style);
                }
                if mode == 3 {
                    self.grid_mut().scrollback.clear();
                }
            }
            _ => {}
        }
    }

    fn erase_in_line_from(&mut self, col: usize, row: usize, style: Style) {
        let cols = self.cols();
        for c in col..cols {
            self.grid_mut().set(c, row, Cell::blank_with_style(style));
        }
    }

    fn erase_in_line(&mut self, mode: u16) {
        let style = self.style;
        let (row, col) = (self.cursor_row, self.cursor_col);
        let cols = self.cols();
        match mode {
            0 => self.erase_in_line_from(col, row, style),
            1 => {
                for c in 0..=col.min(cols.saturating_sub(1)) {
                    self.grid_mut().set(c, row, Cell::blank_with_style(style));
                }
            }
            2 => self.grid_mut().clear_row(row, style),
            _ => {}
        }
    }

    fn apply_sgr(&mut self, csi: &CsiEvent) {
        let mut it = csi.iter();
        while let Some(p) = it.next() {
            let code = p.first().copied().unwrap_or(0);
            match code {
                0 => self.style.reset(),
                1 => self.style.bold = true,
                2 => self.style.faint = true,
                3 => self.style.italic = true,
                4 => self.style.underline = true,
                21 => self.style.double_underline = true,
                5 | 6 => self.style.blink = true,
                7 => self.style.reverse = true,
                8 => self.style.hidden = true,
                9 => self.style.strikethrough = true,
                22 => {
                    self.style.bold = false;
                    self.style.faint = false;
                }
                23 => self.style.italic = false,
                24 => {
                    self.style.underline = false;
                    self.style.double_underline = false;
                }
                25 => self.style.blink = false,
                27 => self.style.reverse = false,
                28 => self.style.hidden = false,
                29 => self.style.strikethrough = false,
                30..=37 => self.style.fg = Color::Indexed((code - 30) as u8),
                38 => {
                    if let Some(c) = parse_extended_color(&mut it) {
                        self.style.fg = c;
                    }
                }
                39 => self.style.fg = Color::Default,
                40..=47 => self.style.bg = Color::Indexed((code - 40) as u8),
                48 => {
                    if let Some(c) = parse_extended_color(&mut it) {
                        self.style.bg = c;
                    }
                }
                49 => self.style.bg = Color::Default,
                90..=97 => self.style.fg = Color::Indexed((code - 90 + 8) as u8),
                100..=107 => self.style.bg = Color::Indexed((code - 100 + 8) as u8),
                _ => {}
            }
        }
    }

    fn set_mode(&mut self, csi: &CsiEvent, enable: bool) {
        let private = csi.is_private();
        for p in csi.iter() {
            let code = p.first().copied().unwrap_or(0);
            if private {
                match code {
                    1 => self.application_cursor_keys = enable,
                    3 => { /* 132/80 column mode: not implemented, ignore */ }
                    6 => self.origin_mode = enable,
                    7 => self.autowrap = enable,
                    12 => self.cursor_blink = enable,
                    25 => self.cursor_visible = enable,
                    1000 => self.mouse_reporting = if enable { MouseMode::Normal } else { MouseMode::Off },
                    1002 => self.mouse_reporting = if enable { MouseMode::ButtonEvent } else { MouseMode::Off },
                    1003 => self.mouse_reporting = if enable { MouseMode::AnyEvent } else { MouseMode::Off },
                    9 => self.mouse_reporting = if enable { MouseMode::X10 } else { MouseMode::Off },
                    1049 | 47 | 1047 => self.set_alternate_screen(enable, code == 1049),
                    2004 => self.bracketed_paste = enable,
                    2026 => self.synchronized_output = enable,
                    _ => {}
                }
            } else if code == 4 {
                self.insert_mode = enable;
            }
        }
    }

    fn set_alternate_screen(&mut self, enable: bool, save_cursor: bool) {
        if enable && !self.using_alternate {
            if save_cursor {
                self.saved_cursor = Some(SavedCursor { col: self.cursor_col, row: self.cursor_row, style: self.style });
            }
            self.using_alternate = true;
            let (cols, rows) = (self.alternate.cols, self.alternate.rows);
            self.alternate = Grid::new(cols, rows);
            self.alternate.scrollback_limit = 0;
            self.cursor_col = 0;
            self.cursor_row = 0;
        } else if !enable && self.using_alternate {
            self.using_alternate = false;
            if save_cursor {
                if let Some(sc) = self.saved_cursor.take() {
                    self.cursor_col = sc.col;
                    self.cursor_row = sc.row;
                    self.style = sc.style;
                }
            }
        }
        self.wrap_pending = false;
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.bell_count += 1,
            0x08 => {
                if self.cursor_col > 0 {
                    self.cursor_col -= 1;
                }
                self.wrap_pending = false;
            }
            0x09 => {
                let cols = self.cols();
                let next_tab = ((self.cursor_col / 8) + 1) * 8;
                self.cursor_col = next_tab.min(cols.saturating_sub(1));
                self.wrap_pending = false;
            }
            0x0A..=0x0C => self.newline(),
            0x0D => {
                self.cursor_col = 0;
                self.wrap_pending = false;
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, osc: &ut_ansi::OscEvent) {
        match osc.kind() {
            Some("0") | Some("2") => {
                if let Some(text) = osc.field(0) {
                    self.title = text;
                }
            }
            Some("1") => {
                if let Some(text) = osc.field(0) {
                    self.icon_name = text;
                }
            }
            Some("7") => {
                if let Some(text) = osc.field(0) {
                    self.reported_cwd = Some(text);
                }
            }
            Some("8") => {
                // OSC 8 hyperlink: recognized and ignored (no link-span tracking on cells yet).
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, csi: &CsiEvent) {
        let action = csi.action;
        let n = |default: u16| csi.param_or(0, default);
        match action {
            'A' => self.move_cursor_to(self.cursor_col, self.cursor_row.saturating_sub(n(1) as usize)),
            'B' | 'e' => self.move_cursor_to(self.cursor_col, self.cursor_row + n(1) as usize),
            'C' | 'a' => self.move_cursor_to(self.cursor_col + n(1) as usize, self.cursor_row),
            'D' => self.move_cursor_to(self.cursor_col.saturating_sub(n(1) as usize), self.cursor_row),
            'E' => self.move_cursor_to(0, self.cursor_row + n(1) as usize),
            'F' => self.move_cursor_to(0, self.cursor_row.saturating_sub(n(1) as usize)),
            'G' | '`' => self.move_cursor_to(n(1) as usize - 1, self.cursor_row),
            'H' | 'f' => {
                let row = csi.param_or(0, 1) as usize - 1;
                let col = csi.param_or(1, 1) as usize - 1;
                let base = self.effective_top();
                self.move_cursor_to(col, base + row);
            }
            'd' => {
                let base = self.effective_top();
                self.move_cursor_to(self.cursor_col, base + n(1) as usize - 1);
            }
            'J' => self.erase_in_display(csi.param_or(0, 0)),
            'K' => self.erase_in_line(csi.param_or(0, 0)),
            'L' => {
                let (top, bottom) = (self.cursor_row, self.scroll_bottom);
                let style = self.style;
                self.grid_mut().scroll_down(top, bottom, n(1) as usize, style);
            }
            'M' => {
                let (top, bottom) = (self.cursor_row, self.scroll_bottom);
                let style = self.style;
                self.grid_mut().scroll_up(top, bottom, n(1) as usize, style, false);
            }
            '@' => {
                self.shift_right_from_cursor(n(1) as usize);
            }
            'P' => {
                let count = n(1) as usize;
                let cols = self.cols();
                let row = self.cursor_row;
                let col = self.cursor_col;
                let style = self.style;
                for c in col..cols {
                    let src = c + count;
                    let cell = if src < cols { self.grid().get(src, row).clone() } else { Cell::blank_with_style(style) };
                    self.grid_mut().set(c, row, cell);
                }
            }
            'X' => {
                let count = n(1) as usize;
                let cols = self.cols();
                let row = self.cursor_row;
                let style = self.style;
                for c in self.cursor_col..(self.cursor_col + count).min(cols) {
                    self.grid_mut().set(c, row, Cell::blank_with_style(style));
                }
            }
            'S' => {
                let (top, bottom) = (self.scroll_top, self.scroll_bottom);
                let style = self.style;
                self.grid_mut().scroll_up(top, bottom, n(1) as usize, style, false);
            }
            'T' => {
                let (top, bottom) = (self.scroll_top, self.scroll_bottom);
                let style = self.style;
                self.grid_mut().scroll_down(top, bottom, n(1) as usize, style);
            }
            'm' => self.apply_sgr(csi),
            'h' => self.set_mode(csi, true),
            'l' => self.set_mode(csi, false),
            'r' => {
                let rows = self.rows();
                let top = csi.param_or(0, 1) as usize - 1;
                let bottom = csi.param_or(1, rows as u16) as usize - 1;
                if top < bottom && bottom < rows {
                    self.scroll_top = top;
                    self.scroll_bottom = bottom;
                } else {
                    self.scroll_top = 0;
                    self.scroll_bottom = rows - 1;
                }
                self.move_cursor_to(0, self.effective_top());
            }
            's' => {
                self.saved_cursor = Some(SavedCursor { col: self.cursor_col, row: self.cursor_row, style: self.style });
            }
            'u' => {
                if let Some(sc) = self.saved_cursor {
                    self.cursor_col = sc.col;
                    self.cursor_row = sc.row;
                    self.style = sc.style;
                    self.wrap_pending = false;
                }
            }
            'q' if csi.has_space_intermediate() => {
                self.cursor_shape = match n(1) {
                    0..=2 => CursorShape::Block,
                    3 | 4 => CursorShape::Underline,
                    5 | 6 => CursorShape::Bar,
                    _ => self.cursor_shape,
                };
                self.cursor_blink = matches!(n(1), 0 | 1 | 3 | 5);
            }
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], byte: u8) {
        match (intermediates.first(), byte) {
            (None, b'D') => self.newline(),
            (None, b'M') => self.reverse_index(),
            (None, b'E') => {
                self.cursor_col = 0;
                self.newline();
            }
            (None, b'7') => {
                self.saved_cursor = Some(SavedCursor { col: self.cursor_col, row: self.cursor_row, style: self.style });
            }
            (None, b'8') => {
                if let Some(sc) = self.saved_cursor {
                    self.cursor_col = sc.col;
                    self.cursor_row = sc.row;
                    self.style = sc.style;
                    self.wrap_pending = false;
                }
            }
            (None, b'c') => {
                let (cols, rows) = (self.cols(), self.rows());
                *self = Terminal::new(cols, rows);
            }
            _ => {}
        }
    }
}

fn parse_extended_color<'a, I>(it: &mut I) -> Option<Color>
where
    I: Iterator<Item = &'a [u16]>,
{
    let kind = it.next()?.first().copied()?;
    match kind {
        5 => {
            let idx = it.next()?.first().copied()?;
            Some(Color::Indexed(idx as u8))
        }
        2 => {
            let r = it.next()?.first().copied()? as u8;
            let g = it.next()?.first().copied()? as u8;
            let b = it.next()?.first().copied()? as u8;
            Some(Color::Rgb(r, g, b))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prints_plain_text() {
        let mut s = Terminal::new(10, 3);
        s.feed(b"hello");
        assert_eq!(s.plain_text()[0], "hello");
        assert_eq!(s.cursor_col, 5);
    }

    #[test]
    fn handles_newline_and_carriage_return() {
        let mut s = Terminal::new(10, 3);
        s.feed(b"a\r\nb");
        assert_eq!(s.plain_text()[0], "a");
        assert_eq!(s.plain_text()[1], "b");
    }

    #[test]
    fn autowraps_at_right_margin() {
        let mut s = Terminal::new(5, 3);
        s.feed(b"abcdef");
        assert_eq!(s.plain_text()[0], "abcde");
        assert_eq!(s.plain_text()[1], "f");
    }

    #[test]
    fn cursor_movement_csi() {
        let mut s = Terminal::new(10, 5);
        s.feed(b"\x1b[3;5Hx");
        assert_eq!(s.plain_text()[2].chars().nth(4), Some('x'));
    }

    #[test]
    fn erase_in_line() {
        let mut s = Terminal::new(10, 1);
        s.feed(b"hello\x1b[3G\x1b[K");
        assert_eq!(s.plain_text()[0], "he");
    }

    #[test]
    fn erase_in_display_full() {
        let mut s = Terminal::new(5, 2);
        s.feed(b"abcde12345\x1b[2J");
        assert_eq!(s.plain_text(), vec!["".to_string(), "".to_string()]);
    }

    #[test]
    fn sgr_colors_applied() {
        let mut s = Terminal::new(10, 1);
        s.feed(b"\x1b[31mred\x1b[0m");
        let g = s.grid();
        assert_eq!(g.get(0, 0).style.fg, Color::Indexed(1));
        assert_eq!(g.get(3, 0).style.fg, Color::Default);
    }

    #[test]
    fn sgr_true_color() {
        let mut s = Terminal::new(10, 1);
        s.feed(b"\x1b[38;2;10;20;30mX");
        assert_eq!(s.grid().get(0, 0).style.fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn sgr_256_color() {
        let mut s = Terminal::new(10, 1);
        s.feed(b"\x1b[48;5;200mX");
        assert_eq!(s.grid().get(0, 0).style.bg, Color::Indexed(200));
    }

    #[test]
    fn bold_and_reset() {
        let mut s = Terminal::new(10, 1);
        s.feed(b"\x1b[1mB\x1b[22mN");
        assert!(s.grid().get(0, 0).style.bold);
        assert!(!s.grid().get(1, 0).style.bold);
    }

    #[test]
    fn alternate_screen_switch_preserves_primary() {
        let mut s = Terminal::new(10, 2);
        s.feed(b"primary");
        s.feed(b"\x1b[?1049h");
        assert!(s.is_alternate_screen());
        s.feed(b"alt");
        assert_eq!(s.plain_text()[0], "alt");
        s.feed(b"\x1b[?1049l");
        assert!(!s.is_alternate_screen());
        assert_eq!(s.plain_text()[0], "primary");
    }

    #[test]
    fn bracketed_paste_mode_toggle() {
        let mut s = Terminal::new(10, 1);
        assert!(!s.bracketed_paste);
        s.feed(b"\x1b[?2004h");
        assert!(s.bracketed_paste);
        s.feed(b"\x1b[?2004l");
        assert!(!s.bracketed_paste);
    }

    #[test]
    fn mouse_reporting_mode() {
        let mut s = Terminal::new(10, 1);
        s.feed(b"\x1b[?1000h");
        assert_eq!(s.mouse_reporting, MouseMode::Normal);
        s.feed(b"\x1b[?1000l");
        assert_eq!(s.mouse_reporting, MouseMode::Off);
    }

    #[test]
    fn osc_title_sets_title() {
        let mut s = Terminal::new(10, 1);
        s.feed(b"\x1b]0;My Title\x07");
        assert_eq!(s.title, "My Title");
    }

    #[test]
    fn osc7_directory_tracking() {
        let mut s = Terminal::new(10, 1);
        s.feed(b"\x1b]7;file://host/home/user\x07");
        assert_eq!(s.reported_cwd.as_deref(), Some("file://host/home/user"));
    }

    #[test]
    fn wide_character_takes_two_columns() {
        let mut s = Terminal::new(10, 1);
        s.feed("\u{4e2d}".as_bytes()); // CJK wide char
        assert_eq!(s.cursor_col, 2);
        assert_eq!(s.grid().get(1, 0).width, 0);
    }

    #[test]
    fn scroll_region_confines_scroll() {
        let mut s = Terminal::new(5, 5);
        s.feed(b"\x1b[2;4r"); // rows 2..4 scrollable
        s.feed(b"\x1b[5;1Hbotom"); // write outside region, shouldn't scroll region
        assert_eq!(s.plain_text()[4], "botom");
    }

    #[test]
    fn scrollback_grows_on_primary_scroll() {
        let mut s = Terminal::new(5, 2);
        s.feed(b"line1\r\nline2\r\nline3");
        assert_eq!(s.grid().total_scrollback_lines(), 1);
    }

    #[test]
    fn insert_mode_shifts_existing_text() {
        let mut s = Terminal::new(10, 1);
        s.feed(b"world");
        s.feed(b"\x1b[1G\x1b[4h"); // move to col 1, enable insert mode
        s.feed(b"hi ");
        assert_eq!(s.plain_text()[0], "hi world");
    }

    #[test]
    fn resize_preserves_top_left() {
        let mut s = Terminal::new(5, 2);
        s.feed(b"ab\r\ncd");
        s.resize(3, 3);
        assert_eq!(s.plain_text()[0], "ab");
        assert_eq!(s.plain_text()[1], "cd");
    }

    #[test]
    fn cursor_save_restore_dec() {
        let mut s = Terminal::new(10, 3);
        s.feed(b"\x1b[2;2H\x1b7");
        s.feed(b"\x1b[5;5H\x1b8");
        assert_eq!((s.cursor_col, s.cursor_row), (1, 1));
    }

    #[test]
    fn reverse_index_scrolls_down_at_top() {
        let mut s = Terminal::new(5, 3);
        s.feed(b"a\r\nb\r\nc");
        s.feed(b"\x1b[1;1H\x1bM"); // RI at top row
        assert_eq!(s.plain_text()[1], "a");
    }

    #[test]
    fn apply_accepts_externally_sourced_events() {
        // Demonstrates the point of the ut-ansi/ut-grid split: events don't
        // have to come from this terminal's own parser.
        let mut parser = AnsiParser::new();
        let events = parser.feed(b"hi\x1b[1m!");
        let mut s = Terminal::new(10, 1);
        for e in &events {
            s.apply(e);
        }
        assert_eq!(s.plain_text()[0], "hi!");
        assert!(s.grid().get(2, 0).style.bold);
    }
}
