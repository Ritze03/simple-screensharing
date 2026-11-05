//! Encoder presets and the small option tables the Stream Control UI offers.
//!
//! # What a preset is
//!
//! A preset is a named combination of the encoder knobs [`TuneMode`],
//! [`QualityLevel`] and the GOP/keyframe interval in milliseconds
//! ([`EncoderTuning`]), plus the **frame rate** it starts from. Picking one is a
//! convenience mapping, not a setting of its own: nothing about a preset is
//! stored by name, "which preset is active" is derived from the stored fields
//! by [`EncoderPreset::detect`] (FPS + quality level + keyframe interval), and a
//! combination that matches none is a legal, unhighlighted state.
//!
//! | preset | FPS | encoding (`QualityLevel`) | keyframe |
//! | --- | --- | --- | --- |
//! | Text | 15 | Quality | 250 ms |
//! | Quality | 60 | Quality | 1000 ms |
//! | Gaming | 120 | Performance | 250 ms |
//!
//! `Why (user's table, plan decision D8, 2026-11-04):` three presets replace
//! Low-Latency / Balanced / Quality / Gaming and the Advanced tuning rows. Only
//! Performance and Quality remain as modes; the old Balanced and Low-Latency
//! stored values load as Quality. **A preset sets the FPS once; changing the FPS
//! by hand keeps the encoder mode and the keyframe interval.**
//!
//! `Why the FPS is part of the match (user, 2026-11-05, changing D8's "never part of detect"):` "it should
//! only show a preset, if the settings match it perfectly". 120 fps + Quality + 250 ms used to light Text.
//! So a preset is lit only when FPS, encoding and keyframe all equal its row; a hand-changed FPS unlights
//! it and picking the matching FPS again relights it.
//!
//! The tune (x264 `zerolatency`, ignored by the VA encoders) is not user-visible
//! any more: Gaming sets [`TuneMode::LowLatency`], the other two
//! [`TuneMode::BestQuality`] (what those presets had before), and detection
//! ignores it.
//!
//! Where the presets get applied (inherent methods, defined at the bottom of
//! this file):
//! - a running destination's edit buffer: [`DestEdit::apply_preset`], or
//!   [`AppState::apply_preset_to_destination`] to also push it into the live
//!   encoder;
//! - the "next capture" settings of the Start-a-stream form:
//!   [`AppState::apply_preset_to_start_stream`].
//!
//! `Why:` those methods live here, not in `session.rs`: the preset table and
//! every place it is applied or reverse-looked-up stay in one file.
//!
//! Rate control is not part of a preset (removed 2026-10-15, #88): every
//! encode is Dynamic. See `superdoc/features/stream-control.md` "Rate control".

use crate::capture::{QualityLevel, TuneMode, VideoCodec};
use crate::session::{AppState, DestEdit, DestinationId};

/// The encoder knobs a preset sets together (the frame rate is separate:
/// [`EncoderPreset::framerate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderTuning {
    pub tune: TuneMode,
    pub quality_level: QualityLevel,
    /// Keyframe/GOP interval in milliseconds (see
    /// `capture::destination::VideoParams::gop_millis`).
    pub gop_millis: u32,
}

impl EncoderTuning {
}

/// Whether the Codec row offers a choice at all. With a single producible
/// codec (software x264 is H.264 only, `capture::supported_codecs`) every
/// other option would be greyed out and nothing the user clicks changes the
/// encode, so the UI hides the row.
pub fn codec_choice_offered(supported: &[VideoCodec]) -> bool {
    supported.len() > 1
}

impl Default for EncoderTuning {
    /// The Quality preset: also what `AppState::default()` starts with.
    fn default() -> Self {
        EncoderPreset::Quality.tuning()
    }
}

/// The preset picker's options, in display order ([`EncoderPreset::ALL`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EncoderPreset {
    Text,
    Quality,
    Gaming,
}

