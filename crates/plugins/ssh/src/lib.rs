//! `ut-plugin-ssh`: SSH/SFTP plugin (Section 12 "SSH and remote work").
//!
//! What's implemented now: the [`HostProfile`] data model (saved hosts,
//! groups, tags, jump hosts, port/identity fields) and a parser for
//! OpenSSH's `~/.ssh/config` format ("SSH config import") — both are pure,
//! offline, and don't need a network connection to be useful or testable.
//!
//! Not yet implemented: actually opening connections, port forwarding,
//! the SFTP browser, and known-host verification — those need a real SSH
//! client library and a live network, which is out of scope for this pass.
//! The [`Plugin`] impl below is deliberately minimal (it only recognizes
//! `ssh <host>` commands against imported profiles) so the crate compiles
//! and is wired into the plugin system now, ready to grow into the rest.

use std::collections::HashMap;
use ut_plugin_api::{Permissions, Plugin, PluginAction, PluginContext};

#[cfg(test)]
use ut_safety::PrivilegeContext;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostProfile {
    pub alias: String,
    pub hostname: Option<String>,
    pub user: Option<String>,
    pub port: Option<u16>,
    pub identity_file: Option<String>,
    pub proxy_jump: Option<String>,
    pub tags: Vec<String>,
    pub group: Option<String>,
}

/// Parse an OpenSSH-style `~/.ssh/config` file into host profiles. Handles
/// `Host`, `HostName`, `User`, `Port`, `IdentityFile`, and `ProxyJump`;
/// unrecognized directives are ignored. A `Host *` block or a host alias
/// containing `*`/`?` (a wildcard pattern, not a real saved host) is skipped.
pub fn parse_ssh_config(text: &str) -> Vec<HostProfile> {
    let mut hosts = Vec::new();
    let mut current: Option<HostProfile> = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let Some(keyword) = parts.next() else { continue };
        let value = parts.next().unwrap_or("").trim();

        match keyword.to_ascii_lowercase().as_str() {
            "host" => {
                if let Some(h) = current.take() {
                    if !h.alias.contains('*') && !h.alias.contains('?') {
                        hosts.push(h);
                    }
                }
                let alias = value.split_whitespace().next().unwrap_or("").to_string();
                current = Some(HostProfile { alias, ..Default::default() });
            }
            "hostname" => set(&mut current, |h| h.hostname = Some(value.to_string())),
            "user" => set(&mut current, |h| h.user = Some(value.to_string())),
            "port" => set(&mut current, |h| h.port = value.parse().ok()),
            "identityfile" => set(&mut current, |h| h.identity_file = Some(value.to_string())),
            "proxyjump" => set(&mut current, |h| h.proxy_jump = Some(value.to_string())),
            _ => {}
        }
    }
    if let Some(h) = current {
        if !h.alias.contains('*') && !h.alias.contains('?') {
            hosts.push(h);
        }
    }
    hosts
}

fn set(current: &mut Option<HostProfile>, f: impl FnOnce(&mut HostProfile)) {
    if let Some(h) = current {
        f(h);
    }
}

/// An in-memory registry of saved hosts, keyed by alias, with simple
/// group/tag lookups ("Host groups", "Host tags").
#[derive(Default)]
pub struct HostRegistry {
    hosts: HashMap<String, HostProfile>,
}

impl HostRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn import(&mut self, profiles: impl IntoIterator<Item = HostProfile>) {
        for p in profiles {
            self.hosts.insert(p.alias.clone(), p);
        }
    }

    pub fn get(&self, alias: &str) -> Option<&HostProfile> {
        self.hosts.get(alias)
    }

    pub fn by_tag<'a>(&'a self, tag: &'a str) -> impl Iterator<Item = &'a HostProfile> {
        self.hosts.values().filter(move |h| h.tags.iter().any(|t| t == tag))
    }

    pub fn by_group<'a>(&'a self, group: &'a str) -> impl Iterator<Item = &'a HostProfile> {
        self.hosts.values().filter(move |h| h.group.as_deref() == Some(group))
    }

    pub fn len(&self) -> usize {
        self.hosts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hosts.is_empty()
    }
}

