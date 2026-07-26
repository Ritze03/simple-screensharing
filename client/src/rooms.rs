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

    /// The saved name for a join string, if this room is saved.
    ///
    /// `Why:` the sidebar's ROOMS section renders `room_store ⟕ rooms` — every
    /// saved room shows, and the ones with a live session get connected
    /// styling — so it needs to join the two collections on something both
    /// sides have. The join string is that key: it's the only thing a
    /// `SavedRoom` and a live session provably agree on (a `RoomHandle` is
    /// per-session and a saved room has none; the relay-assigned `ConnId` is
    /// per-connection and a *saved* room has none either). Name is not usable
    /// as the key in the other direction — the same room can be saved under
    /// any name the user likes — so the lookup only ever goes join-string →
    /// name, never the reverse.
    pub fn name_for(&self, join_string: &str) -> Option<&str> {
        self.rooms.iter().find(|r| r.join_string == join_string).map(|r| r.name.as_str())
    }
}

/// Which `wgpu::PresentMode` the eframe/wgpu render surface is configured
/// with. See `superdoc/features/presentation-modes.md` for the full
/// rationale, including the runtime-vs-restart constraint this settings
/// value has to work around.
///
/// Its own small enum rather than storing `wgpu::PresentMode` directly:
/// keeps this module's persisted TOML schema stable even across a future
/// `wgpu` upgrade that renames/reorders its variants, and avoids needing
/// `wgpu`'s `serde` feature for three values.
///
/// Limited to the three modes documented as available everywhere
/// (`AutoVsync`/`Immediate`/`Mailbox`) rather than exposing every
/// `wgpu::PresentMode` variant — `Immediate`/`Mailbox` already fall back to
/// `Fifo` at surface-configure time on a backend that doesn't support them
/// (see `to_wgpu`), so there's no reason to also offer `Fifo`/`FifoRelaxed`/
/// `AutoNoVsync` as separate user-facing choices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PresentationMode {
    /// Vsync-locked to the display refresh rate, no tearing — today's
    /// implicit behavior (egui-wgpu's own default), kept as the default here
    /// too so a user who never touches the setting sees no change.
    #[default]
    AutoVsync,
    /// No vsync ceiling: submits frames as soon as they're ready, may tear.
    Immediate,
    /// No vsync ceiling, no tearing: swaps into a single-slot buffer instead
    /// of waiting for the next vblank rather than queuing several frames.
    Mailbox,
}

impl PresentationMode {
    pub const ALL: [PresentationMode; 3] = [Self::AutoVsync, Self::Immediate, Self::Mailbox];

    /// Human-readable label for the presentation-mode dropdown.
    pub fn label(self) -> &'static str {
        match self {
            Self::AutoVsync => "Vsync (default)",
            Self::Immediate => "Immediate (no vsync)",
            Self::Mailbox => "Mailbox (no vsync, no tearing)",
        }
    }

    /// The actual `wgpu::PresentMode` to configure the render surface with.
    /// `Immediate`/`Mailbox` aren't guaranteed on every backend, but `wgpu`
    /// itself falls back to `Fifo` at `Surface::configure()` time for a mode
    /// missing from `SurfaceCapabilities::present_modes`, so no fallback
    /// logic is needed on this side.
    pub fn to_wgpu(self) -> wgpu::PresentMode {
        match self {
            Self::AutoVsync => wgpu::PresentMode::AutoVsync,
            Self::Immediate => wgpu::PresentMode::Immediate,
            Self::Mailbox => wgpu::PresentMode::Mailbox,
        }
    }
}

/// How the Stream Control tab decides each outgoing destination's bitrate.
/// The two modes the plan doc names (`superdoc/brainstorm/
/// ui-redesign-multiroom-streaming.md`, round-2 Q6).
///
/// `Why` these are two modes rather than one number with a checkbox: they
/// answer different questions. "Per-Stream Maximum" answers *how good should
/// this one stream be* (the user sets each destination's rate by hand, and
/// starting another stream doesn't change any existing one). "Absolute
/// Maximum" answers *how much of my uplink may this app ever use in total*
/// — one per-client aggregate across every destination in every room, so
/// starting another stream anywhere shrinks every other destination's share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum BandwidthMode {
    /// The user sets a bitrate per destination; nothing is shared or
    /// re-divided. Default: it is the behaviour that surprises nobody, and
    /// it's what a user who never opens the bandwidth card gets.
    #[default]
    PerStream,
    /// The user sets one total; `bandwidth::allocate` divides it across every
    /// active destination by `width*height*fps` weight.
    Absolute,
}

/// Minimal app-wide settings: UI language, the render presentation mode, and
/// the Stream Control bandwidth preferences. Kept in its own small TOML file
/// alongside `rooms.toml` rather than folded into `RoomStore` — none of it is
/// a saved room, and this keeps each file's schema focused.
///
/// Preferences only, never session state: no active stream, watch, room
/// connection or per-destination manual bitrate is persisted (plan doc,
/// round-1 Q9 — every launch starts fresh).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "AppSettings::default_language")]
    pub language: String,
    /// Read once at startup and passed into `NativeOptions.wgpu_options` in
    /// `main.rs` — changing it takes effect on the next launch, not live.
    /// See `superdoc/features/presentation-modes.md`.
    #[serde(default)]
    pub presentation_mode: PresentationMode,
    /// Which bandwidth mode the Stream Control tab is in.
    #[serde(default)]
    pub bandwidth_mode: BandwidthMode,
    /// Absolute-Maximum mode's single per-client total, in kbit/s.
    ///
    /// `Why` this is stored independently of any per-destination rate, and
    /// why both survive a mode switch: the two modes' numbers mean different
    /// things (a total vs. one stream's rate), so folding them into one field
    /// would silently destroy whichever the user isn't currently looking at.
    /// Toggling modes back and forth must be lossless.
    #[serde(default = "AppSettings::default_absolute_total_kbps")]
    pub absolute_total_kbps: u32,
    /// Absolute-Maximum mode's guaranteed floor *per destination*, in kbit/s
    /// (plan doc, round-2 Q7: "full guaranteed floor per destination").
    /// Feeds `bandwidth::DestSpec::floor_kbps`; when the floors don't all fit
    /// the total, `allocate` overshoots and reports it rather than starving
    /// anyone (round-1 Q8).
    #[serde(default = "AppSettings::default_destination_floor_kbps")]
    pub destination_floor_kbps: u32,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            language: Self::default_language(),
            presentation_mode: PresentationMode::default(),
            bandwidth_mode: BandwidthMode::default(),
            absolute_total_kbps: Self::default_absolute_total_kbps(),
            destination_floor_kbps: Self::default_destination_floor_kbps(),
        }
    }
}

