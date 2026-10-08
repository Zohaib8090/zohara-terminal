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

//! # Dynamic plugins (read before extending this integration)
//!
//! `App::load_plugins_from_dir` loads `<name>.so` + `<name>.manifest.json`
//! pairs through `ut-plugin-loader::load_verified` — nothing runs unless
//! its artifact hash and signature check out against a key `App::
//! trust_plugin_publisher` was told to trust. A loaded plugin sees every
//! command right after this app's own safety gate decides to forward it
//! (`notify_plugins_of_command`), and every raw output chunk via
//! `pump_all`. It can never cause a command to run, even via
//! `PluginAction::RunCommand` — dynamic plugins have no `PluginContext`
//! and no write-access grant in this version (see `ut-plugin-loader`'s
//! module docs for why), so every action besides `ShowMessage` is logged
//! to `plugin_messages` as a suggestion, not executed.

mod render;
mod ui;

pub use render::{grid_to_text, pane_border_style};
pub use ui::draw;

use std::collections::HashMap;
use std::path::Path;
use ut_grid::Terminal as GridTerminal;
use ut_input::{encode, EncodeModes, Key, Modifiers};
use ut_plugin_api::PluginAction;
use ut_plugin_loader::LoadedPlugin;
use ut_plugin_signing::{Keyring, SignedManifest};
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
    /// Dynamically-loaded plugins (see `ut-plugin-loader`), driven from
    /// `handle_key`'s Enter path (`on_command`) and `pump_all` (`on_output`).
    plugins: Vec<LoadedPlugin>,
    /// Publisher keys this app instance trusts for loading plugins.
    plugin_keyring: Keyring,
    /// Human-readable log of plugin activity: load successes/failures and
    /// every `ShowMessage`/`SuggestCommand`/`RunCommand` a plugin returned.
    /// A front end surfaces this however it likes (status line, toast,
    /// side panel) — `App` itself has no UI opinion.
    pub plugin_messages: Vec<String>,
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
            plugins: Vec::new(),
            plugin_keyring: Keyring::new(),
            plugin_messages: Vec::new(),
        })
    }

    /// Trust a publisher's signing key for dynamically-loaded plugins.
    /// Must be called before [`App::load_plugins_from_dir`] for that
    /// publisher's plugins to pass verification — see
    /// `ut-plugin-signing::Keyring::trust`.
    pub fn trust_plugin_publisher(&mut self, public_key_hex: &str, label: &str) {
        let _ = self.plugin_keyring.trust(public_key_hex, label);
    }

    /// Scan `dir` for `<name>.so` files with a sibling `<name>.manifest.json`
    /// holding a JSON-encoded `ut_plugin_signing::SignedManifest`, and load
    /// the ones that verify against this app's trusted keyring via
    /// `ut_plugin_loader::load_verified`. A plugin that's missing its
    /// manifest, fails to parse, or fails verification is skipped — not
    /// fatal to the rest of the scan — with the reason recorded in
    /// [`App::plugin_messages`]. Returns how many plugins actually loaded.
    ///
    /// # Safety
    /// This loads and executes native code from every verified `.so` in
    /// `dir`. Only point it at a directory the user actually intends to
    /// load plugins from — verification proves a plugin is untampered and
    /// from a trusted publisher, not that the publisher's code is safe to
    /// run unsupervised (see `ut-plugin-loader`'s module docs).
    pub unsafe fn load_plugins_from_dir(&mut self, dir: &Path) -> usize {
        let mut loaded = 0;
        let Ok(entries) = std::fs::read_dir(dir) else {
            self.plugin_messages.push(format!("plugin directory unreadable: {}", dir.display()));
            return 0;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("so") {
                continue;
            }
            match load_one_plugin(&path, &self.plugin_keyring) {
                Ok(plugin) => {
                    self.plugin_messages.push(format!("loaded plugin: {}", plugin.id()));
                    self.plugins.push(plugin);
                    loaded += 1;
                }
                Err(reason) => self.plugin_messages.push(format!("plugin not loaded ({}): {reason}", path.display())),
            }
        }
        loaded
    }

    /// Notify every loaded plugin that `command` is about to run (after
    /// this app's own safety gate already decided to forward it) and
    /// record whatever `PluginAction` each one returns. A plugin can never
    /// cause a command to run by returning `RunCommand` here — dynamic
    /// plugins have no write-access permission grant in this version, so
    /// it's surfaced as a message, never executed, matching "never execute
    /// silently" for in-process plugins too.
    fn notify_plugins_of_command(&mut self, command: &str) {
        for plugin in &mut self.plugins {
            let action = plugin.on_command(command);
            record_plugin_action(&mut self.plugin_messages, &action);
        }
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
        let has_plugins = !self.plugins.is_empty();
        let mut chunks: Vec<Vec<u8>> = Vec::new();
        for pane in self.tab.panes_mut() {
            if has_plugins {
                if let Ok(Some(data)) = pane.pump_output_captured(&mut buf) {
                    if !data.is_empty() {
                        chunks.push(data.to_vec());
                    }
                }
            } else {
                let _ = pane.pump_output(&mut buf);
            }
        }
        for chunk in &chunks {
            let text = String::from_utf8_lossy(chunk);
            for plugin in &mut self.plugins {
                let action = plugin.on_output(&text);
                record_plugin_action(&mut self.plugin_messages, &action);
            }
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
                    if !line.trim().is_empty() {
                        self.notify_plugins_of_command(&line);
                    }
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
        if !line.trim().is_empty() {
            self.notify_plugins_of_command(line);
        }
    }

    pub fn grid_for(&self, pane_id: u64) -> Option<&GridTerminal> {
        self.tab.panes().iter().find(|p| p.id == pane_id).map(|p| &p.terminal)
    }
}