impl EncoderPreset {
    pub const ALL: [EncoderPreset; 3] = [Self::Text, Self::Quality, Self::Gaming];

    /// Stable identifier, usable as a settings/IPC key (same strings as the browser's).
    pub const fn value(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Quality => "quality",
            Self::Gaming => "gaming",
        }
    }

    /// Inverse of [`Self::value`]. The retired `balanced` and `low_latency`
    /// load as Quality (the browser stores preset names, `docs/js/core/video-tx.js`).
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "balanced" | "low_latency" => Some(Self::Quality),
            _ => Self::ALL.into_iter().find(|p| p.value() == value),
        }
    }

    /// Button label, localized.
    pub fn label(self) -> &'static str {
        match self {
            Self::Text => localized("presets.text.label", "Text"),
            Self::Quality => localized("presets.quality.label", "Quality"),
            Self::Gaming => localized("presets.gaming.label", "Gaming"),
        }
    }

    /// Tooltip text, localized.
    pub fn description(self) -> &'static str {
        match self {
            Self::Text => localized("presets.text.description", "Sharp, readable text and documents at a low frame rate."),
            Self::Quality => localized("presets.quality.description", "Maximizes visual quality within the bandwidth ceiling."),
            Self::Gaming => localized(
                "presets.gaming.description",
                "Tuned for fast-motion/gaming content: a high frame rate and a short keyframe interval for quicker recovery.",
            ),
        }
    }

    /// The frame rate picking this preset starts from.
    pub const fn framerate(self) -> u32 {
        match self {
            Self::Text => 15,
            Self::Quality => 60,
            Self::Gaming => 120,
        }
    }

    /// The encoder fields this preset sets.
    pub const fn tuning(self) -> EncoderTuning {
        match self {
            Self::Text => EncoderTuning { tune: TuneMode::BestQuality, quality_level: QualityLevel::Quality, gop_millis: 250 },
            Self::Quality => EncoderTuning { tune: TuneMode::BestQuality, quality_level: QualityLevel::Quality, gop_millis: 1000 },
            Self::Gaming => EncoderTuning { tune: TuneMode::LowLatency, quality_level: QualityLevel::Performance, gop_millis: 250 },
        }
    }

    /// The preset that equals `framerate` and `tuning`'s quality level and
    /// keyframe interval exactly, or `None` (an allowed, unhighlighted state).
    /// The tune is ignored: it is not shown, and the VA encoders do not read it.
    pub fn detect(framerate: u32, tuning: EncoderTuning) -> Option<Self> {
        Self::ALL.into_iter().find(|p| {
            let t = p.tuning();
            (p.framerate(), t.quality_level, t.gop_millis) == (framerate, tuning.quality_level, tuning.gop_millis)
        })
    }
}

// ---------------------------------------------------------------------
// Applying presets
// ---------------------------------------------------------------------

impl DestEdit {
    /// The three preset-driven encoder fields of this edit buffer.
    pub fn tuning(&self) -> EncoderTuning {
        EncoderTuning {
            tune: self.tune,
            quality_level: self.quality_level,
            gop_millis: self.gop_millis,
        }
    }

    /// Overwrite frame rate, tune, quality level and GOP with `preset`'s
    /// values. Edit buffer only — nothing reaches the live encoder until
    /// [`AppState::apply_destination_edit`] (or use
    /// [`AppState::apply_preset_to_destination`], which does both).
    pub fn apply_preset(&mut self, preset: EncoderPreset) {
        let t = preset.tuning();
        self.framerate = preset.framerate();
        self.tune = t.tune;
        self.quality_level = t.quality_level;
        self.gop_millis = t.gop_millis;
    }

    /// Which preset this buffer currently equals, if any — for highlighting
    /// the active preset button. `None` for a hand-tuned combination.
    pub fn active_preset(&self) -> Option<EncoderPreset> {
        EncoderPreset::detect(self.framerate, self.tuning())
    }
}

