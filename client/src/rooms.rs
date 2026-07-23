//! Local TOML-backed storage of saved rooms ("lounges").
//!
//! Stores each saved room as the whole pasted `server|room-id|key` join
//! string plus a friendly name, rather than splitting it back into fields —
//! that matches the single-paste-field join UX in `ui.rs` and avoids a
//! second place that needs to agree on the join-string encoding
//! (see `net.rs` module docs for the encoding itself).

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedRoom {
    pub name: String,
    /// The full `server|room-id|key` join string (see `net.rs`).
    pub join_string: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoomStore {
    #[serde(default)]
    pub rooms: Vec<SavedRoom>,
}

impl RoomStore {
    /// `$XDG_CONFIG_HOME/simple-screensharing/rooms.toml` (or the platform
    /// equivalent via the `dirs` crate), falling back to a relative path if
    /// no config directory can be determined.
    pub fn config_path() -> PathBuf {
        match dirs::config_dir() {
            Some(dir) => dir.join("simple-screensharing").join("rooms.toml"),
            None => PathBuf::from("simple-screensharing-rooms.toml"),
        }
    }

    /// Load the saved-rooms file, or return an empty store if it doesn't
    /// exist yet (first run). Parse errors are surfaced, not silently
    /// swallowed, since silently discarding a corrupt file would lose the
    /// user's saved rooms without telling them.
    pub fn load() -> Result<Self> {
        let path = Self::config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read saved rooms from {}", path.display()))?;
        toml::from_str(&contents)
            .with_context(|| format!("failed to parse saved rooms file at {}", path.display()))
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config directory {}", parent.display()))?;
        }
        let contents = toml::to_string_pretty(self).context("failed to serialize saved rooms")?;
        fs::write(&path, contents)
            .with_context(|| format!("failed to write saved rooms to {}", path.display()))
    }

    /// Insert a new saved room, or overwrite the existing one with the same
    /// name.
    pub fn upsert(&mut self, name: String, join_string: String) {
        if let Some(existing) = self.rooms.iter_mut().find(|r| r.name == name) {
            existing.join_string = join_string;
        } else {
            self.rooms.push(SavedRoom { name, join_string });
        }
    }

    pub fn remove(&mut self, name: &str) {
        self.rooms.retain(|r| r.name != name);
    }
}

/// Minimal app-wide settings, currently just the UI language. Kept in its
/// own small TOML file alongside `rooms.toml` rather than folded into
/// `RoomStore` — a language preference isn't a saved room, and this keeps
/// each file's schema focused.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "AppSettings::default_language")]
    pub language: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self { language: Self::default_language() }
    }
}

impl AppSettings {
    fn default_language() -> String {
        "en".to_string()
    }

    /// `$XDG_CONFIG_HOME/simple-screensharing/settings.toml` (or the
    /// platform equivalent), mirroring `RoomStore::config_path`.
    pub fn config_path() -> PathBuf {
        match dirs::config_dir() {
            Some(dir) => dir.join("simple-screensharing").join("settings.toml"),
            None => PathBuf::from("simple-screensharing-settings.toml"),
        }
    }

    /// Load the settings file, or return defaults if it doesn't exist yet or
    /// fails to parse — a missing/corrupt language preference shouldn't
    /// block the app from starting, unlike saved rooms.
    pub fn load() -> Self {
        let path = Self::config_path();
        let Ok(contents) = fs::read_to_string(&path) else {
            return Self::default();
        };
        toml::from_str(&contents).unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create config directory {}", parent.display()))?;
        }
        let contents = toml::to_string_pretty(self).context("failed to serialize app settings")?;
        fs::write(&path, contents).with_context(|| format!("failed to write app settings to {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_adds_then_updates_in_place() {
        let mut store = RoomStore::default();
        store.upsert("friends".to_string(), "a|b|c".to_string());
        assert_eq!(store.rooms.len(), 1);
        store.upsert("friends".to_string(), "x|y|z".to_string());
        assert_eq!(store.rooms.len(), 1);
        assert_eq!(store.rooms[0].join_string, "x|y|z");
    }

    #[test]
    fn toml_round_trip() {
        let mut store = RoomStore::default();
        store.upsert("friends".to_string(), "server:4433|00112233445566778899aabbccddeeff|key".to_string());
        let toml_str = toml::to_string_pretty(&store).unwrap();
        let parsed: RoomStore = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.rooms, store.rooms);
    }

    #[test]
    fn app_settings_defaults_to_english() {
        assert_eq!(AppSettings::default().language, "en");
    }

    #[test]
    fn app_settings_toml_round_trip() {
        let settings = AppSettings { language: "de".to_string() };
        let toml_str = toml::to_string_pretty(&settings).unwrap();
        let parsed: AppSettings = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.language, "de");
    }
}
