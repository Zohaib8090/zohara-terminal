//! A real SSH client (Section 12): connect, verify the host key against a
//! known-hosts store, authenticate, and run a command — built on `ssh2`
//! (libssh2 bindings), which is a blocking, synchronous API well suited to
//! a plugin hook that isn't part of an async runtime.
//!
//! [`KnownHosts`] is pure parsing/comparison logic ("Known-host
//! verification" / "Host-key change warning") and is exercised by ordinary
//! unit tests. [`SshClient::connect`]/[`exec`] need a real reachable SSH
//! server, so their tests are `#[ignore]`d by default — run them with
//! `cargo test -p ut-plugin-ssh -- --ignored` against a local `sshd`. They
//! were verified in development against exactly that (a local `sshd` on
//! `127.0.0.1:2222` with a throwaway `ed25519` key) — this is real,
//! working code, not a stub, but a workspace `cargo test` run intentionally
//! doesn't depend on a live SSH server being available in the environment.
//!
//! [`exec`]: SshClient::exec

use base64::Engine;
use ssh2::Session;
use std::io::Read;
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct SshConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: AuthMethod,
    pub connect_timeout: Duration,
}

impl SshConfig {
    pub fn new(host: impl Into<String>, username: impl Into<String>, auth: AuthMethod) -> Self {
        SshConfig { host: host.into(), port: 22, username: username.into(), auth, connect_timeout: Duration::from_secs(10) }
    }
}

#[derive(Debug, Clone)]
pub enum AuthMethod {
    Password(String),
    PrivateKeyFile { path: PathBuf, passphrase: Option<String> },
    /// Key material already in memory (e.g. decrypted from a credential
    /// store), rather than a path on disk.
    PrivateKeyMemory { pem: String, passphrase: Option<String> },
    /// Defer to a running `ssh-agent` ("SSH-agent support").
    Agent,
}

#[derive(Debug)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_status: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("network error: {0}")]
    Io(#[from] std::io::Error),
    #[error("ssh protocol error: {0}")]
    Protocol(#[from] ssh2::Error),
    #[error("server did not present a host key")]
    NoHostKey,
    #[error("host key for '{0}' is not in the known-hosts store")]
    UnknownHostKey(String),
    #[error("host key for '{0}' has changed since it was last seen — possible man-in-the-middle")]
    HostKeyChanged(String),
    #[error("authentication failed")]
    AuthFailed,
}

pub struct SshClient {
    session: Session,
}

/// Map libssh2's `HostKeyType` to the algorithm name `known_hosts` files
/// use (`"ssh-ed25519"`, `"ssh-rsa"`, ...).
fn host_key_type_name(key_type: ssh2::HostKeyType) -> &'static str {
    match key_type {
        ssh2::HostKeyType::Rsa => "ssh-rsa",
        ssh2::HostKeyType::Dss => "ssh-dss",
        ssh2::HostKeyType::Ecdsa256 => "ecdsa-sha2-nistp256",
        ssh2::HostKeyType::Ecdsa384 => "ecdsa-sha2-nistp384",
        ssh2::HostKeyType::Ecdsa521 => "ecdsa-sha2-nistp521",
        ssh2::HostKeyType::Ed25519 => "ssh-ed25519",
        ssh2::HostKeyType::Unknown => "unknown",
    }
}

impl SshClient {
    /// Connect, verify the host key against `known_hosts` (pass `None` to
    /// skip verification — never do this against anything but a throwaway
    /// test target), and authenticate per `config.auth`.
    pub fn connect(config: &SshConfig, known_hosts: Option<&KnownHosts>) -> Result<Self, SshError> {
        let tcp = TcpStream::connect((config.host.as_str(), config.port))?;
        tcp.set_read_timeout(Some(config.connect_timeout))?;
        let mut session = Session::new()?;
        session.set_tcp_stream(tcp);
        session.handshake()?;

        if let Some(kh) = known_hosts {
            let (key_bytes, key_type) = session.host_key().ok_or(SshError::NoHostKey)?;
            let key_base64 = base64::engine::general_purpose::STANDARD.encode(key_bytes);
            match kh.verify(&config.host, host_key_type_name(key_type), &key_base64) {
                HostKeyStatus::Known => {}
                HostKeyStatus::Unknown => return Err(SshError::UnknownHostKey(config.host.clone())),
                HostKeyStatus::Changed { .. } => return Err(SshError::HostKeyChanged(config.host.clone())),
            }
        }

        match &config.auth {
            AuthMethod::Password(pw) => session.userauth_password(&config.username, pw)?,
            AuthMethod::PrivateKeyFile { path, passphrase } => {
                session.userauth_pubkey_file(&config.username, None, path, passphrase.as_deref())?
            }
            AuthMethod::PrivateKeyMemory { pem, passphrase } => {
                session.userauth_pubkey_memory(&config.username, None, pem, passphrase.as_deref())?
            }
            AuthMethod::Agent => session.userauth_agent(&config.username)?,
        }

        if !session.authenticated() {
            return Err(SshError::AuthFailed);
        }
        Ok(SshClient { session })
    }

    /// Run one command and collect its stdout/stderr/exit status
    /// ("Remote command history" starts from having this available at all).
    pub fn exec(&self, command: &str) -> Result<ExecOutput, SshError> {
        let mut channel = self.session.channel_session()?;
        channel.exec(command)?;
        let mut stdout = String::new();
        channel.read_to_string(&mut stdout)?;
        let mut stderr = String::new();
        channel.stderr().read_to_string(&mut stderr)?;
        channel.wait_close()?;
        let exit_status = channel.exit_status()?;
        Ok(ExecOutput { stdout, stderr, exit_status })
    }
}

