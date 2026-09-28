//! `ut-tui`: an interactive, terminal-hosted multiplexer UI.
//!
//! This is the "toolkit chosen for now" front end referenced by `ut-input`/
//! `ut-renderer`/`ut-ui`'s doc comments: real split panes (via `ut-ui`'s
//! layout tree), each a real shell (via `ut-session::Pane`), rendered with
//! [ratatui](https://ratatui.rs) into whatever terminal `ut-tui` itself
//! runs in. It directly implements Section 13 "Multiplexing and
//! persistence" (named panes, split/zoom/close) as a real, usable program
//! — not the eventual native-windowed GUI (still future work), but the
//! same `core`/`services` stack that GUI would sit on.
//!
//! # Command safety on a raw PTY (read this before extending input handling)
//!
//! A real terminal doesn't parse the bytes it sends — the shell's own line
//! editor (readline/ZLE) owns the command line, and arrow-key history
//! recall, tab completion, and multi-line constructs all happen *inside*
//! the child process, invisibly to us. So [`App`]'s `pending_line` tracking
//! (below) is a **best-effort local echo of what we think is on the
//! line**, built by watching typed/backspaced characters since the last
//! Enter — it can desync from the shell's real buffer (e.g. after a
//! history recall via Up-arrow, or a paste containing control sequences).
//! It is run through `ut-safety` on Enter and can hold back a destructive-
//! looking line for confirmation, which is a real, useful safety net for
//! the common case (typing), but it is **not** a substitute for real shell
//! integration (a preexec/precmd hook) — that remains future work, noted
//! in the spec's beginner-mode "Destructive-command warnings" section.

mod render;
mod ui;

pub use render::{grid_to_text, pane_border_style};
pub use ui::draw;

use std::collections::HashMap;
use ut_grid::Terminal as GridTerminal;
use ut_input::{encode, EncodeModes, Key, Modifiers};
use ut_safety::{PrivilegeContext, Report};
use ut_session::{InputOutcome, Pane, Tab};
use ut_ui::{Direction as SplitDirection, Layout, LayoutState};

/// What the UI should currently show for the active pane's input line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum PromptState {
    #[default]
    Normal,
    /// A destructive-looking command is being held back; show `report` and
    /// wait for the user to confirm (send anyway) or cancel.
    Confirming { pending_line: String, report_summary: Vec<String> },
}

/// Everything `ut-tui` needs across one frame/event: the pane tree
/// (`ut-session::Tab`) plus the split layout over the same pane IDs
/// (`ut-ui::LayoutState`), plus per-pane best-effort input-line tracking.
pub struct App {
    pub tab: Tab,
    pub layout: LayoutState,
    pending_lines: HashMap<u64, String>,
    pub prompt: PromptState,
    privilege: PrivilegeContext,
    next_id: u64,
    pub should_quit: bool,
}

impl App {
    pub fn new(cols: u16, rows: u16) -> std::io::Result<Self> {
        let privilege = PrivilegeContext::local_normal_user();
        let pane = Pane::spawn(1, cols, rows, None, privilege.clone())?;
        let mut tab = Tab::new(1, "main");
        tab.add_pane(pane);
        Ok(App {
            tab,
            layout: LayoutState::new(Layout::leaf(1)),
            pending_lines: HashMap::new(),
            prompt: PromptState::Normal,
            privilege,
            next_id: 2,
            should_quit: false,
        })
    }

    fn active_pane_id(&self) -> Option<u64> {
        self.tab.active_pane().map(|p| p.id)
    }

    /// Pump all panes' output into their `Terminal`s. Call this on every
    /// tick/idle loop iteration; individual `Pane`s must already be in
    /// non-blocking mode (see `ut_session::Pane::set_nonblocking`) or this
    /// will stall on whichever pane has no output ready.
    pub fn pump_all(&mut self) {
        let mut buf = [0u8; 8192];
        for pane in self.tab.panes_mut() {
            let _ = pane.pump_output(&mut buf);
        }
    }

    pub fn split_active(&mut self, direction: SplitDirection, cols: u16, rows: u16) -> std::io::Result<()> {
        let Some(active_id) = self.active_pane_id() else { return Ok(()) };
        let new_id = self.next_id;
        self.next_id += 1;
        let pane = Pane::spawn(new_id, cols, rows, None, self.privilege.clone())?;
        self.tab.add_pane(pane);
        self.tab.focus_pane(new_id);
        self.layout.layout.split(active_id, new_id, direction);
        Ok(())
    }

