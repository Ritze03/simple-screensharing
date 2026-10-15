//! Encoder presets and the small option tables the Stream Control UI offers.
//!
//! Ported from the former Electron frontend (its `components.js`, removed in
//! the egui rework: `ENCODER_PRESETS` / `activeEncoderPreset` / `CODEC_OPTIONS`, and
//! `stream-control.js`'s `FRAMERATE_OPTIONS` / `framerateOptionsFor`) so the
//! native UI stays a thin view over the lib: it asks this module "which
//! preset is active?" and "what does picking Gaming set?" instead of owning
//! the table itself.
//!
//! # What a preset is
//!
//! A preset is a named combination of four encoder knobs — [`TuneMode`],
//! [`QualityLevel`], [`RateControlMode`] and the GOP/keyframe interval in
//! milliseconds ([`EncoderTuning`]). Picking a preset is a convenience
//! mapping onto those four fields, not a fifth setting: nothing about a
//! preset is stored anywhere, so "which preset is active" is always derived
//! from the four fields by [`EncoderPreset::detect`]. A hand-tuned combination
//! that matches no preset is a legal, unhighlighted state.
//!
//! Where the presets get applied (inherent methods, defined at the bottom of
//! this file):
//! - a running destination's edit buffer — [`DestEdit::apply_preset`], or
//!   [`AppState::apply_preset_to_destination`] to also push it into the live
//!   encoder;
//! - the "next capture" settings of the Start-a-stream form —
//!   [`AppState::apply_preset_to_start_stream`].
//!
//! `Why:` those methods live here, not in `session.rs`: the preset table and
//! every place it is applied or reverse-looked-up stay in one file, and
//! `session.rs` (already ~4k lines) needs no `crate::presets` import. Callers
//! see them as ordinary `AppState`/`DestEdit` methods either way.
//!
//! # Why these four, and why these values
//!
//! (Carried over from the JS table's comments; 2026-07-30 rate-control
//! collapse from three variants to CQP/QVBR.)
//!
//! - **Low-Latency** wants predictable, bounded bandwidth: Dynamic (`Qvbr`,
//!   i.e. VA `vbr`), fastest quality level (`performance`).
//! - **Balanced** and **Quality** both want "spend the bandwidth budget on
//!   quality", which is exactly what Dynamic does: `Qvbr`.
//! - **Gaming** is Dynamic as well, paired with a much shorter GOP
//!   ([`GAMING_GOP_MILLIS`], 0.75 s vs. the 2 s default) for faster recovery
//!   from motion artefacts, and the fast end of Quality Level (`performance`)
//!   to favour low encode latency.
//! - `Why:` Low-Latency and Gaming used to be Static (`Cqp`, 2026-07-30 to
//!   2026-10-15). A constant quantizer ignores the configured bitrate, so big
//!   motion produced bursts above the link's capacity and the stream froze
//!   (user report 2026-10-15, task #87). Dynamic keeps the bitrate bounded.
//!   A saved setting of Low-Latency+Cqp now shows as a hand-tuned
//!   combination (no highlighted preset) until a preset is picked again.
//! - **Balanced** is the middle, default option: it equals `AppState`'s own
//!   default tune/quality/rate-control/GOP, so a freshly launched app's "next
//!   capture" settings already *are* the Balanced preset.

use crate::capture::{DEFAULT_GOP_MILLIS, QualityLevel, RateControlMode, TuneMode, VideoCodec};
use crate::session::{AppState, DestEdit, DestinationId};

/// GOP/keyframe interval of the Gaming preset, ms. Every other preset keeps
/// [`DEFAULT_GOP_MILLIS`] (the protocol spec's 2 s self-heal ceiling).
pub const GAMING_GOP_MILLIS: u32 = 750;

/// The four encoder knobs a preset sets together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderTuning {
    pub tune: TuneMode,
    pub quality_level: QualityLevel,
    pub rate_control: RateControlMode,
    /// Keyframe/GOP interval in milliseconds (see
    /// `capture::destination::VideoParams::gop_millis`).
    pub gop_millis: u32,
}

impl EncoderTuning {
    /// The preset whose four fields exactly match `self`, or `None` for a
    /// hand-tuned combination. Reverse lookup for highlighting the active
    /// preset button.
    pub fn preset(self) -> Option<EncoderPreset> {
        EncoderPreset::detect(self)
    }
}