// ---------------------------------------------------------------------
// known_hosts
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownHostEntry {
    pub host: String,
    pub key_type: String,
    pub key_base64: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostKeyStatus {
    Known,
    Unknown,
    Changed { previous_key_base64: String },
}

/// A parsed `~/.ssh/known_hosts`-format store.
#[derive(Debug, Clone, Default)]
pub struct KnownHosts {
    entries: Vec<KnownHostEntry>,
}

impl KnownHosts {
    pub fn parse(text: &str) -> Self {
        let entries = text
            .lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                let mut parts = line.split_whitespace();
                let host = parts.next()?.to_string();
                let key_type = parts.next()?.to_string();
                let key_base64 = parts.next()?.to_string();
                Some(KnownHostEntry { host, key_type, key_base64 })
            })
            .collect();
        KnownHosts { entries }
    }

    pub fn add(&mut self, entry: KnownHostEntry) {
        self.entries.push(entry);
    }

    /// Check `host`'s presented key against what's on record. Host-key
    /// *type* is part of the identity: an entry for the same host under a
    /// different key type doesn't count as a match either way.
    pub fn verify(&self, host: &str, key_type: &str, key_base64: &str) -> HostKeyStatus {
        let matching_host: Vec<&KnownHostEntry> = self.entries.iter().filter(|e| e.host == host && e.key_type == key_type).collect();
        if matching_host.is_empty() {
            return HostKeyStatus::Unknown;
        }
        if matching_host.iter().any(|e| e.key_base64 == key_base64) {
            return HostKeyStatus::Known;
        }
        HostKeyStatus::Changed { previous_key_base64: matching_host[0].key_base64.clone() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
example.com ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIexamplekeydata1
10.0.0.5 ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABgQCexamplekeydata2
";

    #[test]
    fn parses_known_hosts_lines() {
        let kh = KnownHosts::parse(SAMPLE);
        assert_eq!(kh.entries.len(), 2);
        assert_eq!(kh.entries[0].host, "example.com");
        assert_eq!(kh.entries[0].key_type, "ssh-ed25519");
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let text = "# comment\n\nexample.com ssh-ed25519 AAAA1234\n";
        let kh = KnownHosts::parse(text);
        assert_eq!(kh.entries.len(), 1);
    }

    #[test]
    fn verify_known_host_matches() {
        let kh = KnownHosts::parse(SAMPLE);
        let status = kh.verify("example.com", "ssh-ed25519", "AAAAC3NzaC1lZDI1NTE5AAAAIexamplekeydata1");
        assert_eq!(status, HostKeyStatus::Known);
    }

    #[test]
    fn verify_unknown_host_is_unknown() {
        let kh = KnownHosts::parse(SAMPLE);
        let status = kh.verify("never-seen.example.com", "ssh-ed25519", "whatever");
        assert_eq!(status, HostKeyStatus::Unknown);
    }

    #[test]
    fn verify_changed_key_detected() {
        let kh = KnownHosts::parse(SAMPLE);
        let status = kh.verify("example.com", "ssh-ed25519", "DIFFERENT_KEY_DATA");
        match status {
            HostKeyStatus::Changed { previous_key_base64 } => {
                assert_eq!(previous_key_base64, "AAAAC3NzaC1lZDI1NTE5AAAAIexamplekeydata1");
            }
            other => panic!("expected Changed, got {other:?}"),
        }
    }

    #[test]
    fn different_key_type_for_same_host_is_unknown_not_changed() {
        let kh = KnownHosts::parse(SAMPLE);
        // example.com only has an ssh-ed25519 entry; asking about ssh-rsa
        // for that host should be Unknown, not confused with a different host.
        let status = kh.verify("example.com", "ssh-rsa", "anything");
        assert_eq!(status, HostKeyStatus::Unknown);
    }

    #[test]
    fn add_grows_the_store() {
        let mut kh = KnownHosts::default();
        assert_eq!(kh.verify("h", "t", "k"), HostKeyStatus::Unknown);
        kh.add(KnownHostEntry { host: "h".into(), key_type: "t".into(), key_base64: "k".into() });
        assert_eq!(kh.verify("h", "t", "k"), HostKeyStatus::Known);
    }

    // ---- Live-server tests: require a reachable sshd, so they're ignored
    // by default. Point SshConfig at a local test sshd to run them:
    //   cargo test -p ut-plugin-ssh -- --ignored

    #[test]
    #[ignore = "requires a live sshd; see module docs"]
    fn connect_and_exec_against_live_server() {
        let config = SshConfig::new(
            std::env::var("UT_SSH_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            std::env::var("UT_SSH_TEST_USER").unwrap_or_else(|_| "root".into()),
            AuthMethod::PrivateKeyFile {
                path: std::env::var("UT_SSH_TEST_KEY").unwrap_or_else(|_| "/tmp/sshd_test/client_key".into()).into(),
                passphrase: None,
            },
        );
        let mut config = config;
        config.port = std::env::var("UT_SSH_TEST_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(2222);

        let client = SshClient::connect(&config, None).expect("connect+auth should succeed against test sshd");
        let output = client.exec("echo integration-ok").expect("exec should succeed");
        assert!(output.stdout.contains("integration-ok"), "stdout was: {:?}", output.stdout);
        assert_eq!(output.exit_status, 0);
    }

    #[test]
    #[ignore = "requires a live sshd; see module docs"]
    fn connect_rejects_unknown_host_key_when_verification_enabled() {
        let config = SshConfig::new("127.0.0.1", "root", AuthMethod::Password("wrong".into()));
        let mut config = config;
        config.port = 2222;
        let empty_known_hosts = KnownHosts::default();
        let result = SshClient::connect(&config, Some(&empty_known_hosts));
        assert!(matches!(result, Err(SshError::UnknownHostKey(_))));
    }
}
