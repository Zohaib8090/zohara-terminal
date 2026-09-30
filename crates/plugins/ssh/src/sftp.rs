//! SFTP file browsing (Section 12 "SFTP browser" / "Upload files" /
//! "Download files"), built on the same authenticated `ssh2::Session` an
//! [`SshClient`] already holds — no second connection needed, since SFTP
//! is just another channel type over one SSH connection.
//!
//! Like [`crate::client`]'s live-connection tests, [`SftpBrowser`]'s tests
//! need a reachable `sshd` and are `#[ignore]`d by default; see that
//! module's docs for how to run them.

use crate::client::{SshClient, SshError};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SftpEntry {
    pub name: String,
    pub is_dir: bool,
    pub is_symlink: bool,
    pub size: u64,
    /// Unix permission bits (e.g. `0o755`), when the server reported them.
    pub permissions: Option<u32>,
}

pub struct SftpBrowser {
    sftp: ssh2::Sftp,
}

impl SshClient {
    /// Open an SFTP browser over this client's existing connection.
    pub fn sftp(&self) -> Result<SftpBrowser, SshError> {
        Ok(SftpBrowser { sftp: self.session.sftp()? })
    }
}

impl SftpBrowser {
    /// List one directory's entries (not recursive).
    pub fn list_dir(&self, path: &Path) -> Result<Vec<SftpEntry>, SshError> {
        let entries = self.sftp.readdir(path)?;
        Ok(entries
            .into_iter()
            .map(|(entry_path, stat)| SftpEntry {
                name: entry_path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| entry_path.display().to_string()),
                is_dir: stat.is_dir(),
                is_symlink: stat.file_type().is_symlink(),
                size: stat.size.unwrap_or(0),
                permissions: stat.perm,
            })
            .collect())
    }

    /// Upload a local file to `remote`, overwriting it if it exists.
    pub fn upload(&self, local: &Path, remote: &Path) -> Result<u64, SshError> {
        let mut local_file = std::fs::File::open(local)?;
        let mut remote_file = self.sftp.create(remote)?;
        Ok(std::io::copy(&mut local_file, &mut remote_file)?)
    }

    /// Download a remote file to `local`, overwriting it if it exists.
    pub fn download(&self, remote: &Path, local: &Path) -> Result<u64, SshError> {
        let mut remote_file = self.sftp.open(remote)?;
        let mut local_file = std::fs::File::create(local)?;
        Ok(std::io::copy(&mut remote_file, &mut local_file)?)
    }

    pub fn mkdir(&self, path: &Path) -> Result<(), SshError> {
        self.sftp.mkdir(path, 0o755)?;
        Ok(())
    }

    pub fn rmdir(&self, path: &Path) -> Result<(), SshError> {
        self.sftp.rmdir(path)?;
        Ok(())
    }

    pub fn remove_file(&self, path: &Path) -> Result<(), SshError> {
        self.sftp.unlink(path)?;
        Ok(())
    }

    pub fn rename(&self, from: &Path, to: &Path) -> Result<(), SshError> {
        self.sftp.rename(from, to, None)?;
        Ok(())
    }

    pub fn exists(&self, path: &Path) -> bool {
        self.sftp.stat(path).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{AuthMethod, SshConfig};
    use std::path::PathBuf;

    fn test_config() -> SshConfig {
        let mut config = SshConfig::new(
            std::env::var("UT_SSH_TEST_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            std::env::var("UT_SSH_TEST_USER").unwrap_or_else(|_| "root".into()),
            AuthMethod::PrivateKeyFile { path: std::env::var("UT_SSH_TEST_KEY").unwrap_or_else(|_| "/tmp/sshd_test/client_key".into()).into(), passphrase: None },
        );
        config.port = std::env::var("UT_SSH_TEST_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(2222);
        config
    }

    fn local_scratch_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ut-sftp-test-{}-{name}", std::process::id()));
        p
    }

    #[test]
    #[ignore = "requires a live sshd; see client module docs"]
    fn upload_download_and_list_round_trip() {
        let client = SshClient::connect(&test_config(), None).expect("connect");
        let sftp = client.sftp().expect("open sftp");

        let local_src = local_scratch_path("upload-src.txt");
        std::fs::write(&local_src, b"sftp round trip contents").unwrap();
        let remote_path = std::path::Path::new("/tmp/ut-sftp-test-upload.txt");

        let uploaded = sftp.upload(&local_src, remote_path).expect("upload");
        assert_eq!(uploaded, 24);

        let entries = sftp.list_dir(std::path::Path::new("/tmp")).expect("list");
        assert!(entries.iter().any(|e| e.name == "ut-sftp-test-upload.txt"));

        let local_dst = local_scratch_path("download-dst.txt");
        let downloaded = sftp.download(remote_path, &local_dst).expect("download");
        assert_eq!(downloaded, 24);
        let contents = std::fs::read_to_string(&local_dst).unwrap();
        assert_eq!(contents, "sftp round trip contents");

        sftp.remove_file(remote_path).expect("cleanup remote");
        assert!(!sftp.exists(remote_path));
        let _ = std::fs::remove_file(&local_src);
        let _ = std::fs::remove_file(&local_dst);
    }

    #[test]
    #[ignore = "requires a live sshd; see client module docs"]
    fn mkdir_rename_and_rmdir() {
        let client = SshClient::connect(&test_config(), None).expect("connect");
        let sftp = client.sftp().expect("open sftp");

        let dir = std::path::Path::new("/tmp/ut-sftp-test-dir");
        let renamed = std::path::Path::new("/tmp/ut-sftp-test-dir-renamed");
        let _ = sftp.rmdir(dir);
        let _ = sftp.rmdir(renamed);

        sftp.mkdir(dir).expect("mkdir");
        assert!(sftp.exists(dir));
        sftp.rename(dir, renamed).expect("rename");
        assert!(sftp.exists(renamed));
        assert!(!sftp.exists(dir));
        sftp.rmdir(renamed).expect("rmdir");
        assert!(!sftp.exists(renamed));
    }
}
