//! `ut-plugin-workflow`: command sequences with variables, conditional
//! failure handling, retries, and timeouts (Section 14 "Automation and
//! workflows").
//!
//! [`Workflow::run`] is executor-agnostic: it takes a closure that actually
//! runs one command and reports back an exit code, so it can be driven by
//! `ut-session::Pane::submit_input` in the app layer (going through the
//! same safety checks as anything else) without this crate depending on
//! `ut-pty`/`ut-session` at all. What's not implemented yet: the visual/
//! text workflow *editor*, scheduling, and the exporters to Bash/Fish/Zsh/
//! PowerShell/YAML/JSON — this crate is the execution model those will sit on.

use std::collections::HashMap;
use std::time::Duration;
use ut_plugin_api::{Permissions, Plugin, PluginAction, PluginContext};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OnFailure {
    /// Stop the whole workflow (the default).
    Stop,
    /// Log the failure and continue to the next step.
    Continue,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub name: String,
    /// May reference `{{VAR}}` placeholders resolved from the workflow's variables.
    pub command: String,
    pub on_failure: OnFailure,
    /// Total attempts (1 = no retry). A step is only retried on failure.
    pub max_attempts: u32,
    pub timeout: Option<Duration>,
}

impl Step {
    pub fn new(name: impl Into<String>, command: impl Into<String>) -> Self {
        Step { name: name.into(), command: command.into(), on_failure: OnFailure::Stop, max_attempts: 1, timeout: None }
    }

    pub fn with_retry(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts.max(1);
        self
    }

    pub fn continue_on_failure(mut self) -> Self {
        self.on_failure = OnFailure::Continue;
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workflow {
    pub name: String,
    pub steps: Vec<Step>,
    pub variables: HashMap<String, String>,
}

impl Workflow {
    pub fn new(name: impl Into<String>) -> Self {
        Workflow { name: name.into(), steps: Vec::new(), variables: HashMap::new() }
    }

    pub fn with_step(mut self, step: Step) -> Self {
        self.steps.push(step);
        self
    }

    pub fn with_variable(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.variables.insert(key.into(), value.into());
        self
    }

    /// Substitute `{{VAR}}` placeholders in `command` from this workflow's variables.
    fn resolve(&self, command: &str) -> String {
        let mut out = command.to_string();
        for (k, v) in &self.variables {
            out = out.replace(&format!("{{{{{k}}}}}"), v);
        }
        out
    }
}

/// What running one attempt of a step actually did — supplied by the caller's executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepResult {
    pub exit_code: i32,
    pub timed_out: bool,
}

impl StepResult {
    pub fn success() -> Self {
        StepResult { exit_code: 0, timed_out: false }
    }

    fn succeeded(self) -> bool {
        !self.timed_out && self.exit_code == 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub step_name: String,
    pub attempts: u32,
    pub result: StepResult,
    pub skipped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    pub outcomes: Vec<StepOutcome>,
    /// True if every non-skipped step that was required to succeed did.
    pub overall_success: bool,
}

impl Workflow {
    /// Execute every step in order using `executor` to actually run a
    /// resolved command string. Stops early if a `Stop`-on-failure step
    /// exhausts its retries; steps after that point are recorded as
    /// `skipped`. Steps configured `continue_on_failure` never stop the run.
    pub fn run(&self, mut executor: impl FnMut(&str, Option<Duration>) -> StepResult) -> WorkflowRun {
        let mut outcomes = Vec::with_capacity(self.steps.len());
        let mut halted = false;
        let mut overall_success = true;

        for step in &self.steps {
            if halted {
                outcomes.push(StepOutcome { step_name: step.name.clone(), attempts: 0, result: StepResult::success(), skipped: true });
                continue;
            }

            let resolved = self.resolve(&step.command);
            let mut attempts = 0;
            let mut last_result = StepResult { exit_code: -1, timed_out: false };
            while attempts < step.max_attempts {
                attempts += 1;
                last_result = executor(&resolved, step.timeout);
                if last_result.succeeded() {
                    break;
                }
            }

            if !last_result.succeeded() {
                overall_success = false;
                if step.on_failure == OnFailure::Stop {
                    halted = true;
                }
            }

            outcomes.push(StepOutcome { step_name: step.name.clone(), attempts, result: last_result, skipped: false });
        }

        WorkflowRun { outcomes, overall_success }
    }
}

/// A registry of saved workflows the terminal can offer to run — the
/// `Plugin` surface just exposes "did you mean to run workflow X" style
/// suggestions when a command matches a saved workflow's name; actually
/// starting a run goes through the app layer via [`Workflow::run`].
#[derive(Default)]
pub struct WorkflowPlugin {
    pub saved: HashMap<String, Workflow>,
}

impl WorkflowPlugin {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn save(&mut self, workflow: Workflow) {
        self.saved.insert(workflow.name.clone(), workflow);
    }
}

impl Plugin for WorkflowPlugin {
    fn id(&self) -> &str {
        "workflow"
    }

