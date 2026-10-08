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
//! [`dynamic_forward`] (`ssh -D`) is the same per-connection
//! `channel_direct_tcpip` logic as [`local_forward`], except the
//! destination host/port isn't fixed at forward-creation time — each
//! accepted connection speaks a minimal SOCKS5 handshake first (CONNECT
//! only, no-auth only; good enough for "point a browser's SOCKS proxy
//! setting at this port", not a general SOCKS server) to say where *it*
//! wants to go. That handshake is done with the new connection briefly
//! back in blocking mode (bounded by a short read timeout) rather than
//! folded into the non-blocking poll loop like everything else here — a
//! deliberate simplification: it's a handful of small reads/writes a real
//! SOCKS client sends immediately after connecting, and doing it inline
//! means the event loop doesn't need a half-open-handshake state machine.
//! The cost is real: a slow or malicious client mid-handshake stalls the
//! *whole* loop (including already-active tunnels) for up to that
//! timeout. Acceptable for a local dev-tool SOCKS proxy; revisit with a
//! proper non-blocking handshake state machine if this is ever exposed to
//! untrusted connectors.
//!
//! Like [`crate::client`]'s tests, the forwarding tests that need a real
//! SSH connection are `#[ignore]`d by default; see that module's docs for
//! how to run them. The SOCKS5 handshake parser itself has no such
//! dependency and is tested directly against a real local `TcpStream`
//! pair in the default test run.

use crate::client::{KnownHosts, SshClient, SshConfig, SshError};
use ssh2::{Channel, Session};
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
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

#[derive(Debug, PartialEq, Eq)]
enum Socks5Error {
    Io,
    UnsupportedVersion,
    NoAcceptableAuthMethod,
    UnsupportedCommand,
    UnsupportedAddressType,
    InvalidDomainName,
}

impl From<std::io::Error> for Socks5Error {
    fn from(_: std::io::Error) -> Self {
        Socks5Error::Io
    }
}

/// Perform a minimal SOCKS5 handshake (RFC 1928) on `stream`, which must
/// already be in blocking mode with a reasonable read timeout set. Only
/// the no-authentication method and the CONNECT command are accepted —
/// everything [`dynamic_forward`] needs and nothing more. On success,
/// writes the "succeeded" reply and returns the requested destination;
/// on failure, best-effort writes a SOCKS5 error reply (ignoring any
/// write error, since the connection is being dropped either way) before
/// returning `Err`.
fn do_socks5_handshake(stream: &mut TcpStream) -> Result<(String, u16), Socks5Error> {
    let mut greeting_header = [0u8; 2];
    stream.read_exact(&mut greeting_header)?;
    if greeting_header[0] != 0x05 {
        return Err(Socks5Error::UnsupportedVersion);
    }
    let mut methods = vec![0u8; greeting_header[1] as usize];
    stream.read_exact(&mut methods)?;
    if !methods.contains(&0x00) {
        let _ = stream.write_all(&[0x05, 0xFF]); // no acceptable methods
        return Err(Socks5Error::NoAcceptableAuthMethod);
    }
    stream.write_all(&[0x05, 0x00])?; // chosen method: no auth

    let mut request_header = [0u8; 4];
    stream.read_exact(&mut request_header)?;
    if request_header[0] != 0x05 {
        return Err(Socks5Error::UnsupportedVersion);
    }
    if request_header[1] != 0x01 {
        let _ = stream.write_all(&socks5_reply(0x07)); // command not supported
        return Err(Socks5Error::UnsupportedCommand);
    }

    let host = match request_header[3] {
        0x01 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr)?;
            Ipv4Addr::from(addr).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len)?;
            let mut name = vec![0u8; len[0] as usize];
            stream.read_exact(&mut name)?;
            String::from_utf8(name).map_err(|_| Socks5Error::InvalidDomainName)?
        }
        0x04 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr)?;
            Ipv6Addr::from(addr).to_string()
        }
        _ => {
            let _ = stream.write_all(&socks5_reply(0x08)); // address type not supported
            return Err(Socks5Error::UnsupportedAddressType);
        }
    };
    let mut port_bytes = [0u8; 2];
    stream.read_exact(&mut port_bytes)?;
    let port = u16::from_be_bytes(port_bytes);

    stream.write_all(&socks5_reply(0x00))?; // succeeded
    Ok((host, port))
}

/// Build a SOCKS5 reply with the given status byte and a dummy
/// `0.0.0.0:0` bound address — we don't track a real local bind address
/// per tunnelled connection (there isn't one; it's relayed over SSH), and
/// real SOCKS clients don't rely on this field being meaningful.
fn socks5_reply(status: u8) -> [u8; 10] {
    [0x05, status, 0x00, 0x01, 0, 0, 0, 0, 0, 0]
}

