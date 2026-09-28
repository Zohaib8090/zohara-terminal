//! `ut-plugin-api`: the contract every plugin implements.
//!
//! This crate is the enforcement point for Section 19 ("Plugin and
//! developer platform") and the permission-scoping half of Section 15
//! ("AI safety" — "Agent permission scopes", "Read-only agent mode",
//! "Ask-before-write mode", etc.): a plugin cannot call anything on
//! `ut-session`, `ut-history`, or the PTY directly. It only ever sees a
//! [`PluginContext`], and that context only exposes the operations its
//! [`Permissions`] grant. Widening what a plugin can do means widening
//! `PluginContext`'s methods, in one reviewable place — not auditing every
//! plugin for what it happens to import.
//!
//! A plugin host (part of `ut-session`, eventually) is responsible for:
//! constructing a `PluginContext` scoped to one plugin's granted
//! `Permissions`, calling the [`Plugin`] trait's hooks at the right times,
//! and never handing a plugin a capability its permissions don't list.

use ut_safety::{PrivilegeContext, Report};

/// What a plugin is allowed to do. Mirrors the spec's "Plugin permissions"
/// and "Agent permission scopes": every capability is opt-in and explicit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Permissions {
    /// Can read terminal output/state (scrollback, cwd, exit codes) but not act on it.
    pub read_output: bool,
    /// Can read (but not rewrite) the command the user is about to run.
    pub read_commands: bool,
    /// Can inject text into the input line for the user to review/edit (never auto-submit).
    pub suggest_input: bool,
    /// Can write directly to the PTY (run commands without the user typing them).
    /// The spec requires this to always be paired with an approval step at
    /// the UI layer — this flag only controls whether the *capability*
    /// exists for this plugin at all.
    pub write_input: bool,
    /// Can make outbound network requests.
    pub network: bool,
    /// Can invoke `sudo`/privilege elevation (still subject to safety checks).
    pub elevate_privileges: bool,
    /// Filesystem paths this plugin may read/write outside of what the
    /// terminal itself already shows it (e.g. SFTP browsing a remote host).
    /// Empty means "no filesystem access beyond terminal output".
    pub filesystem_roots: Vec<String>,
}

impl Permissions {
    /// The most restrictive useful permission set: can see output, nothing else.
    pub fn read_only() -> Self {
        Permissions { read_output: true, ..Default::default() }
    }
}

/// A narrow, permission-checked window into terminal/session state that a
/// plugin is handed for the duration of one call. Every method here either
/// no-ops or returns `Err`/`None` when the plugin's [`Permissions`] don't
/// allow it — a plugin cannot bypass this by holding a reference longer or
/// calling a method twice.
pub struct PluginContext<'a> {
    permissions: Permissions,
    privilege: &'a PrivilegeContext,
    /// Recent output lines available to plugins with `read_output`.
    recent_output: &'a [String],
    cwd: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginError {
    PermissionDenied(&'static str),
}

impl<'a> PluginContext<'a> {
    pub fn new(permissions: Permissions, privilege: &'a PrivilegeContext, recent_output: &'a [String], cwd: Option<&'a str>) -> Self {
        PluginContext { permissions, privilege, recent_output, cwd }
    }

    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }

    pub fn recent_output(&self) -> Result<&'a [String], PluginError> {
        if self.permissions.read_output {
            Ok(self.recent_output)
        } else {
            Err(PluginError::PermissionDenied("read_output"))
        }
    }

    pub fn current_directory(&self) -> Result<Option<&'a str>, PluginError> {
        if self.permissions.read_output {
            Ok(self.cwd)
        } else {
            Err(PluginError::PermissionDenied("read_output"))
        }
    }

    pub fn privilege(&self) -> &'a PrivilegeContext {
        self.privilege
    }

    /// Run a proposed command through the same safety analysis the
    /// interactive shell path uses, so a plugin-generated command gets the
    /// exact same warnings a typed one would ("Never execute silently").
    pub fn check_command_safety(&self, command: &str) -> Report {
        ut_safety::analyze(command, self.privilege)
    }

    /// Propose text for the user's input line. Always requires explicit
    /// user action to run — this never executes anything itself, matching
    /// "Show generated command before execution" / "Require explicit
    /// approval to run" / "Never execute silently".
    pub fn suggest_input(&self, action: PluginAction) -> Result<PluginAction, PluginError> {
        if self.permissions.suggest_input || self.permissions.write_input {
            Ok(action)
        } else {
            Err(PluginError::PermissionDenied("suggest_input"))
        }
    }
}