impl Default for EncoderTuning {
    /// The Balanced preset — also what `AppState::default()` starts with.
    fn default() -> Self {
        EncoderPreset::Balanced.tuning()
    }
}

/// The preset picker's options, in display order (Low-Latency, Balanced,
/// Quality, Gaming — [`EncoderPreset::ALL`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncoderPreset {
    LowLatency,
    Balanced,
    Quality,
    Gaming,
}

impl EncoderPreset {
    pub const ALL: [EncoderPreset; 4] = [Self::LowLatency, Self::Balanced, Self::Quality, Self::Gaming];

    /// Stable identifier (the JS table's `value`), usable as a settings/IPC
    /// key.
    pub const fn value(self) -> &'static str {
        match self {
            Self::LowLatency => "low_latency",
            Self::Balanced => "balanced",
            Self::Quality => "quality",
            Self::Gaming => "gaming",
        }
    }

    /// Inverse of [`Self::value`].
    pub fn from_value(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.value() == value)
    }

    /// Button label, localized (en: the JS table's `label`).
    pub fn label(self) -> &'static str {
        localized(match self {
            Self::LowLatency => "presets.low_latency.label",
            Self::Balanced => "presets.balanced.label",
            Self::Quality => "presets.quality.label",
            Self::Gaming => "presets.gaming.label",
        })
    }

    /// Tooltip text, localized (en: the JS table's `title`).
    pub fn description(self) -> &'static str {
        localized(match self {
            Self::LowLatency => "presets.low_latency.description",
            Self::Balanced => "presets.balanced.description",
            Self::Quality => "presets.quality.description",
            Self::Gaming => "presets.gaming.description",
        })
    }

    /// The four encoder fields this preset sets.
    pub const fn tuning(self) -> EncoderTuning {
        match self {
            Self::LowLatency => EncoderTuning {
                tune: TuneMode::LowLatency,
                quality_level: QualityLevel::Performance,
                rate_control: RateControlMode::Qvbr,
                gop_millis: DEFAULT_GOP_MILLIS,
            },
            Self::Balanced => EncoderTuning {
                tune: TuneMode::BestQuality,
                quality_level: QualityLevel::Balanced,
                rate_control: RateControlMode::Qvbr,
                gop_millis: DEFAULT_GOP_MILLIS,
            },
            Self::Quality => EncoderTuning {
                tune: TuneMode::BestQuality,
                quality_level: QualityLevel::Quality,
                rate_control: RateControlMode::Qvbr,
                gop_millis: DEFAULT_GOP_MILLIS,
            },
            Self::Gaming => EncoderTuning {
                tune: TuneMode::LowLatency,
                quality_level: QualityLevel::Performance,
                rate_control: RateControlMode::Qvbr,
                gop_millis: GAMING_GOP_MILLIS,
            },
        }
    }

    /// Whichever preset's four fields exactly match `tuning`, or `None` if
    /// none does (an allowed, unhighlighted state).
    pub fn detect(tuning: EncoderTuning) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.tuning() == tuning)
    }
}

// ---------------------------------------------------------------------
// Applying presets
// ---------------------------------------------------------------------

impl DestEdit {
    /// The four preset-driven encoder fields of this edit buffer.
    pub fn tuning(&self) -> EncoderTuning {
        EncoderTuning {
            tune: self.tune,
            quality_level: self.quality_level,
            rate_control: self.rate_control,
            gop_millis: self.gop_millis,
        }
    }

    /// Overwrite tune / quality level / rate control / GOP with `preset`'s
    /// values. Edit buffer only — nothing reaches the live encoder until
    /// [`AppState::apply_destination_edit`] (or use
    /// [`AppState::apply_preset_to_destination`], which does both).
    pub fn apply_preset(&mut self, preset: EncoderPreset) {
        let t = preset.tuning();
        self.tune = t.tune;
        self.quality_level = t.quality_level;
        self.rate_control = t.rate_control;
        self.gop_millis = t.gop_millis;
    }

    /// Which preset this buffer currently equals, if any — for highlighting
    /// the active preset button. `None` for a hand-tuned combination.
    pub fn active_preset(&self) -> Option<EncoderPreset> {
        self.tuning().preset()
    }
}

