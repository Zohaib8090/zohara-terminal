//! `ut-config`: typed configuration schema (Section 5 "Profiles and appearance").
//!
//! Defines the on-disk shape of a profile — shell, starting directory,
//! environment, font, theme, scrollback, confirmation settings — and how
//! several of them merge via inheritance (`Profile::merged_with`, the
//! "Profile inheritance" requirement). This crate only knows about *data*:
//! reading, validating, and merging config. It has no opinion about where
//! the file lives on disk, how it's watched for changes, or how a UI edits
//! it — that belongs to `ut-profile` (the service) and `ut-ui`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A complete or partial terminal profile. Every field is optional so a
/// profile can be a small override that inherits the rest from a parent
/// (`extends`), matching "Profile inheritance" in the spec.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    /// Name of a parent profile this one inherits unset fields from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extends: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub starting_directory: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub environment: HashMap<String, String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_shape: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback_lines: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bell_style: Option<BellStyle>,

    /// Whether destructive-command confirmation prompts are shown for this profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_destructive_commands: Option<bool>,
    /// How readily destructive-command confirmations fire, as a kebab-case
    /// `ut_safety::Risk` name ("low", "medium", "high", "critical").
    /// "medium" (the default) confirms common footguns; "critical" only
    /// the disk-destroyers; "low" almost everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning_sensitivity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tab_color: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub startup_commands: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BellStyle {
    None,
    Visual,
    Audible,
    Both,
}

impl Profile {
    /// Merge `self` on top of `parent`: any field `self` leaves unset falls
    /// back to `parent`'s value. `environment` maps are merged key-wise
    /// (self wins on conflicts); `startup_commands` from the parent run
    /// first, then self's.
    pub fn merged_with(&self, parent: &Profile) -> Profile {
        let mut env = parent.environment.clone();
        env.extend(self.environment.clone());

        let mut startup_commands = parent.startup_commands.clone();
        startup_commands.extend(self.startup_commands.clone());

        Profile {
            extends: None, // resolved, not inherited further
            shell: self.shell.clone().or_else(|| parent.shell.clone()),
            starting_directory: self.starting_directory.clone().or_else(|| parent.starting_directory.clone()),
            environment: env,
            font_family: self.font_family.clone().or_else(|| parent.font_family.clone()),
            font_size: self.font_size.or(parent.font_size),
            theme: self.theme.clone().or_else(|| parent.theme.clone()),
            cursor_shape: self.cursor_shape.clone().or_else(|| parent.cursor_shape.clone()),
            scrollback_lines: self.scrollback_lines.or(parent.scrollback_lines),
            bell_style: self.bell_style.or(parent.bell_style),
            confirm_destructive_commands: self.confirm_destructive_commands.or(parent.confirm_destructive_commands),
            warning_sensitivity: self.warning_sensitivity.clone().or_else(|| parent.warning_sensitivity.clone()),
            tab_color: self.tab_color.clone().or_else(|| parent.tab_color.clone()),
            startup_commands,
        }
    }
}

/// The full configuration file: a set of named profiles plus the name of
/// the default one to use when none is specified.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub default_profile: Option<String>,
    #[serde(default)]
    pub profiles: HashMap<String, Profile>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not parse config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("profile '{0}' extends unknown profile '{1}'")]
    UnknownParent(String, String),
    #[error("profile inheritance cycle detected starting at '{0}'")]
    InheritanceCycle(String),
}

