//! Zohara Terminal's own settings screen (the data half).
//!
//! `ut-config`/`ut-profile` existed from the start but nothing in the app
//! ever read or wrote a settings file — the terminal had no settings area
//! at all. This module is the state behind the GUI's settings window: it
//! loads the config from disk, maps the screen's controls onto a
//! [`Profile`], and saves it back. The *drawing* half lives in `main.rs`
//! (it needs the egui context); keeping the logic here makes it unit-
//! testable without a window.

use std::path::PathBuf;
use ut_config::{BellStyle, Profile};
use ut_profile::ProfileService;
use ut_safety::Risk;

/// The config file this app reads and writes. Nothing else in the
/// codebase had picked a location, so this is the convention now:
/// `$XDG_CONFIG_HOME/zohara/terminal/config.toml` (or `~/.config/...`).
pub fn config_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .ok()
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from(".config"));
    base.join("zohara").join("terminal").join("config.toml")
}

/// The one profile this screen edits. The config schema supports named
/// profiles with inheritance; the GUI edits the plain "default" profile.
pub const PROFILE_NAME: &str = "default";

pub const CURSOR_SHAPES: [&str; 3] = ["block", "underline", "bar"];

/// Labels for the sensitivity combo, matching `Risk` order. The default
/// (medium) confirms common footguns; critical only holds back the
/// disk-destroyers.
pub const SENSITIVITY_LABELS: [&str; 4] = [
    "Low — confirm almost everything risky",
    "Medium — common footguns (default)",
    "High — anything that can destroy data",
    "Critical — only disk-destroyers",
];

pub fn sensitivity_risk(index: usize) -> Risk {
    match index {
        0 => Risk::Low,
        2 => Risk::High,
        3 => Risk::Critical,
        _ => Risk::Medium,
    }
}

pub fn sensitivity_index(risk: Risk) -> usize {
    match risk {
        Risk::Low => 0,
        Risk::High => 2,
        Risk::Critical => 3,
        Risk::Medium => 1,
    }
}

pub fn bell_style_index(style: BellStyle) -> usize {
    match style {
        BellStyle::None => 0,
        BellStyle::Visual => 1,
        BellStyle::Audible => 2,
        BellStyle::Both => 3,
    }
}

pub fn bell_style_from_index(index: usize) -> BellStyle {
    match index {
        1 => BellStyle::Visual,
        2 => BellStyle::Audible,
        3 => BellStyle::Both,
        _ => BellStyle::None,
    }
}

/// The settings screen's state: the loaded config service plus the
/// control values the window binds to. `dirty` is set by the UI when
/// anything changes; Save writes through [`Self::save`].
pub struct SettingsState {
    pub service: ProfileService,
    pub font_size: f32,
    pub scrollback_lines: u32,
    pub cursor_shape: usize,
    pub bell_style: usize,
    pub sensitivity: usize,
    pub dirty: bool,
    /// Last save outcome, shown next to the Save button.
    pub status: Option<String>,
}

impl SettingsState {
    /// Load from this machine's default config path (missing file → the
    /// schema defaults; a corrupt file is surfaced as an error status, not
    /// a crash — the profile service's own "safe startup" rule).
    pub fn load() -> Self {
        Self::load_from(config_path())
    }

    pub fn load_from(path: PathBuf) -> Self {
        let (service, corrupt) = ProfileService::load_or_recover(&path);
        let mut state = Self {
            font_size: 15.0,
            scrollback_lines: 10_000,
            cursor_shape: 0,
            bell_style: 1, // visual
            sensitivity: 1, // medium
            dirty: false,
            status: corrupt,
            service,
        };
        state.read_profile();
        state
    }

