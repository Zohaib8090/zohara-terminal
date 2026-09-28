//! `ut-pty`: spawn a shell (or any program) behind a pseudo-terminal.
//!
//! This is the *only* crate in `terminal-core` allowed to touch process
//! spawning, file descriptors, or `unsafe`. Everything above it (`ut-ansi`,
//! `ut-grid`) is pure and OS-agnostic; everything that wants a real,
//! running shell goes through here. That boundary is deliberate: it's the
//! one place a security review or a sandboxing policy needs to look.
//!
//! Unix-only (Arch Linux is the target platform per the spec).

use nix::pty::{openpty, Winsize};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{close, dup2, execvp, fork, setsid, ForkResult, Pid};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

/// Terminal dimensions in character cells (and, optionally, pixels — most
/// callers can leave the pixel fields at 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub rows: u16,
    pub cols: u16,
    pub pixel_width: u16,
    pub pixel_height: u16,
}

impl PtySize {
    pub fn new(cols: u16, rows: u16) -> Self {
        PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
    }

    fn to_winsize(self) -> Winsize {
        Winsize { ws_row: self.rows, ws_col: self.cols, ws_xpixel: self.pixel_width, ws_ypixel: self.pixel_height }
    }
}

/// How the child process ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtyExitStatus {
    Exited(i32),
    Signaled(i32),
    StillRunning,
}

/// A running (or exited-but-not-yet-reaped) child process attached to a PTY.
pub struct Pty {
    master: RawFd,
    child: Pid,
    reaped: Option<PtyExitStatus>,
}

// SAFETY: `master` is a plain fd number; `read`/`write`/`ioctl` on it from
// multiple threads is exactly what a terminal app does (one reader thread,
// one writer thread) and is safe at the OS level for a PTY master fd.
unsafe impl Send for Pty {}
unsafe impl Sync for Pty {}

fn io_err(e: nix::Error) -> io::Error {
    io::Error::from_raw_os_error(e as i32)
}

impl Pty {
    /// Spawn `command` with `args` behind a new PTY of the given size, with
    /// `env` applied on top of (and possibly overriding) the current
    /// process's environment, and `cwd` as the starting directory.
    pub fn spawn(command: &str, args: &[&str], size: PtySize, cwd: Option<&str>, env: &[(&str, &str)]) -> io::Result<Self> {
        let winsize = size.to_winsize();
        let ends = openpty(Some(&winsize), None).map_err(io_err)?;
        let master = ends.master;
        let slave = ends.slave;

        // Prepare everything that allocates *before* forking, so the child
        // does as little as possible before exec.
        let c_command = CString::new(command).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let mut c_argv: Vec<CString> = Vec::with_capacity(args.len() + 1);
        c_argv.push(c_command.clone());
        for a in args {
            c_argv.push(CString::new(*a).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?);
        }
        let c_cwd = cwd.map(CString::new).transpose().map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let env_cstrings: Vec<CString> = std::env::vars()
            .filter(|(k, _)| !env.iter().any(|(ek, _)| ek == k))
            .chain(env.iter().map(|(k, v)| (k.to_string(), v.to_string())))
            .map(|(k, v)| CString::new(format!("{k}={v}")).unwrap_or_else(|_| CString::new("").unwrap()))
            .collect();

        let slave_fd = slave.as_raw_fd();
        let master_fd = master.as_raw_fd();

        // SAFETY: between fork() and execvp() in the child branch we only
        // call async-signal-safe libc/nix functions (setsid, ioctl, dup2,
        // close, chdir, execvp) and touch no Rust heap allocations beyond
        // ones already made above.
        match unsafe { fork() }.map_err(io_err)? {
            ForkResult::Parent { child } => {
                drop(slave); // parent doesn't need the slave end
                let master_raw = master.as_raw_fd();
                std::mem::forget(master); // we manage this fd's lifetime ourselves (closed in Drop)
                Ok(Pty { master: master_raw, child, reaped: None })
            }
            ForkResult::Child => {
                // New session + controlling terminal.
                let _ = setsid();
                unsafe {
                    if libc::ioctl(slave_fd, libc::TIOCSCTTY as _, 0) != 0 {
                        libc::_exit(127);
                    }
                }
                let _ = dup2(slave_fd, 0);
                let _ = dup2(slave_fd, 1);
                let _ = dup2(slave_fd, 2);
                if slave_fd > 2 {
                    let _ = close(slave_fd);
                }
                let _ = close(master_fd);

                if let Some(dir) = &c_cwd {
                    unsafe {
                        libc::chdir(dir.as_ptr());
                    }
                }
                for kv in &env_cstrings {
                    unsafe {
                        libc::putenv(kv.as_ptr() as *mut libc::c_char);
                    }
                }

                let _ = execvp(&c_command, &c_argv);
                // execvp only returns on failure.
                unsafe { libc::_exit(127) };
            }
        }
    }