impl AppState {
    /// The three preset-driven encoder fields of the live "next capture"
    /// settings (what the Start-a-stream form edits).
    pub fn start_stream_tuning(&self) -> EncoderTuning {
        EncoderTuning {
            tune: self.tune,
            quality_level: self.quality_level,
            gop_millis: self.gop_millis,
        }
    }

    /// Which preset the "next capture" settings currently equal, if any — for
    /// highlighting the active preset button.
    pub fn start_stream_preset(&self) -> Option<EncoderPreset> {
        EncoderPreset::detect(self.framerate, self.start_stream_tuning())
    }

    /// Picking `preset` on the Start-a-stream form: set the live "next
    /// capture" frame rate / tune / quality level / GOP together. Saved at
    /// "Go live" by [`AppState::go_live_and_save_defaults`].
    pub fn apply_preset_to_start_stream(&mut self, preset: EncoderPreset) {
        let t = preset.tuning();
        self.framerate = preset.framerate();
        self.tune = t.tune;
        self.quality_level = t.quality_level;
        self.gop_millis = t.gop_millis;
    }

    /// Picking `preset` on a destination row: set the row's preset-driven
    /// edit fields and apply immediately (the preset buttons apply on click).
    /// No-op if the destination
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
    match codec {
        VideoCodec::H264 => localized("presets.codec.h264.description", "Most compatible — decodes everywhere, including hardware that can't do H.265/AV1."),
        VideoCodec::Av1 => localized("presets.codec.av1.description", "Modern, royalty-free, best efficiency on Intel/NVIDIA hardware."),
        VideoCodec::H265 => localized("presets.codec.h265.description", "Best quality on this hardware; licensing caveats on some platforms/browsers."),
    }
}