impl AppState {
    /// The four preset-driven encoder fields of the live "next capture"
    /// settings (what the Start-a-stream form edits).
    pub fn start_stream_tuning(&self) -> EncoderTuning {
        EncoderTuning {
            tune: self.tune,
            quality_level: self.quality_level,
            rate_control: self.rate_control,
            gop_millis: self.gop_millis,
        }
    }

    /// Which preset the "next capture" settings currently equal, if any — for
    /// highlighting the active preset button.
    pub fn start_stream_preset(&self) -> Option<EncoderPreset> {
        self.start_stream_tuning().preset()
    }

    /// Picking `preset` on the Start-a-stream form: set the live "next
    /// capture" tune / quality level / rate control / GOP together. Not
    /// persisted (GOP never is; the rest is saved at "Go live" by
    /// [`AppState::go_live_and_save_defaults`]).
    pub fn apply_preset_to_start_stream(&mut self, preset: EncoderPreset) {
        let t = preset.tuning();
        self.tune = t.tune;
        self.quality_level = t.quality_level;
        self.rate_control = t.rate_control;
        self.gop_millis = t.gop_millis;
    }

    /// Picking `preset` on a destination row: set the row's preset-driven
    /// edit fields and apply immediately (the preset buttons apply on click,
    /// unlike the Advanced fields' explicit Apply). No-op if the destination
    /// is already gone.
    pub fn apply_preset_to_destination(&mut self, destination_id: DestinationId, preset: EncoderPreset) {
        self.with_destination(destination_id, |dest| dest.edit.apply_preset(preset));
        self.apply_destination_edit(destination_id);
    }
}

// ---------------------------------------------------------------------
// Option tables
// ---------------------------------------------------------------------

/// The framerates the Framerate picker offers. 240 exceeds real monitor Hz
/// but is the project's documented spec ceiling, offered as the top value on
/// explicit user request to experiment up to it.
pub const FRAMERATE_OPTIONS: [u32; 8] = [15, 30, 60, 90, 120, 144, 165, 240];

/// [`FRAMERATE_OPTIONS`] plus `current` if it isn't one of them (sorted).
///
/// `Why:` a stream started before the list changed, or one whose rate was
/// clamped server-side, must still be shown truthfully rather than silently
/// snapped to a neighbour. A `current` of 0 (unset) adds nothing.
pub fn framerate_options_for(current: u32) -> Vec<u32> {
    let mut options = FRAMERATE_OPTIONS.to_vec();
    if current != 0 && !options.contains(&current) {
        options.push(current);
        options.sort_unstable();
    }
    options
}

/// Lower bound of the per-stream bitrate slider, kbit/s. The upper bound is
/// `session::MAX_BITRATE_KBPS`.
pub const MIN_BITRATE_KBPS: u32 = 200;
/// Step of the per-stream bitrate slider, kbit/s.
pub const BITRATE_STEP_KBPS: u32 = 100;

/// Codecs in the order the Codec picker shows them (H.264, AV1, H.265 — the
/// order the project owner asked for; *not* `capture::supported_codecs`'
/// order). The UI greys out any entry missing from the machine's supported
/// list ([`crate::session::AppState::supported_codecs`]).
pub const CODEC_DISPLAY_ORDER: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::Av1, VideoCodec::H265];

/// Codec picker button label.
pub const fn codec_label(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "H.264",
        VideoCodec::Av1 => "AV1",
        VideoCodec::H265 => "H.265",
    }
}

/// Codec picker tooltip text, localized.
pub fn codec_description(codec: VideoCodec) -> &'static str {
    localized(match codec {
        VideoCodec::H264 => "presets.codec.h264.description",
        VideoCodec::Av1 => "presets.codec.av1.description",
        VideoCodec::H265 => "presets.codec.h265.description",
    })
}

