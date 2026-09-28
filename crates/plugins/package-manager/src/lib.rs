//! `ut-plugin-package-manager`: Arch package-management helpers (Section 11).
//!
//! What's implemented now: pure text parsers for `pacman`/`checkupdates`
//! output — "Update preview", "Package list preview", "Download size",
//! "Installed-size change" — which need no process execution to be useful
//! or tested. Actually *running* pacman transactions (Section 11's
//! "Install packages" etc.) needs a privileged subprocess call, which
//! belongs in the app layer (it has to go through the same safety/
//! confirmation path as any other command — see `ut-session::Pane`), not
//! in this plugin.

use ut_plugin_api::{Permissions, Plugin, PluginAction, PluginContext};

/// One pending update, as reported by `checkupdates` or `pacman -Qu`
/// (`name old-version -> new-version`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingUpdate {
    pub name: String,
    pub old_version: String,
    pub new_version: String,
}

/// Parse `checkupdates`/`pacman -Qu`-style output, one update per line:
/// `name old -> new`. Lines that don't match are skipped.
pub fn parse_pending_updates(output: &str) -> Vec<PendingUpdate> {
    output
        .lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let name = it.next()?;
            let old_version = it.next()?;
            let arrow = it.next()?;
            let new_version = it.next()?;
            if arrow != "->" {
                return None;
            }
            Some(PendingUpdate { name: name.to_string(), old_version: old_version.to_string(), new_version: new_version.to_string() })
        })
        .collect()
}

/// A transaction size summary, parsed from `pacman -Sp`/`-Su --print`-style
/// verbose output lines such as:
/// `Total Download Size:    123.45 MiB`
/// `Total Installed Size:   456.78 MiB`
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TransactionSize {
    pub download_mib: Option<f64>,
    pub installed_mib: Option<f64>,
}

pub fn parse_transaction_size(output: &str) -> TransactionSize {
    let mut size = TransactionSize::default();
    for line in output.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Total Download Size:") {
            size.download_mib = parse_mib(rest);
        } else if let Some(rest) = line.strip_prefix("Total Installed Size:") {
            size.installed_mib = parse_mib(rest);
        }
    }
    size
}

fn parse_mib(text: &str) -> Option<f64> {
    let text = text.trim();
    let value_str = text.split_whitespace().next()?;
    let value: f64 = value_str.parse().ok()?;
    let unit = text.split_whitespace().nth(1).unwrap_or("MiB");
    Some(match unit {
        "KiB" => value / 1024.0,
        "GiB" => value * 1024.0,
        _ => value, // assume MiB
    })
}

/// Recognizes package-removal/install commands so the app layer can show a
/// friendlier preview before they run. Only classification, not execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageOperation {
    Install,
    Remove,
    Upgrade,
    Query,
    Other,
}

pub fn classify_pacman_command(command: &str) -> Option<PackageOperation> {
    let words: Vec<&str> = command.split_whitespace().collect();
    let prog = words.first().copied()?;
    if !matches!(prog, "pacman" | "yay" | "paru") {
        return None;
    }
    let flags = words.get(1).copied().unwrap_or("");
    Some(if flags.starts_with("-S") || flags == "install" {
        if flags.contains('y') && flags.contains('u') { PackageOperation::Upgrade } else { PackageOperation::Install }
    } else if flags.starts_with("-R") || flags == "remove" {
        PackageOperation::Remove
    } else if flags.starts_with("-Q") {
        PackageOperation::Query
    } else {
        PackageOperation::Other
    })
}

pub struct PackageManagerPlugin;

impl Plugin for PackageManagerPlugin {
    fn id(&self) -> &str {
        "package-manager"
    }

    fn requested_permissions(&self) -> Permissions {
        Permissions { read_commands: true, ..Default::default() }
    }

    fn on_command(&mut self, _ctx: &PluginContext, command: &str) -> PluginAction {
        match classify_pacman_command(command) {
            Some(PackageOperation::Remove) => {
                PluginAction::ShowMessage("Tip: run `pacman -Qi <package>` first to see what depends on it.".into())
            }
            _ => PluginAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pending_updates() {
        let out = "firefox 120.0-1 -> 121.0-1\nlinux 6.6.1-1 -> 6.6.2-1\n";
        let updates = parse_pending_updates(out);
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0], PendingUpdate { name: "firefox".into(), old_version: "120.0-1".into(), new_version: "121.0-1".into() });
    }

    #[test]
    fn ignores_malformed_update_lines() {
        let out = "not a valid line\nfirefox 120.0-1 -> 121.0-1\n";
        let updates = parse_pending_updates(out);
        assert_eq!(updates.len(), 1);
    }

    #[test]
    fn parses_transaction_size_in_mib() {
        let out = "Total Download Size:    123.45 MiB\nTotal Installed Size:   456.78 MiB\n";
        let size = parse_transaction_size(out);
        assert_eq!(size.download_mib, Some(123.45));
        assert_eq!(size.installed_mib, Some(456.78));
    }

    #[test]
    fn parses_transaction_size_with_unit_conversion() {
        let out = "Total Download Size:    2.50 GiB\nTotal Installed Size:   512.00 KiB\n";
        let size = parse_transaction_size(out);
        assert_eq!(size.download_mib, Some(2560.0));
        assert!((size.installed_mib.unwrap() - 0.5).abs() < 0.001);
    }

    #[test]
    fn classifies_install_and_remove() {
        assert_eq!(classify_pacman_command("pacman -S firefox"), Some(PackageOperation::Install));
        assert_eq!(classify_pacman_command("pacman -Rns firefox"), Some(PackageOperation::Remove));
        assert_eq!(classify_pacman_command("pacman -Syu"), Some(PackageOperation::Upgrade));
        assert_eq!(classify_pacman_command("ls -la"), None);
    }

    #[test]
    fn plugin_suggests_dependency_check_on_remove() {
        let mut plugin = PackageManagerPlugin;
        let priv_ctx = ut_plugin_api_test_support::privilege();
        let lines: Vec<String> = vec![];
        let ctx = PluginContext::new(Permissions::default(), &priv_ctx, &lines, None);
        let action = plugin.on_command(&ctx, "pacman -Rns firefox");
        assert!(matches!(action, PluginAction::ShowMessage(_)));
    }

    mod ut_plugin_api_test_support {
        pub fn privilege() -> ut_safety::PrivilegeContext {
            ut_safety::PrivilegeContext::local_normal_user()
        }
    }
}
