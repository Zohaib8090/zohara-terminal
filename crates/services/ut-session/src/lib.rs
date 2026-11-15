//! `ut-session`: owns PTY-backed panes, tabs, and workspaces (Section 4).
//!
//! This is the layer that turns `ut-pty` + `ut-grid` + `ut-safety` into
//! something an app can actually drive: a [`Pane`] is one running shell
//! (a PTY child process plus the `Terminal` that renders its output), a
//! [`Tab`] holds one or more panes, and a [`Workspace`] holds one or more
//! tabs. Critically, [`Pane::submit_input`] is the **single write path**
//! into a shell — nothing in this codebase should call `ut_pty::Pty::write`
//! directly outside of this module, because this is where every input
//! source (typed, pasted, a plugin's `RunCommand`, a workflow step) gets
//! run through `ut-safety` before it reaches a real process. That's what
//! makes "Command safety" an architectural guarantee instead of a UI
//! feature someone can forget to wire up on a new input path.
//!
//! A future headless multiplexer daemon (Section 13, "Built-in
//! multiplexer") is this crate running without a GUI attached — sessions
//! outlive any particular window because nothing about `Pane`/`Tab`/
//! `Workspace` depends on `ut-ui`.

use std::io;
use ut_grid::Terminal;
use ut_pty::{Pty, PtyExitStatus, PtySize};
use ut_safety::{PrivilegeContext, Report, Risk};

/// The result of trying to send input to a pane's shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputOutcome {
    /// Written straight through (nothing risky enough to warrant a prompt).
    Written,
    /// Held back: the safety report has findings at or above the pane's
    /// confirmation threshold. The caller (UI layer) is expected to show
    /// `report` to the user and, if they confirm, call
    /// [`Pane::submit_input_forced`] with the same text.
    NeedsConfirmation(Report),
}

/// One running shell: a PTY child process plus the terminal state that
/// interprets its output.
pub struct Pane {
    pub id: u64,
    pty: Pty,
    pub terminal: Terminal,
    pub title: String,
    privilege: PrivilegeContext,
    /// Findings at or above this risk level block a plain `submit_input`
    /// and require `submit_input_forced` instead. Matches "Warning
    /// sensitivity settings" — a beginner profile might set this to `Low`,
    /// an advanced-user profile to `Critical` ("Advanced-user warning bypass").
    pub confirmation_threshold: Risk,
}

impl Pane {
    /// Spawn a new shell in a freshly created PTY.
    pub fn spawn(id: u64, cols: u16, rows: u16, cwd: Option<&str>, privilege: PrivilegeContext) -> io::Result<Self> {
        let pty = Pty::spawn_shell(PtySize::new(cols, rows), cwd)?;
        Ok(Pane {
            id,
            pty,
            terminal: Terminal::new(cols as usize, rows as usize),
            title: String::new(),
            privilege,
            confirmation_threshold: Risk::Medium,
        })
    }

    /// Spawn a specific program (not the login shell) behind a PTY. This
    /// is the path any "run this in a pane" feature sits on — an SSH
    /// session pane, a workflow step, a test fixture (`/bin/cat` echoing
    /// input back makes input-path tests deterministic, unlike a real
    /// shell's prompt/greeting).
    pub fn spawn_command(
        id: u64,
        program: &str,
        args: &[&str],
        cols: u16,
        rows: u16,
        cwd: Option<&str>,
        privilege: PrivilegeContext,
    ) -> io::Result<Self> {
        let pty = Pty::spawn(program, args, PtySize::new(cols, rows), cwd, &[])?;
        Ok(Pane {
            id,
            pty,
            terminal: Terminal::new(cols as usize, rows as usize),
            title: String::new(),
            privilege,
            confirmation_threshold: Risk::Medium,
        })
    }

