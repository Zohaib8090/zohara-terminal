//! The cell grid: colors, styles, and a fixed-size 2D buffer of cells,
//! plus scrollback storage.

use std::collections::VecDeque;

/// A terminal color: the 16 ANSI colors, an indexed 256-color palette
/// entry, a 24-bit true color, or "use the default".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// Text attributes set via SGR (`ESC [ ... m`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub faint: bool,
    pub italic: bool,
    pub underline: bool,
    pub double_underline: bool,
    pub blink: bool,
    pub reverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            fg: Color::Default,
            bg: Color::Default,
            bold: false,
            faint: false,
            italic: false,
            underline: false,
            double_underline: false,
            blink: false,
            reverse: false,
            hidden: false,
            strikethrough: false,
        }
    }
}

impl Style {
    /// Reset all attributes to defaults (SGR 0).
    pub fn reset(&mut self) {
        *self = Style::default();
    }
}

/// One cell in the grid: a display character (which may be a combined
/// grapheme's base char; combining marks are appended into `text` when the
/// cell holds more than a single `char`) plus its style and width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    /// The visible text for this cell. Usually one `char`; may hold a base
    /// character plus trailing combining marks.
    pub text: String,
    pub style: Style,
    /// Terminal column width: 1 for normal/narrow, 2 for wide (CJK, many
    /// emoji). A wide character's second column is a zero-width placeholder
    /// cell with `width == 0` so the grid stays rectangular.
    pub width: u8,
}

impl Cell {
    pub fn blank() -> Self {
        Cell { text: " ".to_string(), style: Style::default(), width: 1 }
    }

    pub fn blank_with_style(style: Style) -> Self {
        Cell { text: " ".to_string(), style, width: 1 }
    }

    pub fn placeholder(style: Style) -> Self {
        Cell { text: String::new(), style, width: 0 }
    }

    pub fn is_blank(&self) -> bool {
        self.text == " " && self.width == 1
    }
}

/// A rectangular buffer of cells with attached scrollback.
#[derive(Debug, Clone)]
pub struct Grid {
    pub cols: usize,
    pub rows: usize,
    cells: Vec<Cell>,
    pub scrollback: VecDeque<Vec<Cell>>,
    pub scrollback_limit: usize,
}

impl Grid {
    pub fn new(cols: usize, rows: usize) -> Self {
        Grid {
            cols,
            rows,
            cells: vec![Cell::blank(); cols * rows],
            scrollback: VecDeque::new(),
            scrollback_limit: 10_000,
        }
    }

    #[inline]
    fn idx(&self, col: usize, row: usize) -> usize {
        row * self.cols + col
    }

    pub fn get(&self, col: usize, row: usize) -> &Cell {
        &self.cells[self.idx(col, row)]
    }

    pub fn get_mut(&mut self, col: usize, row: usize) -> &mut Cell {
        let i = self.idx(col, row);
        &mut self.cells[i]
    }

    pub fn set(&mut self, col: usize, row: usize, cell: Cell) {
        let i = self.idx(col, row);
        self.cells[i] = cell;
    }

    pub fn row(&self, row: usize) -> &[Cell] {
        let start = row * self.cols;
        &self.cells[start..start + self.cols]
    }

    pub fn row_mut(&mut self, row: usize) -> &mut [Cell] {
        let start = row * self.cols;
        &mut self.cells[start..start + self.cols]
    }

    /// Clear a full row to blanks with the given style (used for erase-with-bg).
    pub fn clear_row(&mut self, row: usize, style: Style) {
        for c in self.row_mut(row) {
            *c = Cell::blank_with_style(style);
        }
    }

    /// Scroll the region `[top, bottom]` (inclusive, 0-indexed) up by `n`
    /// lines, pushing lines that leave `top` into scrollback only when the
    /// region is the full grid (i.e. it's the primary screen's normal
    /// scroll, not a partial scroll-region shuffle).
    pub fn scroll_up(&mut self, top: usize, bottom: usize, n: usize, style: Style, save_to_scrollback: bool) {
        let n = n.min(bottom - top + 1);
        for _ in 0..n {
            let first = self.row(top).to_vec();
            if save_to_scrollback && top == 0 {
                self.scrollback.push_back(first);
                while self.scrollback.len() > self.scrollback_limit {
                    self.scrollback.pop_front();
                }
            }
            for r in top..bottom {
                let next = self.row(r + 1).to_vec();
                self.row_mut(r).clone_from_slice(&next);
            }
            self.clear_row(bottom, style);
        }
    }

    /// Scroll the region down (used by RI / DL at top margin).
    pub fn scroll_down(&mut self, top: usize, bottom: usize, n: usize, style: Style) {
        let n = n.min(bottom - top + 1);
        for _ in 0..n {
            for r in (top + 1..=bottom).rev() {
                let prev = self.row(r - 1).to_vec();
                self.row_mut(r).clone_from_slice(&prev);
            }
            self.clear_row(top, style);
        }
    }

    /// Resize the grid, preserving the top-left content where possible.
    pub fn resize(&mut self, cols: usize, rows: usize, style: Style) {
        let mut new_cells = vec![Cell::blank_with_style(style); cols * rows];
        for r in 0..rows.min(self.rows) {
            for c in 0..cols.min(self.cols) {
                new_cells[r * cols + c] = self.get(c, r).clone();
            }
        }
        self.cells = new_cells;
        self.cols = cols;
        self.rows = rows;
    }

    pub fn total_scrollback_lines(&self) -> usize {
        self.scrollback.len()
    }
}
