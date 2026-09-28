//! `zohara-term`: a headless terminal screen model.
//!
//! This crate implements the parsing/state-machine half of Section 3
//! ("Terminal compatibility"): it turns a byte stream containing ANSI/VT
//! escape sequences into a grid of styled cells plus scrollback, without
//! touching a PTY or a GUI. A front end feeds it bytes read from a PTY
//! master and reads back [`Screen`] state to render.
//!
//! Implemented now: SGR (colors incl. 256/true-color, bold/italic/underline/
//! reverse), cursor movement, line/character editing (ED/EL/IL/DL/ICH/DCH),
//! scrolling regions, the alternate screen, bracketed paste mode toggling,
//! basic mouse-reporting mode toggling, wide characters, and scrollback.

mod grid;
mod screen;

pub use grid::{Cell, Color, Grid, Style};
pub use screen::{CursorShape, Screen};

/// Parse `bytes` into `screen`, updating its grid/cursor/modes in place.
pub fn feed(screen: &mut Screen, bytes: &[u8]) {
    screen.feed(bytes);
}