    /// Read whatever output is currently available (non-blocking assumed to
    /// be configured by the caller on the fd, or blocking with a short
    /// buffer for a dedicated reader thread) and feed it into the terminal.
    /// Returns `Ok(false)` once the child has exited and there's nothing
    /// left to read.
    pub fn pump_output(&mut self, buf: &mut [u8]) -> io::Result<bool> {
        match self.pty.read(buf) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.terminal.feed(&buf[..n]);
                if let Some(title) = self.title_from_terminal() {
                    self.title = title;
                }
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(true),
            Err(e) => Err(e),
        }
    }

    /// Like [`Pane::pump_output`], but also returns the exact bytes read,
    /// for a caller that needs to re-broadcast the raw stream verbatim —
    /// e.g. `ut-daemon` forwarding live output to attached clients —
    /// rather than only updating `self.terminal`.
    ///
    /// Returns `Ok(None)` when the fd is non-blocking and no data is
    /// available yet (distinct from EOF, which is `Ok(Some(&[]))`) — a
    /// caller polling this on a non-blocking pane should treat `None` as
    /// "try again shortly", not "the shell exited".
    pub fn pump_output_captured<'b>(&mut self, buf: &'b mut [u8]) -> io::Result<Option<&'b [u8]>> {
        match self.pty.read(buf) {
            Ok(n) => {
                if n > 0 {
                    self.terminal.feed(&buf[..n]);
                    if let Some(title) = self.title_from_terminal() {
                        self.title = title;
                    }
                }
                Ok(Some(&buf[..n]))
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Set the underlying PTY master fd's blocking mode. A dedicated
    /// reader thread that also needs to interleave writes through the same
    /// `Mutex<Pane>` (as `ut-daemon` does) must use non-blocking mode and
    /// poll — holding the lock through a *blocking* read would starve any
    /// writer waiting on the same mutex.
    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let fd = self.pty.master_fd();
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            let new_flags = if nonblocking { flags | libc::O_NONBLOCK } else { flags & !libc::O_NONBLOCK };
            if libc::fcntl(fd, libc::F_SETFL, new_flags) < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn title_from_terminal(&self) -> Option<String> {
        if self.terminal.title.is_empty() {
            None
        } else {
            Some(self.terminal.title.clone())
        }
    }

    /// The single write path into this pane's shell. Runs `text` through
    /// `ut-safety` first; if nothing meets `confirmation_threshold`, writes
    /// immediately and returns `Written`. Otherwise writes nothing and
    /// returns the report for the caller to show the user.
    pub fn submit_input(&mut self, text: &str) -> io::Result<InputOutcome> {
        let report = ut_safety::analyze(text, &self.privilege);
        let blocks = report.highest_risk().map(|r| r >= self.confirmation_threshold).unwrap_or(false);
        if blocks {
            return Ok(InputOutcome::NeedsConfirmation(report));
        }
        self.pty.write(text.as_bytes())?;
        Ok(InputOutcome::Written)
    }

    /// Write `text` regardless of what `ut-safety` finds — only to be
    /// called after the UI has shown the report from `submit_input` and the
    /// user explicitly confirmed (matches "Type-to-confirm option" /
    /// "Command cancellation guidance": the caller is expected to have
    /// implemented the actual confirmation UI, not to call this blindly).
    pub fn submit_input_forced(&mut self, text: &str) -> io::Result<()> {
        self.pty.write(text.as_bytes())?;
        Ok(())
    }

    pub fn resize(&mut self, cols: u16, rows: u16) -> io::Result<()> {
        self.pty.resize(PtySize::new(cols, rows))?;
        self.terminal.resize(cols as usize, rows as usize);
        Ok(())
    }

    /// Write raw bytes straight to the shell, bypassing `ut-safety`
    /// entirely. This exists for a *low-level* passthrough channel — e.g.
    /// `ut-daemon`'s attach connection, which forwards a real terminal
    /// client's raw keystrokes exactly the way a real terminal does. A real
    /// terminal doesn't safety-check every byte either: it's the layer that
    /// *composes* a full command line (a line editor, a shell wrapper) that
    /// is responsible for calling `submit_input` on the assembled line
    /// before it's sent. Only use this when nothing between the keystroke
    /// source and this call has a chance to assemble a full command line.
    pub fn write_raw(&self, bytes: &[u8]) -> io::Result<()> {
        self.pty_write(bytes)
    }

    fn pty_write(&self, bytes: &[u8]) -> io::Result<()> {
        self.pty.write(bytes)?;
        Ok(())
    }

    pub fn try_wait(&mut self) -> io::Result<PtyExitStatus> {
        self.pty.try_wait()
    }

    pub fn hangup(&self) {
        self.pty.hangup()
    }
}

/// A tab: an ordered list of panes plus which one has focus. Full
/// tree-shaped split layouts (Section 4 "Panes": nested splits, zoom,
/// layout presets) are future work; this holds the flat pane list and
/// focus tracking those will be built on top of.
#[derive(Default)]
pub struct Tab {
    pub id: u64,
    pub name: String,
    panes: Vec<Pane>,
    active_pane: usize,
}

impl Tab {
    pub fn new(id: u64, name: impl Into<String>) -> Self {
        Tab { id, name: name.into(), panes: Vec::new(), active_pane: 0 }
    }

    pub fn add_pane(&mut self, pane: Pane) {
        self.panes.push(pane);
        self.active_pane = self.panes.len() - 1;
    }

    pub fn close_pane(&mut self, pane_id: u64) -> Option<Pane> {
        let idx = self.panes.iter().position(|p| p.id == pane_id)?;
        let removed = self.panes.remove(idx);
        if self.active_pane >= self.panes.len() {
            self.active_pane = self.panes.len().saturating_sub(1);
        }
        Some(removed)
    }

    pub fn panes(&self) -> &[Pane] {
        &self.panes
    }

    pub fn panes_mut(&mut self) -> &mut [Pane] {
        &mut self.panes
    }

    pub fn active_pane(&self) -> Option<&Pane> {
        self.panes.get(self.active_pane)
    }

    pub fn active_pane_mut(&mut self) -> Option<&mut Pane> {
        self.panes.get_mut(self.active_pane)
    }

    pub fn focus_pane(&mut self, pane_id: u64) -> bool {
        if let Some(idx) = self.panes.iter().position(|p| p.id == pane_id) {
            self.active_pane = idx;
            true
        } else {
            false
        }
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }
}

/// A workspace: an ordered list of tabs plus which one has focus. Matches
/// Section 4 "Workspaces" minus persistence (saving/restoring to disk is
/// `ut-profile`'s job, layered on top of this in-memory model).
#[derive(Default)]
pub struct Workspace {
    pub id: u64,
    pub name: String,
    tabs: Vec<Tab>,
    active_tab: usize,
}

impl Workspace {
    pub fn new(id: u64, name: impl Into<String>) -> Self {
        Workspace { id, name: name.into(), tabs: Vec::new(), active_tab: 0 }
    }

    pub fn add_tab(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
    }

    pub fn close_tab(&mut self, tab_id: u64) -> Option<Tab> {
        let idx = self.tabs.iter().position(|t| t.id == tab_id)?;
        let removed = self.tabs.remove(idx);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
        Some(removed)
    }

    pub fn tabs(&self) -> &[Tab] {
        &self.tabs
    }

    pub fn tabs_mut(&mut self) -> &mut [Tab] {
        &mut self.tabs
    }

    pub fn active_tab(&self) -> Option<&Tab> {
        self.tabs.get(self.active_tab)
    }

    pub fn active_tab_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active_tab)
    }

    pub fn focus_tab(&mut self, tab_id: u64) -> bool {
        if let Some(idx) = self.tabs.iter().position(|t| t.id == tab_id) {
            self.active_tab = idx;
            true
        } else {
            false
        }
    }
}

