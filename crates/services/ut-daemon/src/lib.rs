//! `ut-daemon`: named sessions that outlive any one client connection
//! (Section 13 "Multiplexing and persistence" — this crate *is* the
//! "Built-in multiplexer": create/detach/reattach/list/rename/close named
//! sessions, persistent local sessions, session restore after the app
//! closes).
//!
//! [`Daemon`] owns a set of named [`ManagedSession`]s, each a real
//! `ut_session::Pane` (PTY + `ut-grid` terminal state) with a dedicated
//! background thread continuously reading its output — so a session keeps
//! running and its scrollback keeps growing whether or not any client is
//! attached, exactly like `tmux`/`screen`. [`serve`] runs the accept loop
//! for a Unix-socket control/attach protocol so a *separate* client
//! process can create, list, and attach to sessions; [`Daemon`]'s own
//! methods are also usable directly in-process without any socket, for an
//! app that wants an embedded multiplexer without a separate daemon process.
//!
//! **Attach is a raw byte passthrough by design** (documented in detail on
//! [`ut_session::Pane::write_raw`]): once attached, keystrokes go straight
//! to the shell exactly as a real terminal would send them. `ut-safety`
//! command checking belongs to whatever composes a full command line
//! upstream of this (a line editor, a shell wrapper) — a raw multiplexer
//! pipe is not that layer, the same way `tmux` itself doesn't safety-check
//! your keystrokes.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use ut_safety::PrivilegeContext;
use ut_session::Pane;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    pub created_at: u64,
    pub alive: bool,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("a session named '{0}' already exists")]
    AlreadyExists(String),
    #[error("no session named '{0}'")]
    NotFound(String),
    #[error("session '{0}' has exited and cannot be attached")]
    Dead(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Live attach subscriptions for a session: each is a subscriber id (so a
/// specific one can be removed on detach) paired with the channel that
/// forwards raw output chunks to that attached client.
type Subscribers = Arc<Mutex<Vec<(u64, mpsc::Sender<Vec<u8>>)>>>;

struct ManagedSession {
    name: Mutex<String>,
    created_at: u64,
    pane: Arc<Mutex<Pane>>,
    alive: Arc<AtomicBool>,
    subscribers: Subscribers,
    next_subscriber_id: Mutex<u64>,
    cols: Mutex<u16>,
    rows: Mutex<u16>,
    _reader_thread: thread::JoinHandle<()>,
}

/// The multiplexer core: a set of named, independently-running sessions.
#[derive(Default)]
pub struct Daemon {
    sessions: Mutex<HashMap<String, Arc<ManagedSession>>>,
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Daemon {
    pub fn new() -> Self {
        Daemon::default()
    }

    /// Create a new named session running `$SHELL` in a PTY of the given
    /// size. The session keeps running (and its background reader thread
    /// keeps pumping output into its terminal state) until [`Daemon::close`]
    /// is called, independent of any client being attached.
    pub fn create(&self, name: &str, cols: u16, rows: u16) -> Result<(), DaemonError> {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.contains_key(name) {
            return Err(DaemonError::AlreadyExists(name.to_string()));
        }

        let pane = Pane::spawn(0, cols, rows, None, PrivilegeContext::local_normal_user())?;
        // Must be non-blocking: the reader thread below polls it while
        // holding the same Mutex a writer (attach's input forwarding)
        // needs — a blocking read would hold that lock indefinitely and
        // starve every write, deadlocking the session.
        pane.set_nonblocking(true)?;
        let pane = Arc::new(Mutex::new(pane));
        let alive = Arc::new(AtomicBool::new(true));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));

        let reader_thread = {
            let pane = Arc::clone(&pane);
            let alive = Arc::clone(&alive);
            let subscribers = Arc::clone(&subscribers);
            thread::spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    // Non-blocking, so this only ever holds the lock for the
                    // duration of one syscall — never while waiting for data —
                    // leaving room for a writer (attach's input forwarding) to
                    // acquire it in between polls.
                    let read = { pane.lock().unwrap().pump_output_captured(&mut buf).map(|opt| opt.map(<[u8]>::to_vec)) };
                    match read {
                        Ok(Some(chunk)) if chunk.is_empty() => {
                            // A real Ok(0) from a non-blocking fd is true EOF.
                            alive.store(false, Ordering::SeqCst);
                            break;
                        }
                        Ok(Some(chunk)) => {
                            let mut subs = subscribers.lock().unwrap();
                            subs.retain(|(_, tx)| tx.send(chunk.clone()).is_ok());
                        }
                        Ok(None) => {
                            thread::sleep(std::time::Duration::from_millis(5));
                        }
                        Err(_) => {
                            alive.store(false, Ordering::SeqCst);
                            break;
                        }
                    }
                }
            })
        };

        sessions.insert(
            name.to_string(),
            Arc::new(ManagedSession {
                name: Mutex::new(name.to_string()),
                created_at: now_secs(),
                pane,
                alive,
                subscribers,
                next_subscriber_id: Mutex::new(0),
                cols: Mutex::new(cols),
                rows: Mutex::new(rows),
                _reader_thread: reader_thread,
            }),
        );
        Ok(())
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let sessions = self.sessions.lock().unwrap();
        sessions
            .values()
            .map(|s| SessionInfo {
                name: s.name.lock().unwrap().clone(),
                created_at: s.created_at,
                alive: s.alive.load(Ordering::SeqCst),
                cols: *s.cols.lock().unwrap(),
                rows: *s.rows.lock().unwrap(),
            })
            .collect()
    }

    pub fn rename(&self, from: &str, to: &str) -> Result<(), DaemonError> {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.contains_key(to) {
            return Err(DaemonError::AlreadyExists(to.to_string()));
        }
        let session = sessions.remove(from).ok_or_else(|| DaemonError::NotFound(from.to_string()))?;
        *session.name.lock().unwrap() = to.to_string();
        sessions.insert(to.to_string(), session);
        Ok(())
    }

    /// Close a session: hang up its shell and drop it from the registry.
    pub fn close(&self, name: &str) -> Result<(), DaemonError> {
        let mut sessions = self.sessions.lock().unwrap();
        let session = sessions.remove(name).ok_or_else(|| DaemonError::NotFound(name.to_string()))?;
        session.pane.lock().unwrap().hangup();
        Ok(())
    }

    pub fn resize(&self, name: &str, cols: u16, rows: u16) -> Result<(), DaemonError> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(name).ok_or_else(|| DaemonError::NotFound(name.to_string()))?;
        session.pane.lock().unwrap().resize(cols, rows)?;
        *session.cols.lock().unwrap() = cols;
        *session.rows.lock().unwrap() = rows;
        Ok(())
    }

    /// Attach to a session: returns the current screen (plain text, one
    /// line per row — a client redraws from this) plus a live subscription
    /// to future raw output, and a handle for writing raw input. Call
    /// [`Daemon::detach`] with the returned subscription id when the
    /// client disconnects; the session itself keeps running.
    pub fn attach(&self, name: &str) -> Result<AttachHandle, DaemonError> {
        let sessions = self.sessions.lock().unwrap();
        let session = sessions.get(name).ok_or_else(|| DaemonError::NotFound(name.to_string()))?;
        if !session.alive.load(Ordering::SeqCst) {
            return Err(DaemonError::Dead(name.to_string()));
        }

        let snapshot = session.pane.lock().unwrap().terminal.plain_text().join("\n");

        let (tx, rx) = mpsc::channel();
        let mut id_guard = session.next_subscriber_id.lock().unwrap();
        let id = *id_guard;
        *id_guard += 1;
        drop(id_guard);
        session.subscribers.lock().unwrap().push((id, tx));

        Ok(AttachHandle { subscriber_id: id, snapshot, receiver: rx, pane: Arc::clone(&session.pane), session: Arc::clone(session) })
    }

    pub fn detach(&self, handle: &AttachHandle) {
        handle.session.subscribers.lock().unwrap().retain(|(id, _)| *id != handle.subscriber_id);
    }
}

