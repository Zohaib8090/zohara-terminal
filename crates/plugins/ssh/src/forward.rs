//! Local and remote TCP port forwarding (Section 12 "Port forwarding" /
//! "Local forwarding" / "Remote forwarding").
//!
//! Each active forward gets its own dedicated, non-blocking SSH session
//! (via [`SshClient::connect`]) and one background thread that
//! cooperatively multiplexes every tunnelled connection on that session —
//! a single-threaded event loop, not one OS thread per tunnelled
//! connection. That's a deliberate choice, not just an optimization:
//! libssh2 requires all I/O on channels belonging to the same `Session` to
//! be serialized (concurrent reads/writes from different threads on one
//! session's socket are undefined behavior), so one thread owning the
//! session outright sidesteps that hazard entirely rather than working
//! around it with locks. [`local_forward`]/[`remote_forward`] each keep
//! *sole* ownership of their session; nothing else ever touches it.
//!
//! Dynamic (SOCKS) forwarding is not implemented — it needs a SOCKS4/5
//! protocol implementation in front of the same per-connection channel
//! logic used here, which is future work layered on top of this module,
//! not a different architecture.
//!
//! Like [`crate::client`]'s tests, these need a reachable `sshd` with
//! `AllowTcpForwarding yes`, so their tests are `#[ignore]`d by default;
//! see that module's docs for how to run them.

use crate::client::{KnownHosts, SshClient, SshConfig, SshError};
use ssh2::{Channel, Session};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// A running forward. Dropping it (or calling [`PortForward::stop`])
/// signals the background thread to exit and joins it, closing every
/// tunnelled connection along with it.
pub struct PortForward {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// The actual bound port — meaningful mainly for local forwarding when
    /// `local_port` was `0` (pick any free port) and for the OS-chosen
    /// remote port in `remote_forward` when `remote_port` was `0`.
    pub bound_port: u16,
}

impl PortForward {
    pub fn stop(mut self) {
        self.stop_and_join();
    }

    fn stop_and_join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

fn is_would_block_ssh(e: &ssh2::Error) -> bool {
    matches!(e.code(), ssh2::ErrorCode::Session(-37))
}

fn is_would_block_io(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::WouldBlock
}

/// Read from `a`, write to `b` (and vice versa) without blocking either
/// direction on the other. Returns `false` once either side has closed or
/// errored, at which point the caller should drop the pair.
fn pump_once(tcp: &mut TcpStream, channel: &mut Channel, buf: &mut [u8]) -> (bool, bool) {
    let mut activity = false;

    match tcp.read(buf) {
        Ok(0) => return (false, activity),
        Ok(n) => {
            if channel.write_all(&buf[..n]).is_err() {
                return (false, activity);
            }
            activity = true;
        }
        Err(e) if is_would_block_io(&e) => {}
        Err(_) => return (false, activity),
    }

    match channel.read(buf) {
        Ok(0) => return (false, activity),
        Ok(n) => {
            if tcp.write_all(&buf[..n]).is_err() {
                return (false, activity);
            }
            activity = true;
        }
        Err(e) if is_would_block_io(&e) => {}
        Err(_) => return (false, activity),
    }

    (true, activity)
}

/// Pump every active pair once; drops (in place, via swap-remove) any pair
/// whose connection just closed. Returns whether anything happened this pass.
fn pump_active(active: &mut Vec<(TcpStream, Channel)>, buf: &mut [u8]) -> bool {
    let mut any_activity = false;
    let mut i = 0;
    while i < active.len() {
        let (tcp, channel) = &mut active[i];
        let (keep, activity) = pump_once(tcp, channel, buf);
        any_activity |= activity;
        if keep {
            i += 1;
        } else {
            active.swap_remove(i);
        }
    }
    any_activity
}

/// `ssh -L local_port:remote_host:remote_port` — accept local TCP
/// connections and tunnel each one to `remote_host:remote_port` as seen
/// from the SSH server.
pub fn local_forward(config: SshConfig, known_hosts: Option<KnownHosts>, local_port: u16, remote_host: String, remote_port: u16) -> Result<PortForward, SshError> {
    let listener = TcpListener::bind(("127.0.0.1", local_port))?;
    listener.set_nonblocking(true)?;
    let bound_port = listener.local_addr()?.port();

    let client = SshClient::connect(&config, known_hosts.as_ref())?;
    let session: Session = client.session;
    session.set_blocking(false);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        let mut pending: Vec<TcpStream> = Vec::new();
        let mut active: Vec<(TcpStream, Channel)> = Vec::new();
        let mut buf = [0u8; 8192];

        while !stop_clone.load(Ordering::SeqCst) {
            let mut activity = false;

            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = stream.set_nonblocking(true);
                    pending.push(stream);
                    activity = true;
                }
                Err(e) if is_would_block_io(&e) => {}
                Err(_) => {}
            }