    /// Pull the current control values out of the "default" profile
    /// (falling back to the schema defaults for anything unset).
    fn read_profile(&mut self) {
        let profile = self.service.config().profiles.get(PROFILE_NAME).cloned().unwrap_or_default();
        self.font_size = profile.font_size.unwrap_or(15.0);
        self.scrollback_lines = profile.scrollback_lines.unwrap_or(10_000);
        self.cursor_shape = CURSOR_SHAPES.iter().position(|s| Some(*s) == profile.cursor_shape.as_deref()).unwrap_or(0);
        self.bell_style = profile.bell_style.map(bell_style_index).unwrap_or(1);
        self.sensitivity = profile
            .warning_sensitivity
            .as_deref()
            .map(|s| match s {
                "low" => 0,
                "high" => 2,
                "critical" => 3,
                _ => 1,
            })
            .unwrap_or(1);
    }

    /// The screen's current values as a profile (the "default" one keeps
    /// any fields this screen doesn't edit).
    pub fn to_profile(&self) -> Profile {
        let mut profile = self.service.config().profiles.get(PROFILE_NAME).cloned().unwrap_or_default();
        profile.font_size = Some(self.font_size);
        profile.scrollback_lines = Some(self.scrollback_lines);
        profile.cursor_shape = Some(CURSOR_SHAPES[self.cursor_shape].to_string());
        profile.bell_style = Some(bell_style_from_index(self.bell_style));
        profile.warning_sensitivity = Some(
            match self.sensitivity {
                0 => "low",
                2 => "high",
                3 => "critical",
                _ => "medium",
            }
            .to_string(),
        );
        profile
    }

    /// Write the current values into the "default" profile and save the
    /// config to disk. The status message is shown by the UI either way.
    pub fn save(&mut self) {
        self.service.set_profile(PROFILE_NAME, self.to_profile());
        self.status = Some(match self.service.save() {
            Ok(()) => format!("Saved to {}", self.service.path().display()),
            Err(e) => format!("Could not save: {e}"),
        });
        self.dirty = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ut-gui-settings-{tag}-{}.toml", std::process::id()))
    }

    #[test]
    fn missing_file_yields_defaults() {
        let s = SettingsState::load_from(temp_path("defaults"));
        assert_eq!(s.font_size, 15.0);
        assert_eq!(s.sensitivity, 1, "medium is the default sensitivity");
        assert_eq!(s.cursor_shape, 0, "block cursor by default");
    }

    #[test]
    fn values_round_trip_through_the_config_file() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);
        {
            let mut s = SettingsState::load_from(path.clone());
            s.font_size = 21.0;
            s.scrollback_lines = 50_000;
            s.cursor_shape = 2; // bar
            s.bell_style = 2; // audible
            s.sensitivity = 3; // critical
            s.save();
        }
        let s = SettingsState::load_from(path.clone());
        assert_eq!(s.font_size, 21.0);
        assert_eq!(s.scrollback_lines, 50_000);
        assert_eq!(s.cursor_shape, 2);
        assert_eq!(s.bell_style, 2);
        assert_eq!(s.sensitivity, 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn sensitivity_maps_to_risk_both_ways() {
        for (i, risk) in [(0, Risk::Low), (1, Risk::Medium), (2, Risk::High), (3, Risk::Critical)] {
            assert_eq!(sensitivity_risk(i), risk);
            assert_eq!(sensitivity_index(risk), i);
        }
    }

    #[test]
    fn corrupt_config_is_reported_not_fatal() {
        let path = temp_path("corrupt");
        std::fs::write(&path, "not [valid toml ===").unwrap();
        let s = SettingsState::load_from(path.clone());
        assert!(s.status.as_deref().unwrap_or_default().contains("could not"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn profile_keeps_fields_this_screen_does_not_edit() {
        let path = temp_path("preserve");
        let _ = std::fs::remove_file(&path);
        // Seed a config with a field the settings screen doesn't touch.
        std::fs::write(&path, "[profiles.default]\nshell = \"/bin/zsh\"\nfont_size = 12.0\n").unwrap();
        let mut s = SettingsState::load_from(path.clone());
        assert_eq!(s.font_size, 12.0, "existing values are read, not reset");
        s.font_size = 18.0;
        s.save();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("/bin/zsh"), "shell survived the save: {text}");
        assert!(text.contains("font_size = 18.0"));
        let _ = std::fs::remove_file(&path);
    }
}
