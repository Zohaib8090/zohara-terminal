//! Privilege-awareness types (Section 10 "Privilege awareness").
//!
//! These are deliberately simple, serializable value types: the terminal
//! front end is responsible for detecting the actual state (parsing prompt
//! sequences, OSC, `whoami`, SSH connection info, container cgroup checks,
//! etc.) and constructing a [`PrivilegeContext`] to pass into [`crate::analyze`],
//! which uses it to adjust wording and severity (e.g. the same `rm -rf`
//! is more severe on a remote production host than on a local sandbox).

/// The user's current privilege level in the active shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Privilege {
    #[default]
    Normal,
    Root,
    Administrator,
}

/// Everything about the current session relevant to risk assessment.
#[derive(Debug, Clone, Default)]
pub struct PrivilegeContext {
    pub privilege: Privilege,
    pub is_remote: bool,
    /// Hostname or label shown to the user, e.g. `prod-db-01`.
    pub host_label: Option<String>,
    /// Heuristic: hostname/tags suggest a production system.
    pub looks_like_production: bool,
    pub is_container: bool,
    pub is_chroot: bool,
    pub is_recovery_environment: bool,
    pub is_read_only_environment: bool,
    /// Seconds remaining on an active `sudo` timestamp, if known.
    pub sudo_timer_seconds: Option<u32>,
}

impl PrivilegeContext {
    pub fn local_normal_user() -> Self {
        Self::default()
    }

    /// A short label combining privilege + location, e.g. "root@prod-db-01 (remote)".
    pub fn label(&self) -> String {
        let who = match self.privilege {
            Privilege::Normal => "user",
            Privilege::Root => "root",
            Privilege::Administrator => "admin",
        };
        let mut s = match &self.host_label {
            Some(h) => format!("{who}@{h}"),
            None => who.to_string(),
        };
        if self.is_remote {
            s.push_str(" (remote)");
        }
        if self.is_container {
            s.push_str(" (container)");
        }
        if self.is_chroot {
            s.push_str(" (chroot)");
        }
        s
    }
}
