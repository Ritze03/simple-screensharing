//! Local TOML-backed storage of saved rooms ("lounges").
//!
//! Stores each saved room as the whole pasted `server|room-id|key` join
//! string plus a friendly name, rather than splitting it back into fields —
//! that matches the single-paste-field join UX (`ui/modals.rs`) and avoids a
//! second place that needs to agree on the join-string encoding
//! (see `net.rs` module docs for the encoding itself).
//!
//! # Identity: one saved room per room, not per name
//!
//! A saved room's identity is [`room_identity`] of its join string: server +
//! room id + key, *without* the optional 4th cert-hash field. Every lookup in
//! [`RoomStore`] (`upsert`, `get`, `name_for`, `rename`, `remove`,
//! `set_auto_connect`) goes through it, so any spelling of the same room
//! reaches the same single entry.
//!
//! `Why` not the name: names are local nicknames the user can pick freely, so
//! keying by name let "MovieNight" and "Films" both point at one room (two
//! entries, both shown connected, one leave dropping both, forget removing only
//! one). `Why` not the whole string: the relay's cert hash rotates, and a
//! rotated cert must update the existing entry, not create a second one.
//!
//! # The UI's `RoomKey`
//!
//! The UI (`ui/mod.rs`) identifies a saved room by `RoomKey = SavedRoom::
//! join_string` and also *connects* with that key, so the key has to stay a
//! full, connectable join string (an identity string has no cert hash and
//! would connect unpinned). Guarantee: an `upsert` that only changes the
//! *name*, or re-saves the identical string, never changes the entry's
//! `join_string`, so its `RoomKey` is stable. The stored string is replaced
//! only by a *different spelling* of the same room (typically a fresh cert
//! hash), because the newest link is the one that connects; that is the only
//! event that changes a `RoomKey`. Use [`RoomStore::same_room`] to compare a
//! live session's join string against a key across such a change.

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::capture::{QualityLevel, RateControlMode, TuneMode, VideoCodec};
use crate::net::{format_join_string, parse_join_string};