    fn requested_permissions(&self) -> Permissions {
        Permissions { read_commands: true, suggest_input: true, ..Default::default() }
    }

    fn on_command(&mut self, _ctx: &PluginContext, command: &str) -> PluginAction {
        let name = command.trim();
        if let Some(wf) = self.saved.get(name) {
            return PluginAction::ShowMessage(format!("'{name}' matches saved workflow '{}' ({} steps)", wf.name, wf.steps.len()));
        }
        PluginAction::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_all_steps_on_success() {
        let wf = Workflow::new("build").with_step(Step::new("a", "true")).with_step(Step::new("b", "true"));
        let run = wf.run(|_cmd, _timeout| StepResult::success());
        assert!(run.overall_success);
        assert_eq!(run.outcomes.len(), 2);
        assert!(run.outcomes.iter().all(|o| !o.skipped));
    }

    #[test]
    fn stop_on_failure_skips_remaining_steps() {
        let wf = Workflow::new("build")
            .with_step(Step::new("a", "false"))
            .with_step(Step::new("b", "true"));
        let run = wf.run(|_cmd, _timeout| StepResult { exit_code: 1, timed_out: false });
        assert!(!run.overall_success);
        assert!(!run.outcomes[0].skipped);
        assert!(run.outcomes[1].skipped);
    }

    #[test]
    fn continue_on_failure_keeps_running() {
        let wf = Workflow::new("build")
            .with_step(Step::new("a", "false").continue_on_failure())
            .with_step(Step::new("b", "true"));
        let run = wf.run(|cmd, _timeout| if cmd == "false" { StepResult { exit_code: 1, timed_out: false } } else { StepResult::success() });
        assert!(!run.overall_success); // step a failed
        assert!(!run.outcomes[1].skipped); // but b still ran
    }

    #[test]
    fn retries_up_to_max_attempts() {
        let mut call_count = 0;
        let wf = Workflow::new("flaky").with_step(Step::new("a", "flaky-cmd").with_retry(3));
        let run = wf.run(|_cmd, _timeout| {
            call_count += 1;
            if call_count < 3 { StepResult { exit_code: 1, timed_out: false } } else { StepResult::success() }
        });
        assert!(run.overall_success);
        assert_eq!(run.outcomes[0].attempts, 3);
    }

    #[test]
    fn gives_up_after_max_attempts() {
        let wf = Workflow::new("always-fails").with_step(Step::new("a", "cmd").with_retry(2));
        let run = wf.run(|_cmd, _timeout| StepResult { exit_code: 1, timed_out: false });
        assert!(!run.overall_success);
        assert_eq!(run.outcomes[0].attempts, 2);
    }

    #[test]
    fn variable_substitution_applies_before_execution() {
        let wf = Workflow::new("deploy")
            .with_variable("ENV", "staging")
            .with_step(Step::new("deploy", "deploy.sh --env={{ENV}}"));
        let mut seen_command = String::new();
        wf.run(|cmd, _timeout| {
            seen_command = cmd.to_string();
            StepResult::success()
        });
        assert_eq!(seen_command, "deploy.sh --env=staging");
    }

    #[test]
    fn timeout_counts_as_failure() {
        let wf = Workflow::new("slow").with_step(Step::new("a", "sleep 100").with_timeout(Duration::from_secs(1)));
        let run = wf.run(|_cmd, _timeout| StepResult { exit_code: 0, timed_out: true });
        assert!(!run.overall_success);
        assert!(run.outcomes[0].result.timed_out);
    }

    #[test]
    fn plugin_recognizes_saved_workflow_name() {
        let mut plugin = WorkflowPlugin::new();
        plugin.save(Workflow::new("deploy-staging").with_step(Step::new("a", "true")));
        let priv_ctx = ut_safety::PrivilegeContext::local_normal_user();
        let lines: Vec<String> = vec![];
        let ctx = PluginContext::new(Permissions::default(), &priv_ctx, &lines, None);
        let action = plugin.on_command(&ctx, "deploy-staging");
        assert!(matches!(action, PluginAction::ShowMessage(_)));
    }
}