    pub fn close_active(&mut self) {
        let Some(active_id) = self.active_pane_id() else { return };
        if let Some(pane) = self.tab.close_pane(active_id) {
            pane.hangup();
        }
        self.pending_lines.remove(&active_id);
        let placeholder = Layout::Leaf(u64::MAX);
        let current = std::mem::replace(&mut self.layout.layout, placeholder);
        match current.close(active_id) {
            Some(new_layout) => self.layout.layout = new_layout,
            None => self.should_quit = true,
        }
    }

    pub fn zoom_active(&mut self) {
        if let Some(id) = self.active_pane_id() {
            if self.layout.zoomed == Some(id) {
                self.layout.unzoom();
            } else {
                self.layout.zoom(id);
            }
        }
    }

    /// Handle one key press for the active pane. This is the pure,
    /// testable core of the event loop — `main.rs` only translates
    /// `crossterm::event::KeyEvent` into `(Key, Modifiers)` and calls this.
    pub fn handle_key(&mut self, key: Key, mods: Modifiers) {
        if let PromptState::Confirming { pending_line, .. } = self.prompt.clone() {
            match key {
                Key::Char('y') | Key::Char('Y') => {
                    self.force_send_line(&pending_line);
                    self.prompt = PromptState::Normal;
                }
                Key::Char('n') | Key::Char('N') | Key::Escape => {
                    self.prompt = PromptState::Normal;
                    if let Some(id) = self.active_pane_id() {
                        self.pending_lines.insert(id, String::new());
                    }
                }
                _ => {} // ignore other input while a confirmation is pending
            }
            return;
        }

        let Some(id) = self.active_pane_id() else { return };

        match key {
            Key::Char(c) if mods.is_none() => {
                self.pending_lines.entry(id).or_default().push(c);
                self.forward_raw(id, key, mods);
            }
            Key::Backspace => {
                if let Some(line) = self.pending_lines.get_mut(&id) {
                    line.pop();
                }
                self.forward_raw(id, key, mods);
            }
            Key::Char('c') if mods.ctrl => {
                self.pending_lines.insert(id, String::new());
                self.forward_raw(id, key, mods);
            }
            Key::Char('u') if mods.ctrl => {
                self.pending_lines.insert(id, String::new());
                self.forward_raw(id, key, mods);
            }
            Key::Enter => {
                let line = self.pending_lines.entry(id).or_default().clone();
                let report = ut_safety::analyze(&line, &self.privilege);
                let blocks = report.highest_risk().map(|r| r >= ut_safety::Risk::Medium).unwrap_or(false);
                if blocks && !line.trim().is_empty() {
                    self.prompt = PromptState::Confirming { pending_line: line, report_summary: summarize(&report) };
                } else {
                    self.forward_raw(id, key, mods);
                    self.pending_lines.insert(id, String::new());
                }
            }
            _ => self.forward_raw(id, key, mods),
        }
    }

    fn forward_raw(&mut self, id: u64, key: Key, mods: Modifiers) {
        if let Some(pane) = self.tab.panes_mut().iter_mut().find(|p| p.id == id) {
            let modes = EncodeModes {
                application_cursor_keys: pane.terminal.application_cursor_keys,
                application_keypad: pane.terminal.application_keypad,
            };
            let bytes = encode(key, mods, modes);
            let _ = pane.write_raw(&bytes);
        }
    }

    fn force_send_line(&mut self, line: &str) {
        let Some(id) = self.active_pane_id() else { return };
        if let Some(pane) = self.tab.panes_mut().iter_mut().find(|p| p.id == id) {
            match pane.submit_input(line) {
                Ok(InputOutcome::Written) => {}
                Ok(InputOutcome::NeedsConfirmation(_)) => {
                    // Already confirmed by the user via the TUI prompt — bypass explicitly.
                    let _ = pane.submit_input_forced(&format!("{line}\r"));
                }
                Err(_) => {}
            }
        }
        self.pending_lines.insert(id, String::new());
    }

    pub fn grid_for(&self, pane_id: u64) -> Option<&GridTerminal> {
        self.tab.panes().iter().find(|p| p.id == pane_id).map(|p| &p.terminal)
    }
}