/// The identity of the room a join string points at: `server|room-id|key`
/// with the optional cert-hash field dropped and the room id / key re-encoded
/// canonically. Two join strings name the same room iff their identities are
/// equal.
///
/// A string that doesn't parse (hand-edited or legacy `rooms.toml` entry) falls
/// back to its first three `|`-separated fields, trimmed, so it still has a
/// stable identity and can be forgotten/renamed instead of being stuck.
pub fn room_identity(join_string: &str) -> String {
    let s = join_string.trim();
    match parse_join_string(s) {
        Ok(p) => format_join_string(&p.server, &p.room_id, &p.room_key),
        Err(_) => s.splitn(4, '|').take(3).collect::<Vec<_>>().join("|"),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedRoom {
    pub name: String,
    /// The full `server|room-id|key` join string (see `net.rs`).
    pub join_string: String,
    /// When `true`, this room is joined automatically at app startup (see
    /// `main.rs`'s `.setup()`) — a deliberate, per-room, user-set preference,
    /// independent of whether the room happened to be connected when the app
    /// last closed. `#[serde(default)]` so existing `rooms.toml` files
    /// written before this field existed still parse (they all mean "not
    /// auto-connected" under the old behavior).
    #[serde(default)]
    pub auto_connect: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoomStore {
    #[serde(default)]
    pub rooms: Vec<SavedRoom>,
}

/// The directory the app's config files live under: `$SSS_CONFIG_HOME` when
/// set, else the platform's config dir (`dirs::config_dir()`).
///
/// Why an own variable: `dirs` honours `XDG_CONFIG_HOME` on Linux but asks the
/// shell for the Roaming AppData folder on Windows, where no environment
/// variable steers it. Scratch runs and the unit tests need to redirect the
/// config (they must never touch a real user's saved rooms), so they set this.
pub fn config_base_dir() -> Option<PathBuf> {
    match std::env::var_os("SSS_CONFIG_HOME") {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => dirs::config_dir(),
    }
}

impl RoomStore {
    /// `$XDG_CONFIG_HOME/simple-screensharing/rooms.toml` (or the platform
    /// equivalent via the `dirs` crate), falling back to a relative path if
    /// no config directory can be determined.
    pub fn config_path() -> PathBuf {
        match config_base_dir() {
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
        let mut store: Self = toml::from_str(&contents)
            .with_context(|| format!("failed to parse saved rooms file at {}", path.display()))?;
        // Migration: a file written while the store was keyed by name may hold
        // several entries for one room. Merge them and write the cleaned file
        // back right away (best effort: a failed write-back is not an error,
        // the next normal `save` rewrites the file anyway).
        if store.merge_duplicates() {
            let _ = store.save();
        }
        Ok(store)
    }

    /// Merge entries that share an identity ([`room_identity`]) into one.
    /// Returns whether anything was merged.
    ///
    /// Rule, per group of duplicates: the merged entry keeps the **first**
    /// entry's position (stable sidebar order); its **name** is the *last*
    /// non-blank name in the group; its **join string** is the last entry's
    /// (newest form, e.g. freshest cert hash); `auto_connect` is the OR of all.
    ///
    /// `Why` last-wins: the file has no timestamps, so file order is the only
    /// recency signal. The old name-keyed store appended an entry on every new
    /// name (a join under a new name, and a rename = remove + upsert), so a
    /// later entry is the more recently typed or renamed one, which is exactly
    /// the "typed name wins" rule `upsert` applies from now on.
    pub fn merge_duplicates(&mut self) -> bool {
        let mut merged: Vec<(String, SavedRoom)> = Vec::with_capacity(self.rooms.len());
        let mut changed = false;
        for room in std::mem::take(&mut self.rooms) {
            let id = room_identity(&room.join_string);
            if let Some((_, kept)) = merged.iter_mut().find(|(k, _)| *k == id) {
                if !room.name.trim().is_empty() {
                    kept.name = room.name;
                }
                kept.join_string = room.join_string;
                kept.auto_connect |= room.auto_connect;
                changed = true;
            } else {
                merged.push((id, room));
            }
        }
        self.rooms = merged.into_iter().map(|(_, r)| r).collect();
        changed
    }

    fn position(&self, join_string: &str) -> Option<usize> {
        let id = room_identity(join_string);
        self.rooms.iter().position(|r| room_identity(&r.join_string) == id)
    }

    /// Whether two join strings name the same room (equal [`room_identity`]).
    pub fn same_room(a: &str, b: &str) -> bool {
        room_identity(a) == room_identity(b)
    }

    /// The saved entry for the room `join_string` names, whichever spelling
    /// (with or without cert hash) it is given in.
    pub fn get(&self, join_string: &str) -> Option<&SavedRoom> {
        self.position(join_string).map(|i| &self.rooms[i])
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

    /// Save a room, keyed by identity. If the room is already saved: its name
    /// becomes `name` when that is non-blank (the typed name wins; a blank
    /// name keeps the old one) and its stored join string becomes
    /// `join_string` (the newest form, e.g. with a fresh cert hash);
    /// `auto_connect` is untouched. Otherwise a new entry is appended.
    pub fn upsert(&mut self, name: String, join_string: String) {
        let name = name.trim().to_string();
        let join_string = join_string.trim().to_string();
        match self.position(&join_string) {
            Some(i) => {
                let existing = &mut self.rooms[i];
                if !name.is_empty() {
                    existing.name = name;
                }
                existing.join_string = join_string;
            }
            None => self.rooms.push(SavedRoom { name, join_string, auto_connect: false }),
        }
    }

    /// Set the name of the one saved entry for this room. No-op if the room
    /// isn't saved or `name` is blank.
    pub fn rename(&mut self, join_string: &str, name: &str) {
        let name = name.trim();
        if !name.is_empty()
            && let Some(i) = self.position(join_string)
        {
            self.rooms[i].name = name.to_string();
        }
    }

    /// Remove the one saved entry for the room `join_string` names (any
    /// spelling). No-op if it isn't saved.
    pub fn remove(&mut self, join_string: &str) {
        if let Some(i) = self.position(join_string) {
            self.rooms.remove(i);
        }
    }

    /// Flip the `auto_connect` flag on the one saved entry for this room.
    /// No-op (not an error) if the room isn't saved — mirrors
    /// `name_for`'s "no entry" case rather than surfacing a lookup failure,
    /// since the frontend only ever offers this checkbox on a row it already
    /// knows is saved.
    pub fn set_auto_connect(&mut self, join_string: &str, value: bool) {
        if let Some(i) = self.position(join_string) {
            self.rooms[i].auto_connect = value;
        }
    }

    /// The saved name for a join string, if this room is saved under a
    /// non-blank name.
    ///
    /// `Why:` the sidebar's ROOMS section renders `room_store ⟕ rooms` — every
    /// saved room shows, and the ones with a live session get connected
    /// styling — so it needs to join the two collections on something both
    /// sides have. The join string (compared by [`room_identity`], so a
    /// rotated cert hash still matches) is that key: it's the only thing a
    /// `SavedRoom` and a live session provably agree on (a `RoomHandle` is
    /// per-session and a saved room has none; the relay-assigned `ConnId` is
    /// per-connection and a *saved* room has none either). Name is not usable
    /// as the key in the other direction — the same room can be saved under
    /// any name the user likes — so the lookup only ever goes join-string →
    /// name, never the reverse.
    pub fn name_for(&self, join_string: &str) -> Option<&str> {
        self.get(join_string).map(|r| r.name.as_str()).filter(|n| !n.is_empty())
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
/// Limited to three user-facing choices (`AutoVsync`/`Immediate`/`Mailbox`)
/// rather than exposing every `wgpu::PresentMode` variant. Only `AutoVsync`
/// falls back on its own: wgpu 27 rejects an explicit `Immediate`/`Mailbox`
/// that the surface doesn't list (`UnsupportedPresentMode`, fatal at surface
/// configure), so the binary checks the surface's capabilities before eframe
/// configures it and relaunches with `AutoVsync` when the mode is missing
/// (see `main.rs`'s `wgpu_configuration`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PresentationMode {
    /// Vsync-locked to the display refresh rate (Fifo), no tearing —
    /// egui-wgpu's own default. Was this app's default until 2026-10-14.
    AutoVsync,
    /// No vsync ceiling: submits frames as soon as they're ready, may tear.
    Immediate,
    /// No vsync ceiling, no tearing: swaps into a single-slot buffer instead
    /// of waiting for the next vblank rather than queuing several frames.
    ///
    /// The default. `Why:` eframe presents its viewports serially on one
    /// thread, and with Fifo every present blocks until that window's own
    /// vblank. With a pop-out window open and a stream faster than the
    /// monitor refresh, the windows starve each other: measured on a 120 Hz
    /// monitor with a 240 fps stream, the main window fell to 69 fps. Mailbox
    /// never blocks on vblank and held 240 fps in both windows, and unlike
    /// Immediate it does not tear. Still applied only at launch (restart to
    /// apply, see `superdoc/features/presentation-modes.md`). Only affects
    /// settings files without a stored mode: every save writes the whole
    /// `AppSettings`, so an existing user's stored `AutoVsync` is kept.
    #[default]
    Mailbox,
}

impl PresentationMode {
    pub const ALL: [PresentationMode; 3] = [Self::AutoVsync, Self::Immediate, Self::Mailbox];

    /// Human-readable label for the presentation-mode dropdown.
    pub fn label(self) -> &'static str {
        match self {
            Self::AutoVsync => "Vsync",
            Self::Immediate => "Immediate (no vsync)",
            Self::Mailbox => "Mailbox (default, no vsync, no tearing)",
        }
    }

    /// The actual `wgpu::PresentMode` to configure the render surface with.
    /// `Immediate`/`Mailbox` aren't guaranteed on every backend and wgpu does
    /// **not** fall back for them (only the `Auto*` modes fall back), so the
    /// caller must check the surface's `present_modes` first — `main.rs`
    /// does, and relaunches with `AutoVsync` when the mode is missing.
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
    /// Ceiling on the View tab's jitter buffer, in milliseconds (plan doc:
    /// user-configurable instead of the buffer's earlier hardcoded 2000ms).
    /// The buffer itself still grows only as far as live jitter demands —
    /// this just caps how far it's allowed to grow.
    #[serde(default = "AppSettings::default_max_jitter_buffer_ms")]
    pub max_jitter_buffer_ms: u32,
    /// Ceiling, as a percent of wall-clock time, on how much faster than
    /// live the View tab's jitter buffer is allowed to play back queued
    /// video while shrinking back down after a jitter spike
    /// (`tile-video.js`'s `updateAppliedBuffer`) — e.g. 5 means "at most 5%
    /// faster than real time" while catching up. Project owner, verbatim:
    /// "It should run at max 5%-10% faster... to basically catch up to live
    /// again." The dynamic shrink itself is always on; this only bounds how
    /// aggressively it's allowed to speed up.
    #[serde(default = "AppSettings::default_catch_up_speed_percent")]
    pub catch_up_speed_percent: u32,
    /// The "START A STREAM" form's last-used values (`stream-control.js`'s
    /// `renderStartStreamCard`) — framerate, bitrate, and the three
    /// testing-oriented encoder tuning knobs, saved on every change so the
    /// form pre-fills with what was last used instead of resetting to
    /// hardcoded defaults on each launch (see `superdoc/features/
    /// stream-control.md`'s 2026-07-28 note).
    ///
    /// `Why its own nested struct rather than five flat fields:` every field
    /// here is set together by `session::save_start_stream_defaults`
    /// whenever the form changes, unlike the
    /// bandwidth/jitter fields above which are each their own independent
    /// setting with its own save call — grouping them mirrors that they are
    /// one cohesive "form state", the same way `edit_destination` already
    /// takes the destination edit panel's fields as one bundle rather than
    /// five separate commands.
    #[serde(default)]
    pub start_stream_defaults: StartStreamDefaults,
    /// The global display name shown to everyone in every room this client
    /// joins or creates (Settings tab). Replaces what used to be a per-join
    /// "Display name" field on the Join/Create dialogs — see
    /// `superdoc/features/stream-control.md`'s 2026-07-28 note on why: the
    /// wire protocol has no concept of changing a display name after joining
    /// a given room anyway (`net.rs`/`protocol.rs` send it once, at connect),
    /// so "one name, set once" is just this client choosing not to re-ask
    /// for something it would use identically every time regardless.
    ///
    /// Empty string means "not set yet" — `AppState::default()` falls back to
    /// `whoami_fallback()` in that case, same as before this setting existed.
    #[serde(default)]
    pub display_name: String,
}

/// See [`AppSettings::start_stream_defaults`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StartStreamDefaults {
    #[serde(default = "StartStreamDefaults::default_framerate")]
    pub framerate: u32,
    #[serde(default = "StartStreamDefaults::default_bitrate_kbps")]
    pub bitrate_kbps: u32,
    #[serde(default)]
    pub tune: TuneMode,
    #[serde(default)]
    pub quality_level: QualityLevel,
    #[serde(default)]
    pub rate_control: RateControlMode,
    /// `#[serde(default)]` (-> `VideoCodec::H264`) is load-bearing here:
    /// every existing user's on-disk `settings.toml` predates this field, so
    /// it must deserialize to exactly the behavior they already had rather
    /// than fail to load — see `VideoCodec`'s own `#[default]` doc comment.
    #[serde(default)]
    pub codec: VideoCodec,
}

impl StartStreamDefaults {
    fn default_framerate() -> u32 {
        crate::session::DEFAULT_FRAMERATE
    }

    fn default_bitrate_kbps() -> u32 {
        crate::session::DEFAULT_BITRATE_KBPS
    }
}

impl Default for StartStreamDefaults {
    fn default() -> Self {
        Self {
            framerate: Self::default_framerate(),
            bitrate_kbps: Self::default_bitrate_kbps(),
            tune: TuneMode::default(),
            quality_level: QualityLevel::default(),
            rate_control: RateControlMode::default(),
            codec: VideoCodec::default(),
        }
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            language: Self::default_language(),
            presentation_mode: PresentationMode::default(),
            bandwidth_mode: BandwidthMode::default(),
            absolute_total_kbps: Self::default_absolute_total_kbps(),
            destination_floor_kbps: Self::default_destination_floor_kbps(),
            max_jitter_buffer_ms: Self::default_max_jitter_buffer_ms(),
            catch_up_speed_percent: Self::default_catch_up_speed_percent(),
            start_stream_defaults: StartStreamDefaults::default(),
            display_name: String::new(),
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

    /// 2000ms: the jitter buffer's original hardcoded ceiling, kept as the
    /// default so this setting's introduction is a no-op for anyone who
    /// never touches it.
    fn default_max_jitter_buffer_ms() -> u32 {
        2000
    }

    /// Clamp range for `max_jitter_buffer_ms`: 0 (buffer effectively
    /// disabled) to 5000ms (well above any jitter this app is expected to
    /// smooth; a higher ceiling just adds playout latency for no benefit).
    pub fn clamp_max_jitter_buffer_ms(ms: u32) -> u32 {
        ms.clamp(0, 5000)
    }

    /// 5%: the conservative end of the owner's requested 5-10% range, kept
    /// as the default so the catch-up speedup starts subtle (barely
    /// perceptible) rather than aggressive — the user can dial it up to
    /// 10% from Settings if they want faster recovery after a jitter spike.
    fn default_catch_up_speed_percent() -> u32 {
        5
    }

    /// Clamp range for `catch_up_speed_percent`: 5 to 10, matching the
    /// range the project owner asked for explicitly ("max 5%-10% faster").
    pub fn clamp_catch_up_speed_percent(percent: u32) -> u32 {
        percent.clamp(5, 10)
    }

    /// `$XDG_CONFIG_HOME/simple-screensharing/settings.toml` (or the
    /// platform equivalent), mirroring `RoomStore::config_path`.
    pub fn config_path() -> PathBuf {
        match config_base_dir() {
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

    use crate::net::format_join_string_with_hash;
    use crate::protocol::{cert_hash_of_der, encode_cert_hash};

    /// Real, parseable join strings for the same room (`id` byte picks the
    /// room), with and without a cert hash.
    fn link(server: &str, id: u8, hash: Option<&[u8]>) -> String {
        let hash = hash.map(cert_hash_of_der);
        format_join_string_with_hash(server, &[id; 16], &[0x42u8; 32], hash.as_ref())
    }

    #[test]
    fn upsert_adds_then_updates_in_place() {
        let mut store = RoomStore::default();
        store.upsert("friends".to_string(), "a|b|c".to_string());
        assert_eq!(store.rooms.len(), 1);
        // Same room, new name: still one entry.
        store.upsert("pals".to_string(), "a|b|c".to_string());
        assert_eq!(store.rooms.len(), 1);
        assert_eq!(store.rooms[0].name, "pals");
        // Different room, same name: a second entry (name is not the key).
        store.upsert("pals".to_string(), "x|y|z".to_string());
        assert_eq!(store.rooms.len(), 2);
    }

    /// The reported bug: the same link joined under two names is one entry,
    /// and the second (typed) name wins.
    #[test]
    fn same_link_under_two_names_is_one_entry_with_the_second_name() {
        let mut store = RoomStore::default();
        let js = link("relay:4433", 1, None);
        store.upsert("MovieNight".to_string(), js.clone());
        store.upsert("Films".to_string(), js.clone());
        assert_eq!(store.rooms.len(), 1);
        assert_eq!(store.rooms[0].name, "Films");
        assert_eq!(store.rooms[0].join_string, js);
        // A blank typed name keeps the existing one.
        store.upsert("  ".to_string(), js);
        assert_eq!(store.rooms[0].name, "Films");
    }

    /// A rotated relay cert (different 4th field) is the same room: one entry,
    /// stored link updated to the newest form, name and auto-connect kept.
    #[test]
    fn same_link_with_a_different_cert_hash_is_one_entry_and_updates_the_stored_link() {
        let mut store = RoomStore::default();
        let old = link("relay:4433", 1, Some(b"old cert"));
        let new = link("relay:4433", 1, Some(b"new cert"));
        assert_ne!(old, new);
        store.upsert("Movies".to_string(), old.clone());
        store.set_auto_connect(&old, true);
        store.upsert(String::new(), new.clone());
        assert_eq!(store.rooms.len(), 1);
        assert_eq!(store.rooms[0].join_string, new);
        assert_eq!(store.rooms[0].name, "Movies");
        assert!(store.rooms[0].auto_connect);
        // Hash-less and hashed spellings are the same room too.
        store.upsert(String::new(), link("relay:4433", 1, None));
        assert_eq!(store.rooms.len(), 1);
        // A different room id or server is a different room.
        store.upsert("Other".to_string(), link("relay:4433", 2, None));
        store.upsert("Elsewhere".to_string(), link("other-relay:4433", 1, None));
        assert_eq!(store.rooms.len(), 3);
    }

    /// `RoomKey` (= stored join string) stays stable across upserts that
    /// don't introduce a new spelling of the link: renames and re-saves of
    /// the identical string never move it. Only a different spelling (new
    /// cert hash) replaces it, and then it is the newest, connectable form.
    #[test]
    fn stored_join_string_is_stable_across_name_only_upserts() {
        let mut store = RoomStore::default();
        let js = link("relay:4433", 1, Some(b"cert"));
        store.upsert("A".to_string(), js.clone());
        store.upsert("B".to_string(), js.clone());
        store.rename(&js, "C");
        store.upsert(String::new(), js.clone());
        assert_eq!(store.rooms.len(), 1);
        assert_eq!(store.rooms[0].join_string, js, "the key must not move on name-only changes");
        assert_eq!(store.rooms[0].name, "C");
    }

    /// Rename, forget and auto-connect each hit exactly one entry, found by
    /// identity (so the cert-hash spelling doesn't matter), leaving the rest.
    #[test]
    fn rename_remove_and_auto_connect_affect_exactly_one_entry() {
        let mut store = RoomStore::default();
        let a = link("relay:4433", 1, Some(b"cert"));
        let a_plain = link("relay:4433", 1, None);
        let b = link("relay:4433", 2, None);
        // Two rooms that happen to carry the same name.
        store.upsert("Same".to_string(), a.clone());
        store.upsert("Same".to_string(), b.clone());
        assert_eq!(store.rooms.len(), 2);

        store.rename(&a_plain, "Renamed");
        assert_eq!(store.rooms[0].name, "Renamed");
        assert_eq!(store.rooms[1].name, "Same");

        store.set_auto_connect(&a_plain, true);
        assert!(store.rooms[0].auto_connect);
        assert!(!store.rooms[1].auto_connect);

        store.remove(&a_plain);
        assert_eq!(store.rooms.len(), 1);
        assert_eq!(store.rooms[0].join_string, b);

        // Unknown room / blank name: no-ops.
        store.rename(&a, "Ghost");
        store.rename(&b, "   ");
        store.remove(&a);
        assert_eq!(store.rooms.len(), 1);
        assert_eq!(store.rooms[0].name, "Same");
    }

    /// Duplicates already on disk collapse: first position, last non-blank
    /// name, newest join string, auto_connect OR-ed; unrelated rooms untouched.
    #[test]
    fn merge_duplicates_collapses_a_legacy_file() {
        let a_old = link("relay:4433", 1, Some(b"old"));
        let a_new = link("relay:4433", 1, Some(b"new"));
        let b = link("relay:4433", 2, None);
        let toml_str = format!(
            r#"
            [[rooms]]
            name = "MovieNight"
            join_string = "{a_old}"
            auto_connect = true

            [[rooms]]
            name = "Other"
            join_string = "{b}"

            [[rooms]]
            name = "Films"
            join_string = "{a_new}"

            [[rooms]]
            name = ""
            join_string = "{a_new}"
            "#
        );
        let mut store: RoomStore = toml::from_str(&toml_str).unwrap();
        assert!(store.merge_duplicates());
        assert_eq!(store.rooms.len(), 2);
        assert_eq!(store.rooms[0].name, "Films");
        // The last duplicate supplies the join string; a blank name never
        // replaces a real one.
        assert_eq!(store.rooms[0].join_string, a_new);
        assert!(store.rooms[0].auto_connect);
        assert_eq!(store.rooms[1].name, "Other");
        assert!(!store.rooms[1].auto_connect);
        // Idempotent.
        assert!(!store.merge_duplicates());
    }

    #[test]
    fn room_identity_drops_only_the_cert_hash() {
        let plain = link("relay:4433", 1, None);
        let hashed = link("relay:4433", 1, Some(b"cert"));
        assert!(hashed.ends_with(&encode_cert_hash(&cert_hash_of_der(b"cert"))));
        assert_eq!(room_identity(&hashed), plain);
        assert_eq!(room_identity(&format!("  {hashed}\n")), plain);
        assert_ne!(room_identity(&link("relay:4433", 2, None)), plain);
        // Unparseable strings fall back to their first three fields.
        assert_eq!(room_identity("a|b|c|d"), "a|b|c");
        assert_eq!(room_identity("a|b|c"), "a|b|c");
        assert_eq!(room_identity("garbage"), "garbage");
        assert!(RoomStore::same_room(&plain, &hashed));
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

    /// A freshly-`upsert`ed room defaults to not-auto-connecting — the
    /// "Auto-Connect" checkbox in the room-detail panel starts unchecked.
    #[test]
    fn upsert_defaults_auto_connect_to_false() {
        let mut store = RoomStore::default();
        store.upsert("friends".to_string(), "a|b|c".to_string());
        assert!(!store.rooms[0].auto_connect);
    }

    /// `set_auto_connect` flips the flag for the matching join string and
    /// round-trips through TOML — the persisted-preference path this
    /// task's Auto-Connect checkbox actually relies on.
    #[test]
    fn set_auto_connect_flips_the_matching_room_and_round_trips() {
        let mut store = RoomStore::default();
        store.upsert("friends".to_string(), "a|b|c".to_string());
        store.upsert("other".to_string(), "x|y|z".to_string());

        store.set_auto_connect("a|b|c", true);
        assert!(store.rooms.iter().find(|r| r.join_string == "a|b|c").unwrap().auto_connect);
        assert!(!store.rooms.iter().find(|r| r.join_string == "x|y|z").unwrap().auto_connect);

        let toml_str = toml::to_string_pretty(&store).unwrap();
        let parsed: RoomStore = toml::from_str(&toml_str).unwrap();
        assert!(parsed.rooms.iter().find(|r| r.join_string == "a|b|c").unwrap().auto_connect);

        // Unset again — also round-trips.
        store.set_auto_connect("a|b|c", false);
        assert!(!store.rooms.iter().find(|r| r.join_string == "a|b|c").unwrap().auto_connect);
    }

    /// A join string with no saved entry is a silent no-op, not a panic —
    /// mirrors `name_for`'s "miss stays a miss" contract.
    #[test]
    fn set_auto_connect_on_unknown_join_string_is_a_no_op() {
        let mut store = RoomStore::default();
        store.upsert("friends".to_string(), "a|b|c".to_string());
        store.set_auto_connect("does-not-exist", true);
        assert_eq!(store.rooms.len(), 1);
        assert!(!store.rooms[0].auto_connect);
    }

    /// A `rooms.toml` written before `auto_connect` existed (no such key in
    /// the file at all) must still parse, defaulting to `false` — the same
    /// forward-compat contract `#[serde(default)]` gives every other field
    /// in this module (see `app_settings_missing_*_defaults` below).
    #[test]
    fn legacy_toml_without_auto_connect_field_defaults_to_false() {
        let toml_str = r#"
            [[rooms]]
            name = "friends"
            join_string = "a|b|c"
        "#;
        let parsed: RoomStore = toml::from_str(toml_str).unwrap();
        assert_eq!(parsed.rooms.len(), 1);
        assert!(!parsed.rooms[0].auto_connect);
    }

    #[test]
    fn app_settings_defaults_to_english() {
        assert_eq!(AppSettings::default().language, "en");
    }

    /// Mailbox is the default since 2026-10-14 (see `PresentationMode::Mailbox`'s
    /// `Why:`: Fifo starves the main window while a pop-out shows a fast stream).
    #[test]
    fn app_settings_defaults_to_mailbox() {
        assert_eq!(AppSettings::default().presentation_mode, PresentationMode::Mailbox);
    }

    #[test]
    fn app_settings_toml_round_trip() {
        let settings = AppSettings {
            language: "de".to_string(),
            presentation_mode: PresentationMode::Mailbox,
            bandwidth_mode: BandwidthMode::Absolute,
            absolute_total_kbps: 12_345,
            destination_floor_kbps: 750,
            max_jitter_buffer_ms: 3_500,
            catch_up_speed_percent: 8,
            start_stream_defaults: StartStreamDefaults::default(),
            display_name: "mo".to_string(),
        };
        let toml_str = toml::to_string_pretty(&settings).unwrap();
        let parsed: AppSettings = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.language, "de");
        assert_eq!(parsed.presentation_mode, PresentationMode::Mailbox);
        assert_eq!(parsed.bandwidth_mode, BandwidthMode::Absolute);
        assert_eq!(parsed.absolute_total_kbps, 12_345);
        assert_eq!(parsed.destination_floor_kbps, 750);
        assert_eq!(parsed.max_jitter_buffer_ms, 3_500);
        assert_eq!(parsed.catch_up_speed_percent, 8);
        assert_eq!(parsed.display_name, "mo");
    }

    /// A settings file written before `display_name` existed must still
    /// parse, defaulting it to empty (not failing to load) — same
    /// missing-field tolerance as every other field here. Empty is
    /// `AppState::default()`'s cue to fall back to `whoami_fallback()`, so
    /// this preserves pre-setting behavior for anyone who never touches it.
    #[test]
    fn app_settings_missing_display_name_defaults_to_empty() {
        let parsed: AppSettings = toml::from_str("language = \"de\"\n").unwrap();
        assert_eq!(parsed.display_name, "");
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
        assert_eq!(parsed.presentation_mode, PresentationMode::Mailbox);
    }

    /// Same missing-field tolerance as above, for `max_jitter_buffer_ms`
    /// specifically — a settings file written before this field existed must
    /// still parse and default it to the jitter buffer's original hardcoded
    /// ceiling.
    #[test]
    fn app_settings_missing_max_jitter_buffer_ms_defaults() {
        let parsed: AppSettings = toml::from_str("language = \"de\"\n").unwrap();
        assert_eq!(parsed.max_jitter_buffer_ms, 2000);
    }

    #[test]
    fn clamp_max_jitter_buffer_ms_clamps_to_0_5000() {
        assert_eq!(AppSettings::clamp_max_jitter_buffer_ms(0), 0);
        assert_eq!(AppSettings::clamp_max_jitter_buffer_ms(5000), 5000);
        assert_eq!(AppSettings::clamp_max_jitter_buffer_ms(9999), 5000);
        assert_eq!(AppSettings::clamp_max_jitter_buffer_ms(2000), 2000);
    }

    /// Same missing-field tolerance as above, for `catch_up_speed_percent`
    /// specifically — a settings file written before this field existed
    /// must still parse and default it to the conservative end of the
    /// owner's requested range.
    #[test]
    fn app_settings_missing_catch_up_speed_percent_defaults() {
        let parsed: AppSettings = toml::from_str("language = \"de\"\n").unwrap();
        assert_eq!(parsed.catch_up_speed_percent, 5);
    }

    #[test]
    fn clamp_catch_up_speed_percent_clamps_to_5_10() {
        assert_eq!(AppSettings::clamp_catch_up_speed_percent(0), 5);
        assert_eq!(AppSettings::clamp_catch_up_speed_percent(5), 5);
        assert_eq!(AppSettings::clamp_catch_up_speed_percent(10), 10);
        assert_eq!(AppSettings::clamp_catch_up_speed_percent(9999), 10);
        assert_eq!(AppSettings::clamp_catch_up_speed_percent(7), 7);
    }

    /// A settings file written before `start_stream_defaults` existed must
    /// still parse, defaulting the whole nested struct rather than failing
    /// to load — same missing-field tolerance as the other fields above.
    #[test]
    fn app_settings_missing_start_stream_defaults_defaults() {
        let parsed: AppSettings = toml::from_str("language = \"de\"\n").unwrap();
        assert_eq!(parsed.start_stream_defaults, StartStreamDefaults::default());
    }

    #[test]
    fn start_stream_defaults_toml_round_trip() {
        let settings = AppSettings {
            start_stream_defaults: StartStreamDefaults {
                framerate: 60,
                bitrate_kbps: 12_000,
                tune: crate::capture::TuneMode::LowLatency,
                quality_level: crate::capture::QualityLevel::Performance,
                rate_control: crate::capture::RateControlMode::Cqp,
                codec: crate::capture::VideoCodec::H265,
            },
            ..AppSettings::default()
        };
        let parsed: AppSettings = toml::from_str(&toml::to_string_pretty(&settings).unwrap()).unwrap();
        assert_eq!(parsed.start_stream_defaults, settings.start_stream_defaults);
    }

    /// A `settings.toml` written before `codec` existed (no `codec` key at
    /// all in the nested `start_stream_defaults` table) must still parse and
    /// default `codec` to `VideoCodec::H264` — the same missing-field
    /// tolerance `app_settings_missing_start_stream_defaults_defaults`
    /// exercises for the whole nested struct, but specifically isolating this
    /// one new field so a future regression that makes `codec` accidentally
    /// required is caught here rather than only at the whole-struct level.
    #[test]
    fn start_stream_defaults_missing_codec_defaults_to_h264() {
        let parsed: AppSettings = toml::from_str(
            "[start_stream_defaults]\nframerate = 60\nbitrate_kbps = 12000\ntune = \"low_latency\"\nquality_level = \"performance\"\nrate_control = \"qvbr\"\n",
        )
        .unwrap();
        assert_eq!(parsed.start_stream_defaults.codec, crate::capture::VideoCodec::H264);
    }

    #[test]
    fn presentation_mode_to_wgpu_maps_expected_variants() {
        assert_eq!(PresentationMode::AutoVsync.to_wgpu(), wgpu::PresentMode::AutoVsync);
        assert_eq!(PresentationMode::Immediate.to_wgpu(), wgpu::PresentMode::Immediate);
        assert_eq!(PresentationMode::Mailbox.to_wgpu(), wgpu::PresentMode::Mailbox);
    }
}
