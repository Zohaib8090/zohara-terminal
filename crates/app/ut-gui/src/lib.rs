//! `ut-gui`: the real native OS window (Section 4 "Windows") this project
//! was missing — an `eframe`/`egui` front end over the exact same
//! `ut_tui::App` state machine `ut-tui` uses (panes, split layout, and the
//! Enter-time `ut-safety` confirmation gate), so nothing about pane
//! management or command safety had to be reimplemented or re-verified:
//! only rendering (`render.rs`, `Grid` → `egui::text::LayoutJob`) and input
//! translation (`main.rs`, `egui::Event` → `ut_input::Key`) are new.
//!
//! Verified with real rendered screenshots (not just "it compiles") taken
//! under a software-rendered `Xvfb` display in development — `ut-gui`'s
//! own `--screenshot <path>` flag (see `main.rs`) drives exactly that
//! capture, so it can be re-verified the same way at any time:
//! `xvfb-run -a cargo run -p ut-gui -- --screenshot /tmp/out.png --exit-after-frames 5`.

pub mod render;
pub mod settings;

pub use render::{grid_to_layout_job, grid_to_rows};