/// What a plugin wants to happen next, returned from a hook. The host
/// decides how to realize it (e.g. `SuggestCommand` populates the input
/// line but never submits it) — a `Plugin` implementation cannot force
/// execution by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginAction {
    /// Do nothing.
    None,
    /// Put this text in the input line for the user to review and run.
    SuggestCommand(String),
    /// Show an informational message to the user (e.g. an explanation, a
    /// log summary) without touching the input line.
    ShowMessage(String),
    /// Ask the host to run a command autonomously — only realized if this
    /// plugin also holds `write_input`, and still subject to the same
    /// destructive-command confirmation flow as anything else.
    RunCommand(String),
}

/// The trait every plugin implements. Hooks are called by the host at the
/// obvious points in the terminal's lifecycle; a plugin that doesn't care
/// about a hook just uses the default no-op.
pub trait Plugin {
    /// Stable identifier, e.g. `"ssh"`, `"ai-assistant"`. Used for permission grants and logs.
    fn id(&self) -> &str;

    /// Called once when the plugin is loaded, with the permissions it was
    /// granted (which may be less than it requested — see `requested_permissions`).
    fn on_load(&mut self, _ctx: &PluginContext) {}

    /// The permissions this plugin would like, for the plugin manager UI to
    /// present to the user ("Plugin permissions").
    fn requested_permissions(&self) -> Permissions {
        Permissions::read_only()
    }

    /// Called just before a command the user typed is about to run.
    fn on_command(&mut self, _ctx: &PluginContext, _command: &str) -> PluginAction {
        PluginAction::None
    }

    /// Called with each chunk of output as it arrives.
    fn on_output(&mut self, _ctx: &PluginContext, _chunk: &str) -> PluginAction {
        PluginAction::None
    }

    /// Called when the plugin is about to be unloaded (disabled, removed, app shutdown).
    fn on_unload(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Probe {
        got_output: bool,
    }

    impl Plugin for Probe {
        fn id(&self) -> &str {
            "probe"
        }
        fn on_output(&mut self, ctx: &PluginContext, _chunk: &str) -> PluginAction {
            self.got_output = ctx.recent_output().is_ok();
            PluginAction::None
        }
    }

    #[test]
    fn read_only_permission_grants_output_access() {
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines = vec!["line one".to_string()];
        let ctx = PluginContext::new(Permissions::read_only(), &priv_ctx, &lines, Some("/home/user"));
        assert!(ctx.recent_output().is_ok());
        assert_eq!(ctx.recent_output().unwrap(), &lines[..]);
    }

    #[test]
    fn no_permissions_denies_output_access() {
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines = vec!["secret".to_string()];
        let ctx = PluginContext::new(Permissions::default(), &priv_ctx, &lines, None);
        assert_eq!(ctx.recent_output(), Err(PluginError::PermissionDenied("read_output")));
    }

    #[test]
    fn suggest_input_denied_without_permission() {
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines: Vec<String> = vec![];
        let ctx = PluginContext::new(Permissions::default(), &priv_ctx, &lines, None);
        let result = ctx.suggest_input(PluginAction::SuggestCommand("ls".into()));
        assert_eq!(result, Err(PluginError::PermissionDenied("suggest_input")));
    }

    #[test]
    fn suggest_input_allowed_with_permission() {
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines: Vec<String> = vec![];
        let perms = Permissions { suggest_input: true, ..Default::default() };
        let ctx = PluginContext::new(perms, &priv_ctx, &lines, None);
        let result = ctx.suggest_input(PluginAction::SuggestCommand("ls".into()));
        assert_eq!(result, Ok(PluginAction::SuggestCommand("ls".into())));
    }

    #[test]
    fn command_safety_check_flows_through_plugin_context() {
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines: Vec<String> = vec![];
        let ctx = PluginContext::new(Permissions::read_only(), &priv_ctx, &lines, None);
        let report = ctx.check_command_safety("rm -rf /");
        assert!(!report.is_empty());
    }

    #[test]
    fn plugin_trait_hook_respects_context_permissions() {
        let priv_ctx = PrivilegeContext::local_normal_user();
        let lines = vec!["hi".to_string()];
        let ctx = PluginContext::new(Permissions::read_only(), &priv_ctx, &lines, None);
        let mut probe = Probe { got_output: false };
        probe.on_output(&ctx, "hi");
        assert!(probe.got_output);
    }
}