/// `t!(key)` as a `&'static str`, so the label/description signatures (and the
/// UI callers that borrow them into `&str` tables) stay unchanged. A key the
/// locale files lack reads as `en` (what `ui::tr` does for the UI).
///
/// `Why:` the leak is bounded: one entry per (locale, key), interned on first use.
fn localized(key: &'static str, en: &'static str) -> &'static str {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    static CACHE: OnceLock<Mutex<HashMap<(String, &'static str), &'static str>>> = OnceLock::new();
    let locale = rust_i18n::locale().to_string();
    let mut cache = CACHE.get_or_init(Default::default).lock().unwrap();
    cache.entry((locale, key)).or_insert_with(|| {
        let s = rust_i18n::t!(key);
        let found = s.as_ref() != key && !s.ends_with(key);
        Box::leak(if found { s.into_owned() } else { en.to_string() }.into_boxed_str())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quality_str(q: QualityLevel) -> &'static str {
        match q {
            QualityLevel::Performance => "performance",
            QualityLevel::Quality => "quality",
        }
    }

    /// The user's table (plan decision D8): `(value, fps, quality level, keyframe ms)`.
    const TABLE: [(&str, u32, &str, u32); 3] = [("text", 15, "quality", 250), ("quality", 60, "quality", 1000), ("gaming", 120, "performance", 250)];

    #[test]
    fn presets_match_the_users_table() {
        assert_eq!(EncoderPreset::ALL.len(), TABLE.len());
        for (preset, (value, fps, quality, gop)) in EncoderPreset::ALL.into_iter().zip(TABLE) {
            assert_eq!(preset.value(), value);
            assert_eq!(preset.framerate(), fps, "{value} fps");
            assert_eq!(quality_str(preset.tuning().quality_level), quality, "{value} quality level");
            assert_eq!(preset.tuning().gop_millis, gop, "{value} keyframe");
        }
    }

    #[test]
    fn a_preset_is_detected_only_on_an_exact_fps_encoding_keyframe_match() {
        let t = EncoderPreset::Text.tuning();
        assert_eq!(EncoderPreset::detect(15, t), Some(EncoderPreset::Text));
        assert_eq!(EncoderPreset::detect(120, t), None, "120 fps + Quality + 250 ms is no preset");
        assert_eq!(EncoderPreset::detect(120, EncoderPreset::Gaming.tuning()), Some(EncoderPreset::Gaming));
    }

    #[test]
    fn labels_are_readable_without_locale_entries() {
        // `presets.text.*` is not in the locale files yet: the English fallback shows, never the key.
        assert!(!EncoderPreset::Text.label().starts_with("presets."));
        assert!(!EncoderPreset::Text.description().starts_with("presets."));
    }

    #[test]
    fn detect_is_the_inverse_of_tuning() {
        for preset in EncoderPreset::ALL {
            assert_eq!(EncoderPreset::detect(preset.framerate(), preset.tuning()), Some(preset));
        }
    }

    /// Text and Quality differ only in the keyframe interval, so that is what tells them apart;
    /// the tune is not shown and does not count.
    #[test]
    fn detect_reads_fps_quality_level_and_keyframe_only() {
        let mut t = EncoderPreset::Text.tuning();
        t.gop_millis = 1000;
        assert_eq!(EncoderPreset::detect(60, t), Some(EncoderPreset::Quality));
        t.tune = TuneMode::LowLatency;
        assert_eq!(EncoderPreset::detect(60, t), Some(EncoderPreset::Quality), "a stale tune does not unhighlight");
    }

    #[test]
    fn hand_tuned_combination_matches_no_preset() {
        let mut t = EncoderPreset::Quality.tuning();
        t.gop_millis = 2000;
        assert_eq!(EncoderPreset::detect(60, t), None);
        let mut t = EncoderPreset::Quality.tuning();
        t.quality_level = QualityLevel::Performance;
        assert_eq!(EncoderPreset::detect(60, t), None, "Performance at 1000 ms is none of the three");
    }

    #[test]
    fn default_tuning_is_the_quality_preset() {
        assert_eq!(EncoderTuning::default(), EncoderPreset::Quality.tuning());
        assert_eq!(EncoderTuning::default().gop_millis, crate::capture::DEFAULT_GOP_MILLIS);
    }

    #[test]
    fn value_round_trips_and_retired_values_load_as_quality() {
        for preset in EncoderPreset::ALL {
            assert_eq!(EncoderPreset::from_value(preset.value()), Some(preset));
        }
        assert_eq!(EncoderPreset::from_value("balanced"), Some(EncoderPreset::Quality));
        assert_eq!(EncoderPreset::from_value("low_latency"), Some(EncoderPreset::Quality));
        assert_eq!(EncoderPreset::from_value("nope"), None);
    }

    #[test]
    fn codec_row_needs_a_choice() {
        assert!(!codec_choice_offered(&[VideoCodec::H264]));
        assert!(codec_choice_offered(&[VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1]));
    }

    #[test]
    fn framerate_options_add_a_nonstandard_current_rate_sorted() {
        assert_eq!(framerate_options_for(60), FRAMERATE_OPTIONS.to_vec());
        assert_eq!(framerate_options_for(0), FRAMERATE_OPTIONS.to_vec());
        assert_eq!(framerate_options_for(100), vec![15, 30, 60, 90, 100, 120, 144, 165, 240]);
        assert_eq!(framerate_options_for(10).first(), Some(&10));
        assert_eq!(framerate_options_for(300).last(), Some(&300));
        for p in EncoderPreset::ALL {
            assert!(FRAMERATE_OPTIONS.contains(&p.framerate()), "{} fps is offered", p.framerate());
        }
    }

    #[test]
    fn codec_display_order_matches_js() {
        assert_eq!(CODEC_DISPLAY_ORDER.map(codec_label), ["H.264", "AV1", "H.265"]);
    }

    // ---- applying presets (needs a real `AppState`, no network/GStreamer) ----

    use crate::session::Destination;
    use crate::session::test_support::{insert_destination, isolated_config_home, video_params};

    #[test]
    fn dest_edit_apply_preset_sets_fps_and_the_tuning_fields_only() {
        let mut edit = Destination::edit_from(video_params());
        edit.scale_percent = 50;
        edit.bitrate_kbps = 12_345;
        edit.codec = VideoCodec::Av1;
        let untouched = edit;

        for preset in EncoderPreset::ALL {
            edit.apply_preset(preset);
            assert_eq!(edit.tuning(), preset.tuning());
            assert_eq!(edit.framerate, preset.framerate());
            assert_eq!(edit.active_preset(), Some(preset));
            assert_eq!(
                (edit.scale_percent, edit.bitrate_kbps, edit.codec),
                (untouched.scale_percent, untouched.bitrate_kbps, untouched.codec),
                "{} must not touch non-preset fields",
                preset.value()
            );
        }

        // A preset sets the FPS once: changing it by hand keeps the encoder mode and keyframe but unlights the preset.
        edit.apply_preset(EncoderPreset::Gaming);
        edit.framerate = 30;
        assert_eq!(edit.tuning(), EncoderPreset::Gaming.tuning());
        assert_eq!(edit.active_preset(), None);
        edit.framerate = 120;
        assert_eq!(edit.active_preset(), Some(EncoderPreset::Gaming));

        edit.gop_millis = 1234;
        assert_eq!(edit.active_preset(), None, "a hand-tuned buffer highlights no preset");
    }

    #[tokio::test]
    async fn start_stream_settings_follow_the_applied_preset() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        // A fresh app has the Quality encoding but the 30 fps default, so no preset is lit until the FPS matches too.
        assert_eq!(state.start_stream_tuning(), EncoderPreset::Quality.tuning());
        assert_eq!(state.start_stream_preset(), None);

        for preset in EncoderPreset::ALL {
            state.apply_preset_to_start_stream(preset);
            assert_eq!(state.start_stream_tuning(), preset.tuning());
            assert_eq!(state.framerate, preset.framerate());
            assert_eq!(state.start_stream_preset(), Some(preset));
        }

        state.apply_preset_to_start_stream(EncoderPreset::Text);
        assert_eq!(state.gop_millis, 250);
        state.apply_preset_to_start_stream(EncoderPreset::Quality);
        assert_eq!(state.gop_millis, 1000);

        // The rate picked by hand afterwards keeps the tuning and unlights the preset; the bitrate is never a preset's.
        state.bitrate_kbps = 20_000;
        state.apply_preset_to_start_stream(EncoderPreset::Gaming);
        state.framerate = 144;
        assert_eq!(state.start_stream_tuning(), EncoderPreset::Gaming.tuning());
        assert_eq!(state.start_stream_preset(), None);
        assert_eq!(state.bitrate_kbps, 20_000);
    }

    #[tokio::test]
    async fn apply_preset_to_destination_updates_the_row_edit_buffer() {
        let _home = isolated_config_home();
        let mut state = AppState::default();
        let (capture_id, destination_id) = insert_destination(&mut state);
        assert_eq!(state.captures[&capture_id].destinations[&destination_id].edit.active_preset(), Some(EncoderPreset::Quality));

        state.apply_preset_to_destination(destination_id, EncoderPreset::Gaming);

        let edit = state.captures[&capture_id].destinations[&destination_id].edit;
        assert_eq!(edit.active_preset(), Some(EncoderPreset::Gaming));
        assert_eq!((edit.framerate, edit.bitrate_kbps), (120, 8000), "the preset's fps, the bitrate as it was");

        // Unknown destination: no-op.
        state.apply_preset_to_destination(crate::session::DestinationId(999), EncoderPreset::Text);
        assert_eq!(state.captures[&capture_id].destinations[&destination_id].edit.active_preset(), Some(EncoderPreset::Gaming));
    }
}
