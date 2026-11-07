//! Sender-side audio boost (task #114, trimmed in #119): the gain range and its conversions.
//!
//! Everything here is pure (no GStreamer, no PipeWire), so it is unit-tested directly. The
//! pipeline side (`volume name=gain` + `rglimiter`) is in `audio::build_pipeline`; the persisted
//! value is `rooms::AppSettings::audio_gain`.
//!
//! `Why a sender-side gain:` a source that is quiet at the sharer can be lifted before Opus
//! encoding in float, where nothing clips until the limiter says so. It is capped at 200 % since
//! #119: the old 10 s calibration (up to 800 %) is gone, and the *viewer* now levels loudness
//! itself ([`crate::leveller`]), so this slider is only a manual trim for a source that is far too
//! quiet or too loud. See `superdoc/features/audio/levelling-and-volume.md`.

/// Quietest boost the setting accepts: -6 dB. Below 1x exists for a source that is too loud.
pub const MIN_GAIN: f64 = 0.5;
/// Loudest boost the setting accepts: +6 dB (200 %, #119). More only lifts the noise floor; the
/// viewers' leveller does the rest.
pub const MAX_GAIN: f64 = 2.0;
/// The default: no change (and then the limiter is bypassed, so default audio is bit-for-bit
/// what it was before the setting existed).
pub const DEFAULT_GAIN: f64 = 1.0;

/// `gain` clamped to [`MIN_GAIN`], [`MAX_GAIN`] and rounded to 0.01, so the stored value reads
/// "1.5" in `settings.toml` and not "1.5000000476837". `NaN` becomes [`DEFAULT_GAIN`].
pub fn clamp_gain(gain: f64) -> f64 {
    if gain.is_nan() {
        return DEFAULT_GAIN;
    }
    ((gain.clamp(MIN_GAIN, MAX_GAIN)) * 100.0).round() / 100.0
}

/// Multiplier -> dB (`20 log10`).
pub fn gain_to_db(gain: f64) -> f64 {
    20.0 * gain.max(1e-6).log10()
}

/// dB -> multiplier.
pub fn db_to_gain(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

/// The slider's range in dB (the slider works in dB so each doubling is the same distance and
/// 100 % sits in the middle: -6 dB to +6 dB).
pub fn gain_db_range() -> std::ops::RangeInclusive<f32> {
    gain_to_db(MIN_GAIN) as f32..=gain_to_db(MAX_GAIN) as f32
}

/// Whether the limiter after the gain stage is needed. At or below 1x a signal that fits
/// stays within full scale, so the limiter is bypassed and plain audio is untouched
/// (`rglimiter` is a waveshaper from -6 dBFS up: it would colour even an unboosted loud signal).
pub fn limiter_needed(gain: f64) -> bool {
    gain > 1.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_rounds_and_limits_the_setting() {
        assert_eq!(clamp_gain(1.0), 1.0);
        assert_eq!(clamp_gain(0.1), MIN_GAIN);
        assert_eq!(clamp_gain(100.0), MAX_GAIN);
        assert_eq!(clamp_gain(1.2000000476837), 1.2);
        assert_eq!(clamp_gain(f64::NAN), DEFAULT_GAIN);
        assert_eq!(clamp_gain(f64::INFINITY), MAX_GAIN);
        assert_eq!(MAX_GAIN, 2.0, "the boost is capped at 200 % (#119)");
    }

    #[test]
    fn db_conversions_round_trip_and_the_range_is_the_gain_range() {
        assert!((db_to_gain(6.0206) - 2.0).abs() < 1e-3);
        assert!((gain_to_db(db_to_gain(-4.5)) + 4.5).abs() < 1e-9);
        let r = gain_db_range();
        assert!((db_to_gain(*r.start() as f64) - MIN_GAIN).abs() < 1e-4);
        assert!((db_to_gain(*r.end() as f64) - MAX_GAIN).abs() < 1e-4);
        // The slider's end and the clamp agree, so dragging to the end does not "change" the setting.
        assert_eq!(clamp_gain(db_to_gain(*r.end() as f64)), MAX_GAIN);
    }

    #[test]
    fn the_limiter_is_only_in_the_signal_path_when_boosting() {
        assert!(!limiter_needed(1.0));
        assert!(!limiter_needed(0.5));
        assert!(limiter_needed(1.01));
    }
}