impl AppSettings {
    fn default_language() -> String {
        "en".to_string()
    }

    /// 20 Mbit/s: comfortably above a single good 1080p stream's ~8 Mbit/s
    /// default (so switching to Absolute mode with one stream running doesn't
    /// immediately throttle it) and comfortably below the project's 40 Mbit/s
    /// per-stream ceiling, so the mode has visible room to divide.
    fn default_absolute_total_kbps() -> u32 {
        20_000
    }

    /// 500 kbit/s: low enough that a handful of destinations can all be
    /// guaranteed their floor out of a modest total without overshooting, high
    /// enough that a destination clamped to it is still watchable rather than
    /// a slideshow.
    fn default_destination_floor_kbps() -> u32 {
        500
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

    /// The sidebar joins saved rooms to live sessions on the join string, so
    /// a hit must be exact and a miss must stay a miss (never fall back to a
    /// name match — see `name_for`'s docs).
    #[test]
    fn name_for_looks_up_by_join_string_only() {
        let mut store = RoomStore::default();
        store.upsert("friends".to_string(), "a|b|c".to_string());
        assert_eq!(store.name_for("a|b|c"), Some("friends"));
        assert_eq!(store.name_for("x|y|z"), None);
        assert_eq!(store.name_for("friends"), None);
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

    /// Load-bearing for "no change for users who never touch the setting":
    /// the default presentation mode must match egui-wgpu's own default.
    #[test]
    fn app_settings_defaults_to_vsync() {
        assert_eq!(AppSettings::default().presentation_mode, PresentationMode::AutoVsync);
    }

    #[test]
    fn app_settings_toml_round_trip() {
        let settings = AppSettings {
            language: "de".to_string(),
            presentation_mode: PresentationMode::Mailbox,
            bandwidth_mode: BandwidthMode::Absolute,
            absolute_total_kbps: 12_345,
            destination_floor_kbps: 750,
        };
        let toml_str = toml::to_string_pretty(&settings).unwrap();
        let parsed: AppSettings = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.language, "de");
        assert_eq!(parsed.presentation_mode, PresentationMode::Mailbox);
        assert_eq!(parsed.bandwidth_mode, BandwidthMode::Absolute);
        assert_eq!(parsed.absolute_total_kbps, 12_345);
        assert_eq!(parsed.destination_floor_kbps, 750);
    }

    /// Both modes' numbers are independent fields, so a settings file written
    /// while in one mode still carries the other's value — switching modes can
    /// never destroy what the other mode was configured with.
    #[test]
    fn both_bandwidth_modes_values_survive_a_round_trip_together() {
        let settings = AppSettings {
            bandwidth_mode: BandwidthMode::PerStream,
            absolute_total_kbps: 33_000,
            ..AppSettings::default()
        };
        let parsed: AppSettings = toml::from_str(&toml::to_string_pretty(&settings).unwrap()).unwrap();
        assert_eq!(parsed.bandwidth_mode, BandwidthMode::PerStream);
        assert_eq!(parsed.absolute_total_kbps, 33_000, "the Absolute total must survive being saved in Per-Stream mode");
    }

    /// A settings file written before the bandwidth fields existed must still
    /// parse, defaulting them rather than failing to load.
    #[test]
    fn app_settings_missing_bandwidth_fields_default() {
        let parsed: AppSettings = toml::from_str("language = \"en\"\n").unwrap();
        assert_eq!(parsed.bandwidth_mode, BandwidthMode::PerStream);
        assert_eq!(parsed.absolute_total_kbps, AppSettings::default_absolute_total_kbps());
        assert_eq!(parsed.destination_floor_kbps, AppSettings::default_destination_floor_kbps());
    }

    /// A settings file written before this field existed (just `language =
    /// "..."`) must still parse, defaulting the new field rather than
    /// failing to load — the same missing-field tolerance `#[serde(default)]`
    /// already gives `language` itself.
    #[test]
    fn app_settings_missing_presentation_mode_defaults() {
        let parsed: AppSettings = toml::from_str("language = \"de\"\n").unwrap();
        assert_eq!(parsed.presentation_mode, PresentationMode::AutoVsync);
    }

    #[test]
    fn presentation_mode_to_wgpu_maps_expected_variants() {
        assert_eq!(PresentationMode::AutoVsync.to_wgpu(), wgpu::PresentMode::AutoVsync);
        assert_eq!(PresentationMode::Immediate.to_wgpu(), wgpu::PresentMode::Immediate);
        assert_eq!(PresentationMode::Mailbox.to_wgpu(), wgpu::PresentMode::Mailbox);
    }
}
