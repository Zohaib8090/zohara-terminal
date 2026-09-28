//! `ut-grid`: terminal screen state.
//!
//! Where `ut-ansi` only knows how to *parse* bytes into events, this crate
//! knows what a terminal *is*: a grid of cells, a cursor with save/restore
//! and movement rules, scrolling regions, the alternate screen, and the
//! handful of modes (autowrap, insert, origin, bracketed paste, mouse
//! reporting) that change how later events are interpreted. [`Terminal`]
//! consumes [`ut_ansi::AnsiEvent`]s (either by calling [`Terminal::feed`]
//! with raw bytes, which runs them through an internal `ut-ansi` parser, or
//! by calling [`Terminal::apply`] directly with events from elsewhere —
//! e.g. a recorded session, or a fuzzer).

mod grid;
mod terminal;

pub use grid::{Cell, Color, Grid, Style};
pub use terminal::{CursorShape, MouseMode, Terminal};