/// `t!(key)` as a `&'static str`, so the label/description signatures (and the
/// UI callers that borrow them into `&str` tables) stay unchanged.
///
/// `Why:` the leak is bounded: one entry per (locale, key), interned on first use.
fn localized(key: &'static str) -> &'static str {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<(String, &'static str), &'static str>>> = OnceLock::new();
    let locale = rust_i18n::locale().to_string();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    cache.entry((locale, key)).or_insert_with(|| Box::leak(rust_i18n::t!(key).into_owned().into_boxed_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The former JS `ENCODER_PRESETS` table (the Electron frontend's `components.js`, removed
    /// ~:994), copied verbatim as `(value, label, title, tune, qualityLevel,
    /// rateControl, gopMillis)` with the JS wire strings
    /// (`dto::parse_tune_mode` etc.) and `DEFAULT_GOP_MILLIS = 2000`,
    /// `GAMING_GOP_MILLIS = 750`.
    const JS_TABLE: [(&str, &str, &str, &str, &str, &str, u32); 4] = [
        (
            "low_latency",
            "Low-Latency",
            "Fastest encode, prioritizes reduced delay over compression efficiency.",
            "low_latency",
            "performance",
            "qvbr",
            2000,
        ),
        ("balanced", "Balanced", "The default — a good mix of latency and quality.", "best_quality", "balanced", "qvbr", 2000),
        ("quality", "Quality", "Maximizes visual quality within the bandwidth ceiling.", "best_quality", "quality", "qvbr", 2000),
        (
            "gaming",
            "Gaming",
            "Tuned for fast-motion/gaming content — shorter keyframe interval for quicker recovery from motion artifacts.",
            "low_latency",
            "performance",
            "qvbr",
            750,
        ),
    ];

    fn tune_str(t: TuneMode) -> &'static str {
        match t {
            TuneMode::LowLatency => "low_latency",
            TuneMode::BestQuality => "best_quality",
        }
    }
    fn quality_str(q: QualityLevel) -> &'static str {
        match q {
            QualityLevel::Performance => "performance",
            QualityLevel::Balanced => "balanced",
            QualityLevel::Quality => "quality",
        }
    }
    fn rate_control_str(r: RateControlMode) -> &'static str {
        match r {
            RateControlMode::Cqp => "cqp",
            RateControlMode::Qvbr => "qvbr",
        }
    }

    #[test]
    fn every_preset_matches_the_js_table_exactly() {
        assert_eq!(EncoderPreset::ALL.len(), JS_TABLE.len());
        for (preset, (value, label, title, tune, quality, rc, gop)) in EncoderPreset::ALL.into_iter().zip(JS_TABLE) {
            assert_eq!(preset.value(), value);
            assert_eq!(preset.label(), label);
            assert_eq!(preset.description(), title);
            let t = preset.tuning();
            assert_eq!(tune_str(t.tune), tune, "{value} tune");
            assert_eq!(quality_str(t.quality_level), quality, "{value} quality level");
            assert_eq!(rate_control_str(t.rate_control), rc, "{value} rate control");
            assert_eq!(t.gop_millis, gop, "{value} gop");
        }
    }

    #[test]
    fn gop_constants_match_js() {
        assert_eq!(DEFAULT_GOP_MILLIS, 2000);
        assert_eq!(GAMING_GOP_MILLIS, 750);
    }

    #[test]
    fn detect_is_the_inverse_of_tuning() {
        for preset in EncoderPreset::ALL {
            assert_eq!(EncoderPreset::detect(preset.tuning()), Some(preset));
            assert_eq!(preset.tuning().preset(), Some(preset));
        }
    }

    /// Low-Latency and Gaming differ only in GOP, so the GOP is what the
    /// reverse lookup has to tell them apart by.
    #[test]
    fn detect_distinguishes_low_latency_from_gaming_by_gop_only() {
        let mut t = EncoderPreset::LowLatency.tuning();
        assert_eq!(t.preset(), Some(EncoderPreset::LowLatency));
        t.gop_millis = GAMING_GOP_MILLIS;
        assert_eq!(t.preset(), Some(EncoderPreset::Gaming));
    }

    #[test]
    fn hand_tuned_combination_matches_no_preset() {
        let tuning = EncoderTuning {
            tune: TuneMode::BestQuality,
            quality_level: QualityLevel::Performance,
            rate_control: RateControlMode::Cqp,
            gop_millis: DEFAULT_GOP_MILLIS,
        };
        assert_eq!(tuning.preset(), None);
        let mut balanced = EncoderPreset::Balanced.tuning();
        balanced.gop_millis = 1234;
        assert_eq!(balanced.preset(), None);
    }

    #[test]
    fn default_tuning_is_balanced_and_equals_the_enums_defaults() {
        assert_eq!(EncoderTuning::default().preset(), Some(EncoderPreset::Balanced));
        let t = EncoderTuning::default();
        assert_eq!(t.tune, TuneMode::default());
        assert_eq!(t.quality_level, QualityLevel::default());
        assert_eq!(t.rate_control, RateControlMode::default());
        assert_eq!(t.gop_millis, DEFAULT_GOP_MILLIS);
    }

    #[test]
    fn value_round_trips() {
        for preset in EncoderPreset::ALL {
            assert_eq!(EncoderPreset::from_value(preset.value()), Some(preset));
        }
        assert_eq!(EncoderPreset::from_value("nope"), None);
    }

    #[test]
    fn framerate_options_add_a_nonstandard_current_rate_sorted() {
        assert_eq!(framerate_options_for(60), FRAMERATE_OPTIONS.to_vec());
        assert_eq!(framerate_options_for(0), FRAMERATE_OPTIONS.to_vec());
        assert_eq!(framerate_options_for(100), vec![15, 30, 60, 90, 100, 120, 144, 165, 240]);
        assert_eq!(framerate_options_for(10).first(), Some(&10));
        assert_eq!(framerate_options_for(300).last(), Some(&300));
    }

    #[test]
    fn codec_display_order_matches_js() {
        assert_eq!(CODEC_DISPLAY_ORDER.map(codec_label), ["H.264", "AV1", "H.265"]);
    }

    // ---- applying presets (needs a real `AppState`, no network/GStreamer) ----

    use crate::session::test_support::{insert_destination, isolated_config_home, video_params};
    use crate::session::Destination;

    #[test]
    fn dest_edit_apply_preset_sets_only_the_four_tuning_fields() {
        let mut edit = Destination::edit_from(video_params());
        edit.scale_percent = 50;
        edit.framerate = 144;
        edit.bitrate_kbps = 12_345;
        edit.codec = VideoCodec::Av1;
        let untouched = edit;

        for preset in EncoderPreset::ALL {
            edit.apply_preset(preset);
            assert_eq!(edit.tuning(), preset.tuning());
            assert_eq!(edit.active_preset(), Some(preset));
            assert_eq!(
                (edit.scale_percent, edit.framerate, edit.bitrate_kbps, edit.codec),
                (untouched.scale_percent, untouched.framerate, untouched.bitrate_kbps, untouched.codec),
                "{} must not touch non-preset fields",
                preset.value()
            );
        }

        edit.gop_millis = 1234;
        assert_eq!(edit.active_preset(), None, "a hand-tuned buffer highlights no preset");
    }

    #[tokio::test]
    async fn start_stream_settings_follow_the_applied_preset() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        assert_eq!(state.start_stream_preset(), Some(EncoderPreset::Balanced), "a fresh app already is the Balanced preset");

        for preset in EncoderPreset::ALL {
            state.apply_preset_to_start_stream(preset);
            assert_eq!(state.start_stream_tuning(), preset.tuning());
            assert_eq!(state.start_stream_preset(), Some(preset));
        }

        // Gaming is the only preset that moves the GOP off the default.
        state.apply_preset_to_start_stream(EncoderPreset::Gaming);
        assert_eq!(state.gop_millis, GAMING_GOP_MILLIS);
        state.apply_preset_to_start_stream(EncoderPreset::Quality);
        assert_eq!(state.gop_millis, DEFAULT_GOP_MILLIS);

        // Applying a preset leaves the rest of the form alone.
        state.framerate = 144;
        state.bitrate_kbps = 20_000;
        state.apply_preset_to_start_stream(EncoderPreset::LowLatency);
        assert_eq!((state.framerate, state.bitrate_kbps), (144, 20_000));
    }

    #[tokio::test]
    async fn apply_preset_to_destination_updates_the_row_edit_buffer() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let (capture_id, destination_id) = insert_destination(&mut state);
        assert_eq!(state.captures[&capture_id].destinations[&destination_id].edit.active_preset(), Some(EncoderPreset::Balanced));

        state.apply_preset_to_destination(destination_id, EncoderPreset::Gaming);

        let edit = state.captures[&capture_id].destinations[&destination_id].edit;
        assert_eq!(edit.active_preset(), Some(EncoderPreset::Gaming));
        assert_eq!((edit.framerate, edit.bitrate_kbps), (60, 8000), "non-preset fields stay as they were");

        // Unknown destination: no-op.
        state.apply_preset_to_destination(crate::session::DestinationId(999), EncoderPreset::Quality);
        assert_eq!(state.captures[&capture_id].destinations[&destination_id].edit.active_preset(), Some(EncoderPreset::Gaming));
    }
}