impl Config {
    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(s)?)
    }

    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }

    /// Validate that every `extends` reference points to a real profile and
    /// that there are no inheritance cycles.
    pub fn validate(&self) -> Result<(), ConfigError> {
        for name in self.profiles.keys() {
            self.resolve_chain(name)?;
        }
        Ok(())
    }

    fn resolve_chain<'a>(&'a self, name: &'a str) -> Result<Vec<&'a str>, ConfigError> {
        let mut chain = Vec::new();
        let mut current = name;
        loop {
            if chain.contains(&current) {
                return Err(ConfigError::InheritanceCycle(name.to_string()));
            }
            chain.push(current);
            let profile = self.profiles.get(current);
            match profile.and_then(|p| p.extends.as_deref()) {
                Some(parent) => {
                    if !self.profiles.contains_key(parent) {
                        return Err(ConfigError::UnknownParent(current.to_string(), parent.to_string()));
                    }
                    current = parent;
                }
                None => break,
            }
        }
        Ok(chain)
    }

    /// Fully resolve a profile by name, walking its `extends` chain and
    /// merging from the root parent down to the named profile.
    pub fn resolve(&self, name: &str) -> Result<Profile, ConfigError> {
        let chain = self.resolve_chain(name)?;
        let mut resolved = Profile::default();
        for link in chain.into_iter().rev() {
            let p = self.profiles.get(link).expect("chain only contains known profiles");
            resolved = p.merged_with(&resolved);
        }
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_toml() {
        let toml = r#"
            default_profile = "default"

            [profiles.default]
            shell = "/bin/zsh"
            font_size = 13.0
        "#;
        let cfg = Config::from_toml_str(toml).unwrap();
        assert_eq!(cfg.default_profile.as_deref(), Some("default"));
        assert_eq!(cfg.profiles["default"].shell.as_deref(), Some("/bin/zsh"));
    }

    #[test]
    fn merges_inherited_fields() {
        let mut child = Profile { extends: Some("base".into()), theme: Some("dracula".into()), ..Default::default() };
        let base = Profile { shell: Some("/bin/bash".into()), font_size: Some(12.0), ..Default::default() };
        child = child.merged_with(&base);
        assert_eq!(child.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(child.theme.as_deref(), Some("dracula"));
        assert_eq!(child.font_size, Some(12.0));
    }

    #[test]
    fn child_overrides_parent() {
        let child = Profile { font_size: Some(16.0), ..Default::default() };
        let base = Profile { font_size: Some(12.0), ..Default::default() };
        let merged = child.merged_with(&base);
        assert_eq!(merged.font_size, Some(16.0));
    }

    #[test]
    fn environment_merges_key_wise() {
        let mut base_env = HashMap::new();
        base_env.insert("A".to_string(), "1".to_string());
        base_env.insert("B".to_string(), "2".to_string());
        let base = Profile { environment: base_env, ..Default::default() };

        let mut child_env = HashMap::new();
        child_env.insert("B".to_string(), "override".to_string());
        let child = Profile { environment: child_env, ..Default::default() };

        let merged = child.merged_with(&base);
        assert_eq!(merged.environment.get("A").unwrap(), "1");
        assert_eq!(merged.environment.get("B").unwrap(), "override");
    }

    #[test]
    fn resolve_walks_multi_level_inheritance() {
        let mut profiles = HashMap::new();
        profiles.insert(
            "base".to_string(),
            Profile { shell: Some("/bin/bash".into()), scrollback_lines: Some(10_000), ..Default::default() },
        );
        profiles.insert(
            "dev".to_string(),
            Profile { extends: Some("base".into()), theme: Some("dark".into()), ..Default::default() },
        );
        profiles.insert(
            "dev-remote".to_string(),
            Profile { extends: Some("dev".into()), shell: Some("/bin/zsh".into()), ..Default::default() },
        );
        let cfg = Config { default_profile: None, profiles };

        let resolved = cfg.resolve("dev-remote").unwrap();
        assert_eq!(resolved.shell.as_deref(), Some("/bin/zsh")); // overridden at leaf
        assert_eq!(resolved.theme.as_deref(), Some("dark")); // inherited from dev
        assert_eq!(resolved.scrollback_lines, Some(10_000)); // inherited from base
    }

    #[test]
    fn detects_cycle() {
        let mut profiles = HashMap::new();
        profiles.insert("a".to_string(), Profile { extends: Some("b".into()), ..Default::default() });
        profiles.insert("b".to_string(), Profile { extends: Some("a".into()), ..Default::default() });
        let cfg = Config { default_profile: None, profiles };
        assert!(matches!(cfg.validate(), Err(ConfigError::InheritanceCycle(_))));
    }

    #[test]
    fn detects_unknown_parent() {
        let mut profiles = HashMap::new();
        profiles.insert("a".to_string(), Profile { extends: Some("ghost".into()), ..Default::default() });
        let cfg = Config { default_profile: None, profiles };
        assert!(matches!(cfg.validate(), Err(ConfigError::UnknownParent(_, _))));
    }

    #[test]
    fn round_trips_through_toml() {
        let mut profiles = HashMap::new();
        profiles.insert("default".to_string(), Profile { shell: Some("/bin/fish".into()), ..Default::default() });
        let cfg = Config { default_profile: Some("default".into()), profiles };
        let s = cfg.to_toml_string().unwrap();
        let back = Config::from_toml_str(&s).unwrap();
        assert_eq!(back.profiles["default"].shell.as_deref(), Some("/bin/fish"));
    }
}
