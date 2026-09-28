//! `ut-plugin-ai`: local-only and network-backed assistance (Section 15
//! "AI assistance").
//!
//! [`AiAssistant`] is the "no model, no network" floor required by
//! "Local-only mode" / "Disable network AI by default for private
//! sessions": a small offline lookup table that explains common commands
//! and flags ([`Explainer`]), plus wiring the `ut-safety` risk report into
//! [`PluginAction::ShowMessage`] so a command gets a plain-English risk
//! explanation ("Show risk classification") *before* it runs, with zero
//! network calls. [`provider::NetworkAssistant`] (in the [`provider`]
//! module) is the opt-in "Remote-provider mode" / "Ollama support" half —
//! it requests the `network` permission explicitly and checks it again at
//! the call site, so nothing here can silently start making network
//! requests. Neither ever executes a command on its own: every answer goes
//! through `ut-plugin-api::PluginContext` (never execute silently, always
//! require approval).

mod provider;

pub use provider::{AiProvider, NetworkAssistant, OllamaProvider, ProviderError};

use std::collections::HashMap;
use ut_plugin_api::{Permissions, Plugin, PluginAction, PluginContext};
use ut_safety::Risk;

/// A small, hand-curated offline explanation table. Keys are program names;
/// values are one-sentence, beginner-friendly explanations — "Command
/// explanation" / "Local manual-page search" without needing `man` or a model.
pub struct Explainer {
    entries: HashMap<&'static str, &'static str>,
}

impl Default for Explainer {
    fn default() -> Self {
        let mut entries = HashMap::new();
        entries.insert("ls", "Lists the files and folders in a directory.");
        entries.insert("cd", "Changes your current directory (where commands run from).");
        entries.insert("pwd", "Prints the full path of your current directory.");
        entries.insert("cat", "Prints a file's contents to the screen.");
        entries.insert("grep", "Searches text for lines matching a pattern.");
        entries.insert("chmod", "Changes who can read, write, or execute a file.");
        entries.insert("chown", "Changes which user/group owns a file.");
        entries.insert("sudo", "Runs the rest of the command as an administrator.");
        entries.insert("ps", "Lists currently running processes.");
        entries.insert("kill", "Sends a signal (often 'stop') to a running process.");
        entries.insert("ssh", "Opens a secure connection to a remote computer.");
        entries.insert("scp", "Copies files to or from a remote computer over SSH.");
        entries.insert("git", "Manages a version-controlled project's history.");
        entries.insert("pacman", "Arch Linux's package manager: install/remove/update software.");
        entries.insert("tar", "Bundles (and optionally compresses) files into an archive.");
        entries.insert("df", "Shows how much disk space is used/free on each filesystem.");
        entries.insert("du", "Shows how much disk space files/folders are using.");
        entries.insert("top", "Shows running processes and resource usage, updating live.");
        entries.insert("export", "Sets an environment variable for this shell and its children.");
        entries.insert("systemctl", "Starts, stops, or inspects system services (systemd).");
        Explainer { entries }
    }
}

impl Explainer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, program: &'static str, explanation: &'static str) {
        self.entries.insert(program, explanation);
    }

    /// Explain the program named by the first word of `command`, if known.
    pub fn explain(&self, command: &str) -> Option<&'static str> {
        let prog = command.split_whitespace().next()?;
        let prog = prog.rsplit('/').next().unwrap_or(prog);
        self.entries.get(prog).copied()
    }
}

pub struct AiAssistant {
    pub explainer: Explainer,
    privilege: ut_safety::PrivilegeContext,
    /// Below `ut_safety::Risk::Low` there's nothing worth surfacing proactively.
    pub min_risk_to_annotate: Risk,
}

impl AiAssistant {
    pub fn new(privilege: ut_safety::PrivilegeContext) -> Self {
        AiAssistant { explainer: Explainer::new(), privilege, min_risk_to_annotate: Risk::Medium }
    }

    /// Explain a command in plain language: the risk classification (if
    /// any) first, then what the program does. This is what a UI would
    /// show for "Explain selected output" / "Explain block" without
    /// calling any model.
    pub fn explain_command(&self, command: &str) -> Option<String> {
        let report = ut_safety::analyze(command, &self.privilege);
        let risk_line = report
            .findings
            .first()
            .filter(|f| f.risk >= self.min_risk_to_annotate)
            .map(|f| format!("Risk: {} — {}", f.risk, f.explanation));
        let explain_line = self.explainer.explain(command).map(|s| s.to_string());
        match (risk_line, explain_line) {
            (Some(r), Some(e)) => Some(format!("{r}\n{e}")),
            (Some(r), None) => Some(r),
            (None, Some(e)) => Some(e),
            (None, None) => None,
        }
    }
}

impl Plugin for AiAssistant {
    fn id(&self) -> &str {
        "ai-assistant"
    }

    fn requested_permissions(&self) -> Permissions {
        // Local-only mode: no network permission requested by default.
        Permissions { read_output: true, read_commands: true, suggest_input: true, ..Default::default() }
    }

    fn on_command(&mut self, _ctx: &PluginContext, command: &str) -> PluginAction {
        match self.explain_command(command) {
            Some(text) => PluginAction::ShowMessage(text),
            None => PluginAction::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ut_safety::PrivilegeContext;

    #[test]
    fn explains_known_program() {
        let explainer = Explainer::new();
        assert_eq!(explainer.explain("ls -la"), Some("Lists the files and folders in a directory."));
    }

    #[test]
    fn unknown_program_returns_none() {
        let explainer = Explainer::new();
        assert_eq!(explainer.explain("some-random-tool --flag"), None);
    }

    #[test]
    fn strips_path_prefix_before_lookup() {
        let explainer = Explainer::new();
        assert_eq!(explainer.explain("/usr/bin/ls -la"), Some("Lists the files and folders in a directory."));
    }

    #[test]
    fn custom_registration_overrides_lookup() {
        let mut explainer = Explainer::new();
        explainer.register("ls", "custom explanation");
        assert_eq!(explainer.explain("ls"), Some("custom explanation"));
    }

    #[test]
    fn explain_command_includes_risk_for_dangerous_commands() {
        let assistant = AiAssistant::new(PrivilegeContext::local_normal_user());
        let text = assistant.explain_command("rm -rf /").unwrap();
        assert!(text.starts_with("Risk:"));
    }

    #[test]
    fn explain_command_falls_back_to_plain_explanation() {
        let assistant = AiAssistant::new(PrivilegeContext::local_normal_user());
        let text = assistant.explain_command("ls -la").unwrap();
        assert_eq!(text, "Lists the files and folders in a directory.");
    }

    #[test]
    fn explain_command_none_for_unknown_safe_command() {
        let assistant = AiAssistant::new(PrivilegeContext::local_normal_user());
        assert_eq!(assistant.explain_command("some-random-tool"), None);
    }

    #[test]
    fn plugin_requests_no_network_permission_by_default() {
        let assistant = AiAssistant::new(PrivilegeContext::local_normal_user());
        assert!(!assistant.requested_permissions().network);
    }

    #[test]
    fn plugin_on_command_surfaces_explanation() {
        let mut assistant = AiAssistant::new(PrivilegeContext::local_normal_user());
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines: Vec<String> = vec![];
        let ctx = PluginContext::new(Permissions::read_only(), &priv_ctx, &lines, None);
        let action = assistant.on_command(&ctx, "chmod 755 script.sh");
        assert!(matches!(action, PluginAction::ShowMessage(_)));
    }
}