/// `ssh -D local_port` — a local SOCKS5 proxy: each connection says where
/// *it* wants to go (via the SOCKS handshake), tunnelled there through
/// the SSH server with `channel_direct_tcpip`, same as [`local_forward`]
/// but with a per-connection destination instead of one fixed at
/// creation. See the module docs for why the handshake itself runs in
/// blocking mode rather than through the non-blocking poll loop below.
pub fn dynamic_forward(config: SshConfig, known_hosts: Option<KnownHosts>, local_port: u16) -> Result<PortForward, SshError> {
    let listener = TcpListener::bind(("127.0.0.1", local_port))?;
    listener.set_nonblocking(true)?;
    let bound_port = listener.local_addr()?.port();

    let client = SshClient::connect(&config, known_hosts.as_ref())?;
    let session: Session = client.session;
    session.set_blocking(false);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        let mut pending: Vec<(TcpStream, String, u16)> = Vec::new();
        let mut active: Vec<(TcpStream, Channel)> = Vec::new();
        let mut buf = [0u8; 8192];

        while !stop_clone.load(Ordering::SeqCst) {
            let mut activity = false;

            match listener.accept() {
                Ok((mut stream, _)) => {
                    activity = true;
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                    match do_socks5_handshake(&mut stream) {
                        Ok((host, port)) => {
                            let _ = stream.set_nonblocking(true);
                            pending.push((stream, host, port));
                        }
                        Err(_) => { /* malformed/unsupported handshake: drop the connection */ }
                    }
                }
                Err(e) if is_would_block_io(&e) => {}
                Err(_) => {}
            }

            let mut i = 0;
            while i < pending.len() {
                let (_, host, port) = &pending[i];
                match session.channel_direct_tcpip(host, *port, None) {
                    Ok(channel) => {
                        let (stream, _, _) = pending.swap_remove(i);
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

    #[test]
    #[ignore = "requires a live sshd; see client module docs"]
    fn dynamic_forward_tunnels_a_socks5_connect_to_a_real_service() {
        let echo_port = spawn_echo_server();
        let forward = dynamic_forward(test_config(), None, 0).expect("start dynamic (SOCKS5) forward");

        let mut conn = TcpStream::connect(("127.0.0.1", forward.bound_port)).expect("connect to SOCKS5 proxy port");
        conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        socks5_client_connect(&mut conn, "127.0.0.1", echo_port).expect("SOCKS5 handshake as a client");

        conn.write_all(b"hello through the SOCKS tunnel").unwrap();
        let mut buf = [0u8; 64];
        let n = conn.read(&mut buf).expect("read echoed bytes");
        assert_eq!(&buf[..n], b"hello through the SOCKS tunnel");

        forward.stop();
    }

    /// The client side of a minimal SOCKS5 CONNECT handshake, standing in
    /// for a real SOCKS5 client library in the one test above that needs
    /// to drive [`dynamic_forward`] end-to-end. Panics/returns `Err` on
    /// any unexpected reply byte rather than silently proceeding.
    fn socks5_client_connect(conn: &mut TcpStream, host: &str, port: u16) -> std::io::Result<()> {
        conn.write_all(&[0x05, 0x01, 0x00])?; // version 5, 1 method, no-auth
        let mut method_reply = [0u8; 2];
        conn.read_exact(&mut method_reply)?;
        assert_eq!(method_reply, [0x05, 0x00], "server should accept no-auth");

        let octets: Vec<u8> = host.split('.').map(|p| p.parse().unwrap()).collect();
        let mut request = vec![0x05, 0x01, 0x00, 0x01];
        request.extend_from_slice(&octets);
        request.extend_from_slice(&port.to_be_bytes());
        conn.write_all(&request)?;

        let mut reply = [0u8; 10];
        conn.read_exact(&mut reply)?;
        assert_eq!(reply[1], 0x00, "SOCKS5 CONNECT should succeed");
        Ok(())
    }

    #[test]
    fn socks5_handshake_parses_an_ipv4_connect_request() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            do_socks5_handshake(&mut stream)
        });

        let mut client = TcpStream::connect(addr).unwrap();
        socks5_client_connect(&mut client, "93.184.216.34", 443).unwrap();

        let (host, port) = server.join().unwrap().expect("handshake should parse cleanly");
        assert_eq!(host, "93.184.216.34");
        assert_eq!(port, 443);
    }

    #[test]
    fn socks5_handshake_parses_a_domain_name_connect_request() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            do_socks5_handshake(&mut stream)
        });

        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).unwrap();

        let domain = b"example.com";
        let mut request = vec![0x05, 0x01, 0x00, 0x03, domain.len() as u8];
        request.extend_from_slice(domain);
        request.extend_from_slice(&80u16.to_be_bytes());
        client.write_all(&request).unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], 0x00);

        let (host, port) = server.join().unwrap().expect("handshake should parse cleanly");
        assert_eq!(host, "example.com");
        assert_eq!(port, 80);
    }

    #[test]
    fn socks5_handshake_rejects_a_non_connect_command() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            do_socks5_handshake(&mut stream)
        });

        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(&[0x05, 0x01, 0x00]).unwrap();
        let mut method_reply = [0u8; 2];
        client.read_exact(&mut method_reply).unwrap();

        // CMD = 0x02 (BIND), which this handshake doesn't support.
        client.write_all(&[0x05, 0x02, 0x00, 0x01, 127, 0, 0, 1, 0, 80]).unwrap();
        let mut reply = [0u8; 10];
        client.read_exact(&mut reply).unwrap();
        assert_eq!(reply[1], 0x07, "should reply 'command not supported'");

        assert_eq!(server.join().unwrap(), Err(Socks5Error::UnsupportedCommand));
    }

    #[test]
    fn socks5_handshake_rejects_wrong_protocol_version() {
        let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            do_socks5_handshake(&mut stream)
        });

        let mut client = TcpStream::connect(addr).unwrap();
        client.write_all(&[0x04, 0x01, 0x00]).unwrap(); // SOCKS4, not SOCKS5

        assert_eq!(server.join().unwrap(), Err(Socks5Error::UnsupportedVersion));
    }
}