    /// Convenience: spawn the user's `$SHELL` (or `/bin/sh` as a fallback)
    /// as an interactive login-ish shell.
    pub fn spawn_shell(size: PtySize, cwd: Option<&str>) -> io::Result<Self> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        Self::spawn(&shell, &[], size, cwd, &[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")])
    }

    /// The master-side file descriptor, for callers that want to select()/poll() on it directly.
    pub fn master_fd(&self) -> RawFd {
        self.master
    }

    pub fn pid(&self) -> i32 {
        self.child.as_raw()
    }

    /// Read available output from the child. Blocks if the fd is in
    /// blocking mode (the default); returns `Ok(0)` on EOF (child exited
    /// and closed its end).
    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let n = unsafe { libc::read(self.master, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let err = io::Error::last_os_error();
            // On Linux, once every fd referencing the slave side has
            // closed (the child exited and nothing else has it open),
            // read() on the master returns EIO rather than 0. Callers
            // expect the usual Ok(0)-means-EOF convention, so translate it
            // here instead of making every caller special-case this.
            if err.raw_os_error() == Some(libc::EIO) {
                Ok(0)
            } else {
                Err(err)
            }
        } else {
            Ok(n as usize)
        }
    }

    /// Write input (keystrokes, pasted text) to the child.
    pub fn write(&self, data: &[u8]) -> io::Result<usize> {
        let n = unsafe { libc::write(self.master, data.as_ptr() as *const libc::c_void, data.len()) };
        if n < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(n as usize)
        }
    }

    /// Notify the child of a terminal resize (SIGWINCH is sent automatically by the kernel).
    pub fn resize(&self, size: PtySize) -> io::Result<()> {
        let ws = size.to_winsize();
        let r = unsafe { libc::ioctl(self.master, libc::TIOCSWINSZ, &ws as *const Winsize) };
        if r != 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Non-blocking check for whether the child has exited, without blocking.
    pub fn try_wait(&mut self) -> io::Result<PtyExitStatus> {
        if let Some(status) = self.reaped {
            return Ok(status);
        }
        match waitpid(self.child, Some(WaitPidFlag::WNOHANG)).map_err(io_err)? {
            WaitStatus::Exited(_, code) => {
                self.reaped = Some(PtyExitStatus::Exited(code));
                Ok(PtyExitStatus::Exited(code))
            }
            WaitStatus::Signaled(_, sig, _) => {
                let status = PtyExitStatus::Signaled(sig as i32);
                self.reaped = Some(status);
                Ok(status)
            }
            _ => Ok(PtyExitStatus::StillRunning),
        }
    }

    /// Block until the child exits and reap it.
    pub fn wait(&mut self) -> io::Result<PtyExitStatus> {
        if let Some(status) = self.reaped {
            return Ok(status);
        }
        match waitpid(self.child, None).map_err(io_err)? {
            WaitStatus::Exited(_, code) => {
                self.reaped = Some(PtyExitStatus::Exited(code));
                Ok(PtyExitStatus::Exited(code))
            }
            WaitStatus::Signaled(_, sig, _) => {
                let status = PtyExitStatus::Signaled(sig as i32);
                self.reaped = Some(status);
                Ok(status)
            }
            other => {
                // Shouldn't happen without WNOHANG/WUNTRACED, but stay safe.
                let _ = other;
                self.wait()
            }
        }
    }

    /// Ask the child to terminate (SIGHUP, matching what a closed terminal does).
    pub fn hangup(&self) {
        unsafe {
            libc::kill(self.child.as_raw(), libc::SIGHUP);
        }
    }

    /// Forcefully terminate the child (SIGKILL).
    pub fn kill(&self) {
        unsafe {
            libc::kill(self.child.as_raw(), libc::SIGKILL);
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.master);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn read_until(pty: &Pty, deadline: Duration, predicate: impl Fn(&str) -> bool) -> String {
        let start = Instant::now();
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        // Use a short poll loop; the fd is blocking, so cap total attempts by wall clock.
        unsafe {
            let flags = libc::fcntl(pty.master_fd(), libc::F_GETFL);
            libc::fcntl(pty.master_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        loop {
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(_) => break,
            }
            let text = String::from_utf8_lossy(&out);
            if predicate(&text) || start.elapsed() > deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn spawns_and_echoes_output() {
        let mut pty = Pty::spawn("/bin/sh", &["-c", "echo hello-from-pty"], PtySize::new(80, 24), None, &[])
            .expect("spawn /bin/sh");
        let out = read_until(&pty, Duration::from_secs(5), |s| s.contains("hello-from-pty"));
        assert!(out.contains("hello-from-pty"), "output was: {out:?}");
        let status = pty.wait().expect("wait");
        assert_eq!(status, PtyExitStatus::Exited(0));
    }

    #[test]
    fn write_reaches_child_stdin() {
        let mut pty = Pty::spawn("/bin/cat", &[], PtySize::new(80, 24), None, &[]).expect("spawn cat");
        pty.write(b"round-trip\n").expect("write");
        let out = read_until(&pty, Duration::from_secs(5), |s| s.contains("round-trip"));
        assert!(out.contains("round-trip"), "output was: {out:?}");
        pty.hangup();
        let _ = pty.wait();
    }

    #[test]
    fn resize_does_not_error() {
        let mut pty = Pty::spawn("/bin/sh", &["-c", "sleep 0.2"], PtySize::new(80, 24), None, &[]).expect("spawn");
        pty.resize(PtySize::new(100, 40)).expect("resize");
        let _ = pty.wait();
    }

    #[test]
    fn try_wait_reports_exit_status() {
        let mut pty = Pty::spawn("/bin/sh", &["-c", "exit 3"], PtySize::new(80, 24), None, &[]).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match pty.try_wait().expect("try_wait") {
                PtyExitStatus::StillRunning if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                status => {
                    assert_eq!(status, PtyExitStatus::Exited(3));
                    break;
                }
            }
        }
    }
}