/// Monotonic ID allocator shared by panes/tabs/workspaces so IDs never collide within a session.
#[derive(Default)]
pub struct IdAllocator(u64);

impl IdAllocator {
    pub fn allocate(&mut self) -> u64 {
        self.0 += 1;
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn set_nonblocking(pane: &Pane) {
        unsafe {
            let fd = pane_fd(pane);
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }

    // Small helper since Pane doesn't expose the raw fd publicly on purpose;
    // tests reach in via the same crate to configure non-blocking reads.
    fn pane_fd(pane: &Pane) -> i32 {
        // SAFETY: test-only; Pane keeps `pty` private but in the same crate
        // we can still call the accessor ut-pty exposes.
        pane.pty_fd_for_test()
    }

    impl Pane {
        fn pty_fd_for_test(&self) -> i32 {
            self.pty.master_fd()
        }
    }

    fn drain_until(pane: &mut Pane, deadline: Duration, predicate: impl Fn(&Pane) -> bool) {
        let start = Instant::now();
        let mut buf = [0u8; 4096];
        loop {
            let _ = pane.pump_output(&mut buf);
            if predicate(pane) || start.elapsed() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn pane_spawn_uses_a_real_shell() {
        // Smoke-test the public Pane::spawn path (spawns $SHELL / /bin/sh),
        // distinct from spawn_test_pane below which pins a deterministic command.
        let mut pane = Pane::spawn(1, 80, 24, None, PrivilegeContext::local_normal_user()).expect("spawn");
        set_nonblocking(&pane);
        pane.hangup();
        let _ = pane.try_wait();
    }

    #[test]
    fn pane_receives_command_output() {
        let mut pane = spawn_test_pane(1, "/bin/sh", &["-c", "echo integration-ok"]);
        set_nonblocking(&pane);
        drain_until(&mut pane, Duration::from_secs(5), |p| p.terminal.plain_text().iter().any(|l| l.contains("integration-ok")));
        assert!(pane.terminal.plain_text().iter().any(|l| l.contains("integration-ok")));
    }

    #[test]
    fn pump_output_captured_returns_exact_bytes() {
        let mut pane = spawn_test_pane(1, "/bin/sh", &["-c", "echo captured-ok"]);
        set_nonblocking(&pane);
        let start = Instant::now();
        let mut seen = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match pane.pump_output_captured(&mut buf) {
                Ok(Some(chunk)) => seen.extend_from_slice(chunk),
                Ok(None) => {}
                Err(_) => break,
            }
            if String::from_utf8_lossy(&seen).contains("captured-ok") || start.elapsed() > Duration::from_secs(5) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(String::from_utf8_lossy(&seen).contains("captured-ok"));
    }

    fn spawn_test_pane(id: u64, cmd: &str, args: &[&str]) -> Pane {
        let pty = Pty::spawn(cmd, args, PtySize::new(80, 24), None, &[]).expect("spawn test pty");
        Pane {
            id,
            pty,
            terminal: Terminal::new(80, 24),
            title: String::new(),
            privilege: PrivilegeContext::local_normal_user(),
            confirmation_threshold: Risk::Medium,
        }
    }

    #[test]
    fn submit_input_writes_safe_commands_immediately() {
        let mut pane = spawn_test_pane(1, "/bin/cat", &[]);
        set_nonblocking(&pane);
        let outcome = pane.submit_input("hello\n").expect("submit");
        assert_eq!(outcome, InputOutcome::Written);
        drain_until(&mut pane, Duration::from_secs(5), |p| p.terminal.plain_text().iter().any(|l| l.contains("hello")));
        assert!(pane.terminal.plain_text().iter().any(|l| l.contains("hello")));
        pane.hangup();
    }

    #[test]
    fn submit_input_blocks_destructive_commands() {
        let mut pane = spawn_test_pane(1, "/bin/cat", &[]);
        let outcome = pane.submit_input("rm -rf /\n").expect("submit");
        match outcome {
            InputOutcome::NeedsConfirmation(report) => {
                assert!(report.highest_risk().unwrap() >= Risk::Critical);
            }
            InputOutcome::Written => panic!("expected rm -rf / to require confirmation"),
        }
        pane.hangup();
    }

    #[test]
    fn submit_input_forced_bypasses_the_check() {
        let mut pane = spawn_test_pane(1, "/bin/cat", &[]);
        set_nonblocking(&pane);
        pane.submit_input_forced("echo forced\n").expect("forced write");
        drain_until(&mut pane, Duration::from_secs(5), |p| p.terminal.plain_text().iter().any(|l| l.contains("forced")));
        assert!(pane.terminal.plain_text().iter().any(|l| l.contains("forced")));
        pane.hangup();
    }

    #[test]
    fn tab_tracks_active_pane_across_close() {
        let mut tab = Tab::new(1, "main");
        tab.add_pane(spawn_test_pane(1, "/bin/sh", &["-c", "sleep 1"]));
        tab.add_pane(spawn_test_pane(2, "/bin/sh", &["-c", "sleep 1"]));
        assert_eq!(tab.active_pane().unwrap().id, 2);
        tab.close_pane(2);
        assert_eq!(tab.active_pane().unwrap().id, 1);
        for p in tab.panes_mut() {
            p.hangup();
        }
    }

    #[test]
    fn workspace_tracks_active_tab() {
        let mut ws = Workspace::new(1, "default");
        ws.add_tab(Tab::new(1, "a"));
        ws.add_tab(Tab::new(2, "b"));
        assert_eq!(ws.active_tab().unwrap().id, 2);
        assert!(ws.focus_tab(1));
        assert_eq!(ws.active_tab().unwrap().id, 1);
        assert!(!ws.focus_tab(999));
    }

    #[test]
    fn id_allocator_never_repeats() {
        let mut ids = IdAllocator::default();
        let a = ids.allocate();
        let b = ids.allocate();
        assert_ne!(a, b);
    }
}