fn summarize(report: &Report) -> Vec<String> {
    report.findings.iter().take(3).map(|f| format!("[{}] {}", f.risk, f.explanation)).collect()
}

/// Load and verify one `<name>.so` against its sibling
/// `<name>.manifest.json`. Pulled out of `App::load_plugins_from_dir` so
/// each candidate's failure is independently caught (one malformed
/// manifest can't abort the whole directory scan).
///
/// # Safety
/// Executes native code from `path` once verification passes — same
/// caveat as `ut_plugin_loader::load_verified`.
unsafe fn load_one_plugin(path: &Path, keyring: &Keyring) -> Result<LoadedPlugin, String> {
    let manifest_path = path.with_extension("manifest.json");
    let manifest_bytes =
        std::fs::read(&manifest_path).map_err(|e| format!("missing/unreadable manifest {}: {e}", manifest_path.display()))?;
    let signed: SignedManifest = serde_json::from_slice(&manifest_bytes).map_err(|e| format!("malformed manifest: {e}"))?;
    ut_plugin_loader::load_verified(path, &signed, keyring).map_err(|e| e.to_string())
}

/// Turn a plugin's returned `PluginAction` into a human-readable log line.
/// `RunCommand`/`SuggestCommand` are deliberately never acted on here —
/// dynamic plugins have no write-access permission grant in this version,
/// so the strongest thing they can do is ask to be noticed, the same
/// "never execute silently" rule `ut-plugin-api::PluginContext` enforces
/// for in-process plugins.
fn record_plugin_action(messages: &mut Vec<String>, action: &PluginAction) {
    match action {
        PluginAction::None => {}
        PluginAction::ShowMessage(s) => messages.push(s.clone()),
        PluginAction::SuggestCommand(s) => messages.push(format!("plugin suggests (not run): {s}")),
        PluginAction::RunCommand(s) => messages.push(format!("plugin asked to run (not run — no permission grant): {s}")),
    }
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

    /// End-to-end proof that a real dynamically-loaded plugin actually
    /// sees commands and output through `App`, not just that
    /// `ut-plugin-loader` can load a `.so` in isolation (already covered
    /// by that crate's own tests). Builds the same fixture cdylib used
    /// there, signs it, trusts the signer, drops a `.so` +
    /// `.manifest.json` pair into a scratch directory, and drives `App`
    /// through its real `handle_key`/`pump_all` paths.
    #[test]
    fn loaded_plugin_is_notified_of_real_commands_and_output() {
        use std::time::Duration;
        use ut_plugin_signing::{hash_artifact, Manifest, PluginKeypair};

        let built_so = build_fixture_cdylib();

        let scratch = std::env::temp_dir().join(format!("ut-tui-plugin-test-{}", std::process::id()));
        std::fs::create_dir_all(&scratch).unwrap();
        let so_path = scratch.join("echofix.so");
        std::fs::copy(&built_so, &so_path).unwrap();

        let bytes = std::fs::read(&so_path).unwrap();
        let keypair = PluginKeypair::generate();
        let manifest = Manifest {
            id: "echo-fixture".into(),
            name: "Echo Fixture".into(),
            version: "1.0.0".into(),
            author: "test".into(),
            requested_capabilities: vec![],
            artifact_sha256: hash_artifact(&bytes),
        };
        let signed = keypair.sign(manifest);
        std::fs::write(so_path.with_extension("manifest.json"), serde_json::to_vec(&signed).unwrap()).unwrap();

        let mut app = App::new(80, 24).unwrap();
        app.trust_plugin_publisher(&keypair.public_key_hex(), "official");
        let loaded = unsafe { app.load_plugins_from_dir(&scratch) };
        let _ = std::fs::remove_dir_all(&scratch);

        assert_eq!(loaded, 1);
        assert!(app.plugin_messages.iter().any(|m| m == "loaded plugin: echo-fixture"));

        for c in "deploy".chars() {
            app.handle_key(Key::Char(c), Modifiers::none());
        }
        app.handle_key(Key::Enter, Modifiers::none());
        assert!(
            app.plugin_messages.iter().any(|m| m == "plugin suggests (not run): echo: deploy"),
            "expected a plugin suggestion from on_command, got: {:?}",
            app.plugin_messages
        );

        // Give the real shell a moment to actually produce output, then
        // pump it through — this is a live /bin/sh, not a mock.
        std::thread::sleep(Duration::from_millis(200));
        app.pump_all();
        assert!(
            app.plugin_messages.iter().any(|m| m.starts_with("saw output: ")),
            "expected on_output to have fired at least once, got: {:?}",
            app.plugin_messages
        );
    }

    /// Builds `ut-plugin-loader-fixture` as a cdylib via a real `cargo
    /// build` subprocess, mirroring `ut-plugin-loader`'s own test helper —
    /// duplicated rather than shared because it's a handful of lines and
    /// pulling in a test-only cross-crate helper dependency isn't worth it.
    fn build_fixture_cdylib() -> std::path::PathBuf {
        let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = manifest_dir.parent().and_then(|p| p.parent()).and_then(|p| p.parent()).expect("workspace root");

        let output = std::process::Command::new(env!("CARGO"))
            .args(["build", "-p", "ut-plugin-loader-fixture", "--lib"])
            .current_dir(workspace_root)
            .output()
            .expect("run cargo build for fixture crate");
        assert!(
            output.status.success(),
            "fixture cdylib build failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let target_dir = workspace_root.join("target/debug");
        for name in ["libut_plugin_loader_fixture.so", "libut_plugin_loader_fixture.dylib", "ut_plugin_loader_fixture.dll"] {
            let candidate = target_dir.join(name);
            if candidate.exists() {
                return candidate;
            }
        }
        panic!("built fixture crate but couldn't find its cdylib under {}", target_dir.display());
    }
}