pub struct SshPlugin {
    pub hosts: HostRegistry,
}

impl SshPlugin {
    pub fn new(hosts: HostRegistry) -> Self {
        SshPlugin { hosts }
    }
}

impl Plugin for SshPlugin {
    fn id(&self) -> &str {
        "ssh"
    }

    fn requested_permissions(&self) -> Permissions {
        Permissions { read_output: true, read_commands: true, ..Default::default() }
    }

    fn on_command(&mut self, _ctx: &PluginContext, command: &str) -> PluginAction {
        let Some(rest) = command.strip_prefix("ssh ") else { return PluginAction::None };
        let alias = rest.split_whitespace().next().unwrap_or("");
        match self.hosts.get(alias) {
            Some(host) if host.proxy_jump.is_some() => {
                PluginAction::ShowMessage(format!("'{alias}' connects via jump host {}", host.proxy_jump.as_deref().unwrap_or("?")))
            }
            _ => PluginAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_host_block() {
        let cfg = "Host prod\n    HostName 10.0.0.5\n    User deploy\n    Port 2222\n";
        let hosts = parse_ssh_config(cfg);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "prod");
        assert_eq!(hosts[0].hostname.as_deref(), Some("10.0.0.5"));
        assert_eq!(hosts[0].user.as_deref(), Some("deploy"));
        assert_eq!(hosts[0].port, Some(2222));
    }

    #[test]
    fn parses_multiple_hosts() {
        let cfg = "Host a\n  HostName a.example.com\nHost b\n  HostName b.example.com\n";
        let hosts = parse_ssh_config(cfg);
        assert_eq!(hosts.len(), 2);
        assert_eq!(hosts[0].alias, "a");
        assert_eq!(hosts[1].alias, "b");
    }

    #[test]
    fn skips_wildcard_hosts() {
        let cfg = "Host *\n  User root\nHost real\n  HostName real.example.com\n";
        let hosts = parse_ssh_config(cfg);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].alias, "real");
    }

    #[test]
    fn parses_proxy_jump_and_identity_file() {
        let cfg = "Host bastion-target\n  ProxyJump bastion\n  IdentityFile ~/.ssh/id_ed25519\n";
        let hosts = parse_ssh_config(cfg);
        assert_eq!(hosts[0].proxy_jump.as_deref(), Some("bastion"));
        assert_eq!(hosts[0].identity_file.as_deref(), Some("~/.ssh/id_ed25519"));
    }

    #[test]
    fn ignores_comments_and_blank_lines() {
        let cfg = "# a comment\n\nHost x\n  # nested comment\n  HostName x.example.com\n";
        let hosts = parse_ssh_config(cfg);
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].hostname.as_deref(), Some("x.example.com"));
    }

    #[test]
    fn registry_lookup_by_tag_and_group() {
        let mut reg = HostRegistry::new();
        reg.import(vec![
            HostProfile { alias: "web1".into(), tags: vec!["prod".into()], group: Some("web".into()), ..Default::default() },
            HostProfile { alias: "web2".into(), tags: vec!["staging".into()], group: Some("web".into()), ..Default::default() },
        ]);
        assert_eq!(reg.by_tag("prod").count(), 1);
        assert_eq!(reg.by_group("web").count(), 2);
    }

    #[test]
    fn plugin_flags_jump_host_usage() {
        let mut reg = HostRegistry::new();
        reg.import(vec![HostProfile { alias: "target".into(), proxy_jump: Some("bastion".into()), ..Default::default() }]);
        let mut plugin = SshPlugin::new(reg);
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines: Vec<String> = vec![];
        let ctx = PluginContext::new(Permissions::read_only(), &priv_ctx, &lines, None);
        let action = plugin.on_command(&ctx, "ssh target");
        assert!(matches!(action, PluginAction::ShowMessage(_)));
    }
}
