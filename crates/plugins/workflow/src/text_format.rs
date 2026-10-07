//! Text-editable workflow definitions (Section 14 "Text workflow editor" /
//! "Export to YAML" / "Export to JSON").
//!
//! A [`Workflow`] is already plain data; this module is just the
//! serializable mirror of it ([`WorkflowDef`]) plus the JSON/YAML
//! round-trip on [`Workflow`] itself. It's a separate shape rather than
//! deriving `Serialize`/`Deserialize` straight on [`Workflow`]/[`Step`]
//! because [`std::time::Duration`] has no serde support in std — `timeout`
//! becomes a plain `timeout_secs: Option<u64>` on the wire, which is also
//! what makes the YAML/JSON actually pleasant to hand-edit (nobody wants
//! to write `{"secs": 30, "nanos": 0}` in a workflow file).
//!
//! Editing a workflow as text *is* "Text workflow editor" at the data
//! layer — open the YAML in any editor, change a command or a retry
//! count, load it back with [`Workflow::from_yaml`]. A dedicated UI for
//! this is still future work, layered on top of this round-trip.

use crate::{OnFailure, Step, Workflow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OnFailureDef {
    Stop,
    Continue,
}

impl From<&OnFailure> for OnFailureDef {
    fn from(v: &OnFailure) -> Self {
        match v {
            OnFailure::Stop => OnFailureDef::Stop,
            OnFailure::Continue => OnFailureDef::Continue,
        }
    }
}

impl From<OnFailureDef> for OnFailure {
    fn from(v: OnFailureDef) -> Self {
        match v {
            OnFailureDef::Stop => OnFailure::Stop,
            OnFailureDef::Continue => OnFailure::Continue,
        }
    }
}

fn default_max_attempts() -> u32 {
    1
}

fn default_on_failure() -> OnFailureDef {
    OnFailureDef::Stop
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StepDef {
    name: String,
    command: String,
    #[serde(default = "default_on_failure")]
    on_failure: OnFailureDef,
    #[serde(default = "default_max_attempts")]
    max_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    timeout_secs: Option<u64>,
}

impl From<&Step> for StepDef {
    fn from(s: &Step) -> Self {
        StepDef {
            name: s.name.clone(),
            command: s.command.clone(),
            on_failure: (&s.on_failure).into(),
            max_attempts: s.max_attempts,
            timeout_secs: s.timeout.map(|d| d.as_secs()),
        }
    }
}

impl From<StepDef> for Step {
    fn from(d: StepDef) -> Self {
        Step {
            name: d.name,
            command: d.command,
            on_failure: d.on_failure.into(),
            max_attempts: d.max_attempts.max(1),
            timeout: d.timeout_secs.map(Duration::from_secs),
        }
    }
}

/// The serializable, human-editable mirror of a [`Workflow`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowDef {
    name: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    variables: HashMap<String, String>,
    steps: Vec<StepDef>,
}

impl From<&Workflow> for WorkflowDef {
    fn from(w: &Workflow) -> Self {
        WorkflowDef { name: w.name.clone(), variables: w.variables.clone(), steps: w.steps.iter().map(StepDef::from).collect() }
    }
}

impl From<WorkflowDef> for Workflow {
    fn from(d: WorkflowDef) -> Self {
        Workflow { name: d.name, variables: d.variables, steps: d.steps.into_iter().map(Step::from).collect() }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TextFormatError {
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),
}

impl Workflow {
    pub fn to_json(&self) -> Result<String, TextFormatError> {
        Ok(serde_json::to_string_pretty(&WorkflowDef::from(self))?)
    }

    pub fn from_json(text: &str) -> Result<Workflow, TextFormatError> {
        let def: WorkflowDef = serde_json::from_str(text)?;
        Ok(def.into())
    }

    pub fn to_yaml(&self) -> Result<String, TextFormatError> {
        Ok(serde_yaml::to_string(&WorkflowDef::from(self))?)
    }

    pub fn from_yaml(text: &str) -> Result<Workflow, TextFormatError> {
        let def: WorkflowDef = serde_yaml::from_str(text)?;
        Ok(def.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Step;

    fn sample() -> Workflow {
        Workflow::new("deploy")
            .with_variable("ENV", "staging")
            .with_step(Step::new("build", "make build").with_retry(2))
            .with_step(Step::new("deploy", "deploy.sh --env={{ENV}}").continue_on_failure().with_timeout(std::time::Duration::from_secs(30)))
    }

    #[test]
    fn json_round_trips_all_fields() {
        let wf = sample();
        let json = wf.to_json().unwrap();
        let back = Workflow::from_json(&json).unwrap();
        assert_eq!(back.name, "deploy");
        assert_eq!(back.variables.get("ENV").unwrap(), "staging");
        assert_eq!(back.steps.len(), 2);
        assert_eq!(back.steps[0].max_attempts, 2);
        assert_eq!(back.steps[1].on_failure, OnFailure::Continue);
        assert_eq!(back.steps[1].timeout, Some(std::time::Duration::from_secs(30)));
    }

    #[test]
    fn yaml_round_trips_all_fields() {
        let wf = sample();
        let yaml = wf.to_yaml().unwrap();
        let back = Workflow::from_yaml(&yaml).unwrap();
        assert_eq!(back, wf);
    }

    #[test]
    fn yaml_is_hand_editable_plain_text() {
        let wf = sample();
        let yaml = wf.to_yaml().unwrap();
        assert!(yaml.contains("name: deploy"));
        assert!(yaml.contains("make build"));
        assert!(!yaml.contains("nanos")); // no raw Duration struct ({secs, nanos}) leaking through
    }

    #[test]
    fn missing_optional_fields_default_sensibly() {
        let minimal = "name: quick\nsteps:\n  - name: a\n    command: echo hi\n";
        let wf = Workflow::from_yaml(minimal).unwrap();
        assert_eq!(wf.steps[0].max_attempts, 1);
        assert_eq!(wf.steps[0].on_failure, OnFailure::Stop);
        assert_eq!(wf.steps[0].timeout, None);
    }

    #[test]
    fn invalid_yaml_returns_error() {
        let result = Workflow::from_yaml("not: [valid, workflow");
        assert!(result.is_err());
    }

    #[test]
    fn invalid_json_returns_error() {
        let result = Workflow::from_json("{not valid json");
        assert!(result.is_err());
    }
}