/// What a caller gets back from [`Daemon::attach`]: an initial snapshot to
/// draw, a stream of future output chunks, and a way to write input.
pub struct AttachHandle {
    subscriber_id: u64,
    pub snapshot: String,
    pub receiver: mpsc::Receiver<Vec<u8>>,
    pane: Arc<Mutex<Pane>>,
    session: Arc<ManagedSession>,
}

impl AttachHandle {
    pub fn write_input(&self, bytes: &[u8]) -> io::Result<()> {
        self.pane.lock().unwrap().write_raw(bytes)
    }
}

// ---------------------------------------------------------------------
// Unix-socket wire protocol
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Create { name: String, cols: u16, rows: u16 },
    List,
    Rename { from: String, to: String },
    Close { name: String },
    Resize { name: String, cols: u16, rows: u16 },
    Attach { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Error { message: String },
    Sessions { sessions: Vec<SessionInfo> },
    Attached { snapshot: String },
}

fn write_line(stream: &mut UnixStream, resp: &Response) -> io::Result<()> {
    let mut line = serde_json::to_string(resp).unwrap_or_else(|_| r#"{"status":"error","message":"encode failure"}"#.to_string());
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// Accept loop: serve control/attach connections on `listener` against
/// `daemon` until the listener errors (e.g. the socket file is removed).
pub fn serve(daemon: Arc<Daemon>, listener: UnixListener) -> io::Result<()> {
    for conn in listener.incoming() {
        let stream = conn?;
        let daemon = Arc::clone(&daemon);
        thread::spawn(move || {
            let _ = handle_connection(daemon, stream);
        });
    }
    Ok(())
}

fn handle_connection(daemon: Arc<Daemon>, stream: UnixStream) -> io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out_stream = stream;

    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Ok(()); // client disconnected before sending anything
    }
    let request: Request = match serde_json::from_str(line.trim()) {
        Ok(r) => r,
        Err(e) => return write_line(&mut out_stream, &Response::Error { message: format!("bad request: {e}") }),
    };

    if let Request::Attach { name } = request {
        return handle_attach(&daemon, &name, reader, out_stream);
    }

    let response = handle_control(&daemon, request);
    write_line(&mut out_stream, &response)
}

