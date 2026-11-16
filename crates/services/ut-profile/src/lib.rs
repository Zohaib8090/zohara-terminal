//! `ut-profile`: loads/saves [`ut_config::Config`] from disk and tracks
//! which profile is active (Section 5 "Profiles").
//!
//! `ut-config` only knows about the *shape* of a profile and how
//! inheritance merges; this crate is the thin, stateful wrapper around it
//! that an app actually holds: it knows the config file's path, whether
//! it's been modified since load ("Configuration backup" / "Atomic
//! configuration writes" build on top of this), and which profile the
//! current session picked.

use std::fs;
use std::path::{Path, PathBuf};
use ut_config::{Config, ConfigError, Profile};

#[derive(Debug)]
pub enum ProfileServiceError {
    Io(std::io::Error),
    Config(ConfigError),
    NotFound(String),
}

impl From<std::io::Error> for ProfileServiceError {
    fn from(e: std::io::Error) -> Self {
        ProfileServiceError::Io(e)
    }
}

impl std::fmt::Display for ProfileServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProfileServiceError::Io(e) => write!(f, "I/O error: {e}"),
            ProfileServiceError::Config(e) => write!(f, "{e}"),
            ProfileServiceError::NotFound(name) => write!(f, "profile '{name}' not found"),
        }
    }
}

impl From<ConfigError> for ProfileServiceError {
    fn from(e: ConfigError) -> Self {
        ProfileServiceError::Config(e)
    }
}

pub struct ProfileService {
    path: PathBuf,
    config: Config,
    active_profile: Option<String>,
}

impl ProfileService {
    /// Load from `path` if it exists, otherwise start from an empty config
    /// (matches "Corrupt-settings recovery" / "Safe-mode startup": a
    /// missing or unreadable file is not a hard failure at this layer).
    pub fn load_or_default(path: impl Into<PathBuf>) -> Result<Self, ProfileServiceError> {
        let path = path.into();
        let config = if path.exists() {
            let text = fs::read_to_string(&path)?;
            let cfg = Config::from_toml_str(&text)?;
            cfg.validate()?;
            cfg
        } else {
            Config::default()
        };
        let active_profile = config.default_profile.clone();
        Ok(ProfileService { path, config, active_profile })
    }

    /// Like [`Self::load_or_default`], but a corrupt/unreadable config
    /// file becomes an *empty* config at the same path (so a later save
    /// repairs it) instead of an error. The message is returned so a UI
    /// can tell the user what happened — the app must still start.
    pub fn load_or_recover(path: impl Into<PathBuf>) -> (Self, Option<String>) {
        let path = path.into();
        match Self::load_or_default(&path) {
            Ok(s) => (s, None),
            Err(e) => {
                let message = format!("{e}");
                let service = ProfileService { path, config: Config::default(), active_profile: None };
                (service, Some(message))
            }
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Write the config atomically: write to a temp file in the same
    /// directory, then rename over the target, so a crash mid-write never
    /// leaves a truncated/corrupt config ("Atomic configuration writes").
    pub fn save(&self) -> Result<(), ProfileServiceError> {
        let text = self.config.to_toml_string()?;
        let tmp_path = self.path.with_extension("toml.tmp");
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&tmp_path, text)?;
        fs::rename(&tmp_path, &self.path)?;
        Ok(())
    }

    /// Add or replace a named profile.
    pub fn set_profile(&mut self, name: impl Into<String>, profile: Profile) {
        self.config.profiles.insert(name.into(), profile);
    }

    pub fn remove_profile(&mut self, name: &str) -> Option<Profile> {
        self.config.profiles.remove(name)
    }

    pub fn active_profile_name(&self) -> Option<&str> {
        self.active_profile.as_deref()
    }

    pub fn switch_to(&mut self, name: &str) -> Result<(), ProfileServiceError> {
        if !self.config.profiles.contains_key(name) {
            return Err(ProfileServiceError::NotFound(name.to_string()));
        }
        self.active_profile = Some(name.to_string());
        Ok(())
    }

    /// The fully resolved (inheritance-merged) active profile.
    pub fn active_profile(&self) -> Result<Profile, ProfileServiceError> {
        let name = self.active_profile.as_deref().ok_or_else(|| ProfileServiceError::NotFound("<no active profile set>".into()))?;
        Ok(self.config.resolve(name)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn temp_path(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("ut-profile-test-{}-{}", std::process::id(), name));
        p
    }

    #[test]
    fn load_or_default_handles_missing_file() {
        let path = temp_path("missing.toml");
        let _ = fs::remove_file(&path);
        let svc = ProfileService::load_or_default(&path).unwrap();
        assert!(svc.config().profiles.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let path = temp_path("roundtrip.toml");
        let mut svc = ProfileService::load_or_default(&path).unwrap();
        svc.set_profile("default", Profile { shell: Some("/bin/zsh".into()), ..Default::default() });
        svc.switch_to("default").unwrap();
        svc.save().unwrap();

        let reloaded = ProfileService::load_or_default(&path).unwrap();
        assert_eq!(reloaded.config().profiles["default"].shell.as_deref(), Some("/bin/zsh"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn switch_to_unknown_profile_errors() {
        let path = temp_path("switch.toml");
        let _ = fs::remove_file(&path);
        let mut svc = ProfileService::load_or_default(&path).unwrap();
        let result = svc.switch_to("ghost");
        assert!(matches!(result, Err(ProfileServiceError::NotFound(_))));
    }

    #[test]
    fn active_profile_resolves_inheritance() {
        let path = temp_path("inherit.toml");
        let _ = fs::remove_file(&path);
        let mut svc = ProfileService::load_or_default(&path).unwrap();
        svc.set_profile("base", Profile { shell: Some("/bin/bash".into()), ..Default::default() });
        svc.set_profile("dev", Profile { extends: Some("base".into()), theme: Some("dark".into()), ..Default::default() });
        svc.switch_to("dev").unwrap();
        let resolved = svc.active_profile().unwrap();
        assert_eq!(resolved.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(resolved.theme.as_deref(), Some("dark"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn atomic_save_leaves_no_tmp_file_behind() {
        let path = temp_path("atomic.toml");
        let _ = fs::remove_file(&path);
        let mut svc = ProfileService::load_or_default(&path).unwrap();
        svc.set_profile("x", Profile::default());
        svc.save().unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("toml.tmp").exists());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn remove_profile_drops_it() {
        let path = temp_path("remove.toml");
        let _ = fs::remove_file(&path);
        let mut svc = ProfileService::load_or_default(&path).unwrap();
        svc.set_profile("temp", Profile::default());
        assert!(svc.remove_profile("temp").is_some());
        assert!(!svc.config().profiles.contains_key("temp"));
    }

    #[test]
    fn default_profile_name_read_from_config() {
        let path = temp_path("defaultname.toml");
        let _ = fs::remove_file(&path);
        {
            let mut profiles = HashMap::new();
            profiles.insert("main".to_string(), Profile::default());
            let cfg = Config { default_profile: Some("main".into()), profiles };
            fs::write(&path, cfg.to_toml_string().unwrap()).unwrap();
        }
        let svc = ProfileService::load_or_default(&path).unwrap();
        assert_eq!(svc.active_profile_name(), Some("main"));
        let _ = fs::remove_file(&path);
    }
}