            let mut i = 0;
            while i < pending.len() {
                match session.channel_direct_tcpip(&remote_host, remote_port, None) {
                    Ok(channel) => {
                        let stream = pending.swap_remove(i);
                        active.push((stream, channel));
                        activity = true;
                    }
                    Err(e) if is_would_block_ssh(&e) => i += 1,
                    Err(_) => {
                        pending.swap_remove(i);
                    }
                }
            }

            activity |= pump_active(&mut active, &mut buf);

            if !activity {
                thread::sleep(Duration::from_millis(5));
            }
        }
    });

    Ok(PortForward { stop, handle: Some(handle), bound_port })
}

/// `ssh -R remote_port:local_host:local_port` — ask the SSH server to
/// listen on `remote_port` and tunnel each connection it accepts there
/// back to `local_host:local_port` on this machine.
pub fn remote_forward(config: SshConfig, known_hosts: Option<KnownHosts>, remote_port: u16, local_host: String, local_port: u16) -> Result<PortForward, SshError> {
    let client = SshClient::connect(&config, known_hosts.as_ref())?;
    let session: Session = client.session;

    // channel_forward_listen itself needs the session in blocking mode to
    // complete the request/reply with the server; only the accept loop runs non-blocking.
    let (mut listener, bound_port) = session.channel_forward_listen(remote_port, None, None)?;
    session.set_blocking(false);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        let mut active: Vec<(TcpStream, Channel)> = Vec::new();
        let mut buf = [0u8; 8192];

        while !stop_clone.load(Ordering::SeqCst) {
            let mut activity = false;

            match listener.accept() {
                Ok(channel) => {
                    if let Ok(tcp) = TcpStream::connect((local_host.as_str(), local_port)) {
                        let _ = tcp.set_nonblocking(true);
                        active.push((tcp, channel));
                        activity = true;
                    }
                    // If the local target refuses the connection, the
                    // channel is simply dropped (closing it on the server side).
                }
                Err(e) if is_would_block_ssh(&e) => {}
                Err(_) => {}
            }

            activity |= pump_active(&mut active, &mut buf);

            if !activity {
                thread::sleep(Duration::from_millis(5));
            }
        }
    });

    Ok(PortForward { stop, handle: Some(handle), bound_port })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::AuthMethod;
    use std::net::TcpListener as StdTcpListener;
    use std::time::Instant;

    fn test_config() -> SshConfig {
        let mut config = SshConfig::new(
            std::env::var("UT_SSH_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            std::env::var("UT_SSH_TEST_USER").unwrap_or_else(|_| "root".into()),
            AuthMethod::PrivateKeyFile { path: std::env::var("UT_SSH_TEST_KEY").unwrap_or_else(|_| "/tmp/sshd_test/client_key".into()).into(), passphrase: None },
        );
        config.port = std::env::var("UT_SSH_TEST_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(2222);
        config
    }

    /// A trivial local TCP echo server, standing in for "some service on
    /// the far side of the tunnel" in both forwarding directions.
    fn spawn_echo_server() -> u16 {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                thread::spawn(move || {
                    let mut stream = stream;
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = stream.read(&mut buf) {
                        if n == 0 || stream.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                });
            }
        });
        port
    }

    #[test]
    #[ignore = "requires a live sshd; see client module docs"]
    fn local_forward_tunnels_to_a_real_service() {
        let echo_port = spawn_echo_server();
        let forward = local_forward(test_config(), None, 0, "127.0.0.1".into(), echo_port).expect("start local forward");

        let mut conn = TcpStream::connect(("127.0.0.1", forward.bound_port)).expect("connect to forwarded port");
        conn.write_all(b"hello through the tunnel").unwrap();

        let mut buf = [0u8; 64];
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let n = conn.read(&mut buf).expect("read echoed bytes");
        assert_eq!(&buf[..n], b"hello through the tunnel");

        forward.stop();
    }

    #[test]
    #[ignore = "requires a live sshd; see client module docs"]
    fn remote_forward_tunnels_back_to_local_service() {
        let echo_port = spawn_echo_server();
        let forward = remote_forward(test_config(), None, 0, "127.0.0.1".into(), echo_port).expect("start remote forward");

        // The remote forward listens on the *server's* side; since our test
        // server is 127.0.0.1 too, we can dial it directly here.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut conn = loop {
            match TcpStream::connect(("127.0.0.1", forward.bound_port)) {
                Ok(c) => break c,
                Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
                Err(e) => panic!("could not connect to remote-forwarded port: {e}"),
            }
        };
        conn.write_all(b"hello via remote forward").unwrap();
        let mut buf = [0u8; 64];
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let n = conn.read(&mut buf).expect("read echoed bytes");
        assert_eq!(&buf[..n], b"hello via remote forward");

        forward.stop();
    }
}