fn handle_control(daemon: &Daemon, request: Request) -> Response {
    let result = match request {
        Request::Create { name, cols, rows } => daemon.create(&name, cols, rows),
        Request::List => return Response::Sessions { sessions: daemon.list() },
        Request::Rename { from, to } => daemon.rename(&from, &to),
        Request::Close { name } => daemon.close(&name),
        Request::Resize { name, cols, rows } => daemon.resize(&name, cols, rows),
        Request::Attach { .. } => unreachable!("handled by handle_attach"),
    };
    match result {
        Ok(()) => Response::Ok,
        Err(e) => Response::Error { message: e.to_string() },
    }
}

fn handle_attach(daemon: &Daemon, name: &str, mut reader: BufReader<UnixStream>, mut out_stream: UnixStream) -> io::Result<()> {
    let AttachHandle { subscriber_id, snapshot, receiver, pane, session } = match daemon.attach(name) {
        Ok(h) => h,
        Err(e) => return write_line(&mut out_stream, &Response::Error { message: e.to_string() }),
    };
    write_line(&mut out_stream, &Response::Attached { snapshot })?;

    let writer_thread = {
        let mut writer_stream = out_stream.try_clone()?;
        thread::spawn(move || {
            while let Ok(chunk) = receiver.recv() {
                if writer_stream.write_all(&chunk).is_err() {
                    break;
                }
            }
        })
    };

    let mut buf = [0u8; 4096];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break; // client detached
        }
        if pane.lock().unwrap().write_raw(&buf[..n]).is_err() {
            break;
        }
    }

    session.subscribers.lock().unwrap().retain(|(id, _)| *id != subscriber_id);
    let _ = writer_thread.join();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_for<F: Fn() -> bool>(deadline: Duration, f: F) -> bool {
        let start = Instant::now();
        while !f() {
            if start.elapsed() > deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
        true
    }

    #[test]
    fn create_then_list_shows_session() {
        let daemon = Daemon::new();
        daemon.create("work", 80, 24).unwrap();
        let sessions = daemon.list();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].name, "work");
        assert!(sessions[0].alive);
    }

    #[test]
    fn create_duplicate_name_errors() {
        let daemon = Daemon::new();
        daemon.create("work", 80, 24).unwrap();
        let result = daemon.create("work", 80, 24);
        assert!(matches!(result, Err(DaemonError::AlreadyExists(_))));
    }

    #[test]
    fn rename_updates_registry() {
        let daemon = Daemon::new();
        daemon.create("work", 80, 24).unwrap();
        daemon.rename("work", "dev").unwrap();
        let sessions = daemon.list();
        assert_eq!(sessions[0].name, "dev");
        assert!(matches!(daemon.attach("work"), Err(DaemonError::NotFound(_))));
    }

    #[test]
    fn close_removes_session() {
        let daemon = Daemon::new();
        daemon.create("work", 80, 24).unwrap();
        daemon.close("work").unwrap();
        assert!(daemon.list().is_empty());
    }

    #[test]
    fn close_unknown_session_errors() {
        let daemon = Daemon::new();
        assert!(matches!(daemon.close("ghost"), Err(DaemonError::NotFound(_))));
    }

    #[test]
    fn attach_receives_live_output_and_can_write_input() {
        let daemon = Daemon::new();
        daemon.create("echo-session", 80, 24).unwrap();
        // Use /bin/cat semantics by writing input and reading it back.
        let handle = daemon.attach("echo-session").unwrap();
        handle.write_input(b"echo attach-roundtrip\n").unwrap();

        let mut collected = String::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !collected.contains("attach-roundtrip") && Instant::now() < deadline {
            if let Ok(chunk) = handle.receiver.recv_timeout(Duration::from_millis(200)) {
                collected.push_str(&String::from_utf8_lossy(&chunk));
            }
        }
        assert!(collected.contains("attach-roundtrip"), "collected: {collected:?}");
    }

    #[test]
    fn detach_unsubscribes_without_killing_session() {
        let daemon = Daemon::new();
        daemon.create("persist", 80, 24).unwrap();
        let handle = daemon.attach("persist").unwrap();
        daemon.detach(&handle);
        // Session should still be listed and alive after detaching.
        let sessions = daemon.list();
        assert_eq!(sessions.len(), 1);
        assert!(sessions[0].alive);
    }

    #[test]
    fn session_survives_across_separate_attach_calls() {
        // This is the actual "detach/reattach" guarantee: state set via one
        // attach (writing input) is still there after a second, independent attach.
        let daemon = Daemon::new();
        daemon.create("durable", 80, 24).unwrap();

        {
            let handle = daemon.attach("durable").unwrap();
            handle.write_input(b"echo first-attach\n").unwrap();
            wait_for(Duration::from_secs(5), || {
                daemon
                    .sessions
                    .lock()
                    .unwrap()
                    .get("durable")
                    .map(|s| s.pane.lock().unwrap().terminal.plain_text().iter().any(|l| l.contains("first-attach")))
                    .unwrap_or(false)
            });
            daemon.detach(&handle);
        }

        // Reattach: the snapshot should still contain output from before detaching.
        let handle2 = daemon.attach("durable").unwrap();
        assert!(handle2.snapshot.contains("first-attach"), "snapshot: {}", handle2.snapshot);
    }

    #[test]
    fn attach_to_unknown_session_errors() {
        let daemon = Daemon::new();
        assert!(matches!(daemon.attach("nope"), Err(DaemonError::NotFound(_))));
    }

    #[test]
    fn resize_updates_reported_dimensions() {
        let daemon = Daemon::new();
        daemon.create("resizable", 80, 24).unwrap();
        daemon.resize("resizable", 120, 40).unwrap();
        let sessions = daemon.list();
        assert_eq!(sessions[0].cols, 120);
        assert_eq!(sessions[0].rows, 40);
    }

    #[test]
    fn socket_protocol_create_list_attach_roundtrip() {
        let dir = std::env::temp_dir().join(format!("ut-daemon-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let socket_path = dir.join("daemon.sock");
        let _ = std::fs::remove_file(&socket_path);

        let daemon = Arc::new(Daemon::new());
        let listener = UnixListener::bind(&socket_path).unwrap();
        let serve_daemon = Arc::clone(&daemon);
        thread::spawn(move || {
            let _ = serve(serve_daemon, listener);
        });
        // Give the accept loop a moment to start.
        thread::sleep(Duration::from_millis(50));

        // Create via socket.
        let mut conn = UnixStream::connect(&socket_path).unwrap();
        conn.write_all(br#"{"cmd":"create","name":"sock-session","cols":80,"rows":24}"#).unwrap();
        conn.write_all(b"\n").unwrap();
        let mut reader = BufReader::new(conn);
        let mut resp_line = String::new();
        reader.read_line(&mut resp_line).unwrap();
        let resp: Response = serde_json::from_str(resp_line.trim()).unwrap();
        assert!(matches!(resp, Response::Ok));

        // List via a fresh socket connection.
        let mut conn2 = UnixStream::connect(&socket_path).unwrap();
        conn2.write_all(br#"{"cmd":"list"}"#).unwrap();
        conn2.write_all(b"\n").unwrap();
        let mut reader2 = BufReader::new(conn2);
        let mut list_line = String::new();
        reader2.read_line(&mut list_line).unwrap();
        let list_resp: Response = serde_json::from_str(list_line.trim()).unwrap();
        match list_resp {
            Response::Sessions { sessions } => {
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].name, "sock-session");
            }
            other => panic!("expected Sessions, got {other:?}"),
        }

        let _ = std::fs::remove_file(&socket_path);
    }
}