fn summarize(report: &Report) -> Vec<String> {
    report.findings.iter().take(3).map(|f| format!("[{}] {}", f.risk, f.explanation)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // App::new spawns a real shell, which is exactly what ut-session's own
    // tests do; these tests exercise the pure input/state logic on top of it.

    #[test]
    fn typed_characters_accumulate_in_pending_line() {
        let mut app = App::new(80, 24).unwrap();
        let id = app.active_pane_id().unwrap();
        for c in "ls -la".chars() {
            app.handle_key(Key::Char(c), Modifiers::none());
        }
        assert_eq!(app.pending_lines.get(&id).unwrap(), "ls -la");
    }

    #[test]
    fn backspace_removes_last_character() {
        let mut app = App::new(80, 24).unwrap();
        let id = app.active_pane_id().unwrap();
        for c in "abc".chars() {
            app.handle_key(Key::Char(c), Modifiers::none());
        }
        app.handle_key(Key::Backspace, Modifiers::none());
        assert_eq!(app.pending_lines.get(&id).unwrap(), "ab");
    }

    #[test]
    fn ctrl_c_clears_pending_line() {
        let mut app = App::new(80, 24).unwrap();
        let id = app.active_pane_id().unwrap();
        for c in "rm -rf".chars() {
            app.handle_key(Key::Char(c), Modifiers::none());
        }
        app.handle_key(Key::Char('c'), Modifiers::ctrl());
        assert_eq!(app.pending_lines.get(&id).unwrap(), "");
    }

    #[test]
    fn enter_with_safe_line_forwards_and_clears() {
        let mut app = App::new(80, 24).unwrap();
        let id = app.active_pane_id().unwrap();
        for c in "ls".chars() {
            app.handle_key(Key::Char(c), Modifiers::none());
        }
        app.handle_key(Key::Enter, Modifiers::none());
        assert_eq!(app.prompt, PromptState::Normal);
        assert_eq!(app.pending_lines.get(&id).unwrap(), "");
    }

    #[test]
    fn enter_with_destructive_line_triggers_confirmation() {
        let mut app = App::new(80, 24).unwrap();
        for c in "rm -rf /".chars() {
            app.handle_key(Key::Char(c), Modifiers::none());
        }
        app.handle_key(Key::Enter, Modifiers::none());
        assert!(matches!(app.prompt, PromptState::Confirming { .. }));
    }

    #[test]
    fn confirming_with_n_cancels_and_returns_to_normal() {
        let mut app = App::new(80, 24).unwrap();
        let id = app.active_pane_id().unwrap();
        for c in "rm -rf /".chars() {
            app.handle_key(Key::Char(c), Modifiers::none());
        }
        app.handle_key(Key::Enter, Modifiers::none());
        assert!(matches!(app.prompt, PromptState::Confirming { .. }));
        app.handle_key(Key::Char('n'), Modifiers::none());
        assert_eq!(app.prompt, PromptState::Normal);
        assert_eq!(app.pending_lines.get(&id).unwrap(), "");
    }

    #[test]
    fn confirming_ignores_unrelated_keys() {
        let mut app = App::new(80, 24).unwrap();
        for c in "rm -rf /".chars() {
            app.handle_key(Key::Char(c), Modifiers::none());
        }
        app.handle_key(Key::Enter, Modifiers::none());
        let before = app.prompt.clone();
        app.handle_key(Key::Char('x'), Modifiers::none());
        assert_eq!(app.prompt, before); // still confirming, unaffected
    }

    #[test]
    fn split_creates_second_pane_in_layout() {
        let mut app = App::new(80, 24).unwrap();
        app.split_active(SplitDirection::Horizontal, 40, 24).unwrap();
        assert_eq!(app.tab.panes().len(), 2);
        assert_eq!(app.layout.layout.leaves().len(), 2);
    }

    #[test]
    fn close_active_removes_pane_and_updates_layout() {
        let mut app = App::new(80, 24).unwrap();
        app.split_active(SplitDirection::Vertical, 80, 12).unwrap();
        assert_eq!(app.tab.panes().len(), 2);
        app.close_active();
        assert_eq!(app.tab.panes().len(), 1);
        assert!(!app.should_quit);
    }

    #[test]
    fn closing_last_pane_sets_should_quit() {
        let mut app = App::new(80, 24).unwrap();
        app.close_active();
        assert!(app.should_quit);
    }

    #[test]
    fn zoom_toggles_on_and_off() {
        let mut app = App::new(80, 24).unwrap();
        app.split_active(SplitDirection::Horizontal, 40, 24).unwrap();
        let id = app.active_pane_id().unwrap();
        app.zoom_active();
        assert_eq!(app.layout.zoomed, Some(id));
        app.zoom_active();
        assert_eq!(app.layout.zoomed, None);
    }
}
