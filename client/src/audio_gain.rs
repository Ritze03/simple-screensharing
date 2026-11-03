//! Sender-side audio boost (task #114): the gain range, the loudness measurement and the
//! calibration that turns "10 s of the source playing" into a multiplier.
//!
//! Everything here is pure (no GStreamer, no PipeWire), so it is unit-tested with synthetic
//! levels; the real source is `audio::AudioLevelTapHandle`, which feeds a [`LoudnessMeter`]
//! from its `level` element messages (see `audio::start_calibration_tap`). The pipeline side
//! (`volume name=gain` + `rglimiter`) is in `audio::build_pipeline`; the persisted value is
//! `rooms::AppSettings::audio_gain`.
//!
//! `Why a sender-side gain and a calibration:` a viewer can boost a sharer to 500 %
//! (#96), but a quiet source boosted that far on the viewer's side just saturates its peaks and
//! clips, and every viewer has to do it again. Fixing the level before Opus encoding gives every
//! viewer a proper picture of the sound. See `superdoc/features/audio-sharing.md`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Quietest boost the setting accepts: -6 dB. Below 1x exists for a source that is too loud.
pub const MIN_GAIN: f64 = 0.5;
/// Loudest boost the setting accepts: +18 dB. More would only lift the noise floor.
pub const MAX_GAIN: f64 = 8.0;
/// The default: no change (and then the limiter is bypassed, so default audio is bit-for-bit
/// what it was before the setting existed).
pub const DEFAULT_GAIN: f64 = 1.0;

/// How long the calibration listens.
pub const CALIBRATION_DURATION: Duration = Duration::from_secs(10);
/// The loudness the calibration aims the RMS at, dBFS. -18 is the usual broadcast/streaming
/// "comfortably loud with headroom" level: music peaks 10-14 dB above its RMS, so they land
/// around -4..-8 dBFS, where the limiter only rounds them.
pub const TARGET_RMS_DB: f64 = -18.0;
/// The highest the *peaks* may land after the boost, dBFS. Above 0 dBFS the limiter has to
/// flatten them; +3 dB of overload is the most it does without audible distortion. This stops a
/// very dynamic source (speech with the odd loud pop) from being boosted by its quiet RMS into a
/// wall of limiting.
pub const PEAK_CEILING_DB: f64 = 3.0;
/// A 100 ms window quieter than this (RMS, dBFS) is silence for the calibration. The same floor
/// the level meters use (`audio::LEVEL_FLOOR_DB`); digital silence reads as -700 dB or so.
pub const SILENCE_GATE_DB: f64 = -60.0;
/// The calibration needs at least this many audible windows (~1 s) to trust a measurement; a
/// click or a notification ping is not a measurement.
pub const MIN_AUDIBLE_WINDOWS: u32 = 10;

/// `gain` clamped to [`MIN_GAIN`], [`MAX_GAIN`] and rounded to 0.01, so the stored value reads
/// "3.2" in `settings.toml` and not "3.2000000476837". `NaN` becomes [`DEFAULT_GAIN`].
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

/// The slider's range in dB (the settings slider works in dB so 100 % is not squeezed into the
/// first few pixels: 100 % to 200 % is 6 dB, 400 % to 800 % is 6 dB).
pub fn gain_db_range() -> std::ops::RangeInclusive<f32> {
    gain_to_db(MIN_GAIN) as f32..=gain_to_db(MAX_GAIN) as f32
}

/// Whether the limiter after the gain stage is needed. At or below 1x a signal that fits
/// stays within full scale, so the limiter is bypassed and plain audio is untouched
/// (`rglimiter` is a waveshaper from -6 dBFS up: it would colour even an unboosted loud signal).
pub fn limiter_needed(gain: f64) -> bool {
    gain > 1.0
}

/// The loudness of a stretch of audio, from [`LoudnessMeter::measurement`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Measurement {
    /// RMS over the audible windows (mean power, so loud passages count for what they are), dBFS.
    pub rms_db: f64,
    /// Highest sample peak of any window, dBFS.
    pub peak_db: f64,
    /// Windows that were louder than [`SILENCE_GATE_DB`].
    pub audible_windows: u32,
    /// All windows seen.
    pub total_windows: u32,
}

/// Accumulates the `level` element's 100 ms windows (RMS and peak, dBFS, loudest channel).
///
/// O(1) state: it keeps the sum of the audible windows' power, the peak and counters, never the
/// windows themselves, so a meter tap that runs for hours costs nothing.
#[derive(Debug, Default, Clone)]
pub struct LoudnessMeter {
    audible: u32,
    total: u32,
    power_sum: f64,
    peak_db: f64,
}

/// A meter shared between the tap's GStreamer bus pump (writer) and the calibration (reader).
/// A `std` mutex is right: held for a few arithmetic operations, never across an await.
pub type SharedLoudness = Arc<Mutex<LoudnessMeter>>;

impl LoudnessMeter {
    /// Fold in one window: `rms_db` and `peak_db` are the loudest channel's values in dBFS.
    pub fn push(&mut self, rms_db: f64, peak_db: f64) {
        self.total += 1;
        if rms_db.is_finite() && rms_db >= SILENCE_GATE_DB {
            if self.audible == 0 || peak_db > self.peak_db {
                self.peak_db = peak_db;
            }
            self.audible += 1;
            self.power_sum += 10f64.powf(rms_db / 10.0);
        }
    }

    /// Forget everything (the calibration starts counting when it starts listening, not when
    /// the tap came up).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// The measurement, or `None` if too little was audible ([`MIN_AUDIBLE_WINDOWS`]).
    pub fn measurement(&self) -> Option<Measurement> {
        if self.audible < MIN_AUDIBLE_WINDOWS {
            return None;
        }
        Some(Measurement {
            rms_db: 10.0 * (self.power_sum / self.audible as f64).log10(),
            peak_db: self.peak_db,
            audible_windows: self.audible,
            total_windows: self.total,
        })
    }
}

/// How far the unclamped result was from what the range allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GainLimit {
    /// Inside the range.
    None,
    /// Even [`MAX_GAIN`] does not reach the target: the source is very quiet.
    Max,
    /// The source is louder than the target and was lowered to [`MIN_GAIN`].
    Min,
}

/// The multiplier for a measurement: the boost that puts the RMS at [`TARGET_RMS_DB`], unless
/// that would push the peaks above [`PEAK_CEILING_DB`] (then the peaks decide), clamped.
/// Absolute, not relative to the current setting: the tap measures the source *before* the gain.
pub fn gain_for(m: &Measurement) -> (f64, GainLimit) {
    let db = (TARGET_RMS_DB - m.rms_db).min(PEAK_CEILING_DB - m.peak_db);
    let raw = db_to_gain(db);
    let gain = clamp_gain(raw);
    let limit = if raw > MAX_GAIN + 1e-9 {
        GainLimit::Max
    } else if raw < MIN_GAIN - 1e-9 {
        GainLimit::Min
    } else {
        GainLimit::None
    };
    (gain, limit)
}

/// What a finished calibration came to.
#[derive(Debug, Clone, PartialEq)]
pub enum CalibrationOutcome {
    /// Measured; `gain` is the new setting.
    Set { gain: f64, limit: GainLimit, measurement: Measurement },
    /// Nothing audible in the 10 s: the setting is left alone.
    Silent,
    /// The tap could not be started (source gone, no PipeWire node, never linked).
    Failed(String),
}

impl CalibrationOutcome {
    /// The outcome of listening to `meter`.
    pub fn from_meter(meter: &LoudnessMeter) -> Self {
        match meter.measurement() {
            Some(m) => {
                let (gain, limit) = gain_for(&m);
                CalibrationOutcome::Set { gain, limit, measurement: m }
            }
            None => CalibrationOutcome::Silent,
        }
    }
}

/// The result of building the calibration tap (the source's level tap, link-checked).
pub type TapResult = Result<crate::audio::AudioLevelTapHandle, String>;

enum Phase {
    /// The tap is being built (`pw-dump`, `pipewiresrc` coming up, link check).
    Starting(tokio::sync::oneshot::Receiver<TapResult>),
    /// Listening since `started`.
    Listening { _tap: Option<crate::audio::AudioLevelTapHandle>, meter: SharedLoudness, started: Instant },
}

/// A calibration in flight: for [`CALIBRATION_DURATION`] it collects the source's levels, then
/// [`CalibrationOutcome`]. The clock is passed in (`now`) and the meter can be fed by anyone, so
/// a test drives a whole calibration with synthetic levels and a fake clock.
pub struct Calibration {
    /// The audio source name being measured.
    pub source: String,
    phase: Phase,
}

impl Calibration {
    /// A calibration waiting for its tap.
    pub fn starting(source: String, rx: tokio::sync::oneshot::Receiver<TapResult>) -> Self {
        Self { source, phase: Phase::Starting(rx) }
    }

    /// A calibration already listening to `meter` since `now` (the tap, if any, is kept alive
    /// for as long as this lives and stopped when it is dropped).
    pub fn listening(source: String, tap: Option<crate::audio::AudioLevelTapHandle>, meter: SharedLoudness, now: Instant) -> Self {
        meter.lock().unwrap_or_else(|p| p.into_inner()).reset();
        Self { source, phase: Phase::Listening { _tap: tap, meter, started: now } }
    }

    /// Advance to `now`. `Some` exactly once, when it is over (the caller then drops the
    /// calibration, which stops the tap).
    pub fn poll(&mut self, now: Instant) -> Option<CalibrationOutcome> {
        match &mut self.phase {
            Phase::Starting(rx) => match rx.try_recv() {
                Ok(Ok(tap)) => {
                    let meter = tap.loudness();
                    *self = Self::listening(std::mem::take(&mut self.source), Some(tap), meter, now);
                    None
                }
                Ok(Err(e)) => Some(CalibrationOutcome::Failed(e)),
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    Some(CalibrationOutcome::Failed("the audio tap task ended".to_string()))
                }
            },
            Phase::Listening { meter, started, .. } => {
                if now.saturating_duration_since(*started) < CALIBRATION_DURATION {
                    return None;
                }
                let m = meter.lock().unwrap_or_else(|p| p.into_inner());
                Some(CalibrationOutcome::from_meter(&m))
            }
        }
    }

    /// Seconds still to listen (whole, rounded up; 10 down to 1), or `None` while the tap is
    /// still starting.
    pub fn remaining_secs(&self, now: Instant) -> Option<u64> {
        match &self.phase {
            Phase::Starting(_) => None,
            Phase::Listening { started, .. } => {
                let left = CALIBRATION_DURATION.saturating_sub(now.saturating_duration_since(*started));
                Some(left.as_secs_f64().ceil() as u64)
            }
        }
    }

    /// The listening meter, for a test (or a live level display) to read or feed.
    pub fn meter(&self) -> Option<SharedLoudness> {
        match &self.phase {
            Phase::Starting(_) => None,
            Phase::Listening { meter, .. } => Some(meter.clone()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fed(windows: &[(f64, f64)]) -> LoudnessMeter {
        let mut m = LoudnessMeter::default();
        for (rms, peak) in windows {
            m.push(*rms, *peak);
        }
        m
    }

    /// `n` identical windows.
    fn steady(n: usize, rms: f64, peak: f64) -> Vec<(f64, f64)> {
        vec![(rms, peak); n]
    }

    #[test]
    fn clamp_rounds_and_limits_the_setting() {
        assert_eq!(clamp_gain(1.0), 1.0);
        assert_eq!(clamp_gain(0.1), MIN_GAIN);
        assert_eq!(clamp_gain(100.0), MAX_GAIN);
        assert_eq!(clamp_gain(3.2000000476837), 3.2);
        assert_eq!(clamp_gain(f64::NAN), DEFAULT_GAIN);
        assert_eq!(clamp_gain(f64::INFINITY), MAX_GAIN);
    }

    #[test]
    fn db_conversions_round_trip_and_the_range_is_the_gain_range() {
        assert!((db_to_gain(6.0206) - 2.0).abs() < 1e-3);
        assert!((gain_to_db(db_to_gain(-4.5)) + 4.5).abs() < 1e-9);
        let r = gain_db_range();
        assert!((db_to_gain(*r.start() as f64) - MIN_GAIN).abs() < 1e-4);
        assert!((db_to_gain(*r.end() as f64) - MAX_GAIN).abs() < 1e-4);
        // The calibration's clamp and the slider's end agree, so a calibration at the maximum
        // does not move the slider's knob (which would then "change" the setting).
        assert_eq!(clamp_gain(db_to_gain(*r.end() as f64)), MAX_GAIN);
    }

    #[test]
    fn the_limiter_is_only_in_the_signal_path_when_boosting() {
        assert!(!limiter_needed(1.0));
        assert!(!limiter_needed(0.5));
        assert!(limiter_needed(1.01));
    }

    #[test]
    fn silence_and_blips_are_not_a_measurement() {
        assert_eq!(fed(&steady(100, -700.0, -700.0)).measurement(), None, "digital silence");
        assert_eq!(fed(&steady(100, f64::NEG_INFINITY, f64::NEG_INFINITY)).measurement(), None);
        assert_eq!(fed(&steady(100, -61.0, -50.0)).measurement(), None, "below the gate");
        let mut blip = steady(95, -700.0, -700.0);
        blip.extend(steady(5, -20.0, -6.0));
        assert_eq!(fed(&blip).measurement(), None, "0.5 s of sound is a ping, not music");
        assert!(fed(&steady(10, -30.0, -20.0)).measurement().is_some(), "1 s is enough");
    }

    #[test]
    fn steady_audio_measures_what_it_is() {
        let m = fed(&steady(100, -30.0, -18.0)).measurement().unwrap();
        assert!((m.rms_db + 30.0).abs() < 1e-9);
        assert!((m.peak_db + 18.0).abs() < 1e-9);
        assert_eq!((m.audible_windows, m.total_windows), (100, 100));
    }

    #[test]
    fn silent_stretches_are_ignored_not_averaged_in() {
        // Half the time nothing plays: the loudness is that of the playing half.
        let mut w = steady(50, -700.0, -700.0);
        w.extend(steady(50, -30.0, -20.0));
        let m = fed(&w).measurement().unwrap();
        assert!((m.rms_db + 30.0).abs() < 1e-9, "got {}", m.rms_db);
        assert_eq!((m.audible_windows, m.total_windows), (50, 100));
    }

    #[test]
    fn loud_passages_count_in_power_not_in_decibels() {
        // One loud window among nine quiet ones: mean power is dominated by the loud one.
        let mut w = steady(9, -40.0, -30.0);
        w.push((-20.0, -10.0));
        let m = fed(&w).measurement().unwrap();
        let expected = 10.0 * ((9.0 * 1e-4 + 1e-2) / 10.0f64).log10();
        assert!((m.rms_db - expected).abs() < 1e-9);
        assert!(m.rms_db > -30.0 && m.rms_db < -20.0);
        assert!((m.peak_db + 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_quiet_source_is_boosted_to_the_target() {
        // -30 dBFS RMS with 12 dB crest: wants +12 dB (peak -18 -> -6, well under the ceiling).
        let m = fed(&steady(100, -30.0, -18.0)).measurement().unwrap();
        let (g, limit) = gain_for(&m);
        assert_eq!(limit, GainLimit::None);
        assert!((gain_to_db(g) - 12.0).abs() < 0.05, "got {} dB", gain_to_db(g));
        assert_eq!(g, 3.98);
    }

    #[test]
    fn peaky_audio_is_held_back_by_its_peaks() {
        // Speech: -35 dBFS RMS, peaks at -10: the RMS wants +17 dB, the peaks allow only +13.
        let m = fed(&steady(100, -35.0, -10.0)).measurement().unwrap();
        let (g, limit) = gain_for(&m);
        assert_eq!(limit, GainLimit::None);
        assert!((gain_to_db(g) - 13.0).abs() < 0.05, "got {} dB", gain_to_db(g));
    }

    #[test]
    fn a_very_quiet_source_hits_the_maximum_and_says_so() {
        let m = fed(&steady(100, -50.0, -40.0)).measurement().unwrap();
        assert_eq!(gain_for(&m), (MAX_GAIN, GainLimit::Max));
    }

    #[test]
    fn a_loud_source_is_lowered_down_to_the_minimum() {
        let m = fed(&steady(100, -8.0, -1.0)).measurement().unwrap();
        // -18 - -8 = -10 dB = 0.316: below the -6 dB floor.
        assert_eq!(gain_for(&m), (MIN_GAIN, GainLimit::Min));
        // A moderately loud one lands inside the range.
        let m = fed(&steady(100, -14.0, -6.0)).measurement().unwrap();
        let (g, limit) = gain_for(&m);
        assert_eq!(limit, GainLimit::None);
        assert!((gain_to_db(g) + 4.0).abs() < 0.05);
    }

    #[test]
    fn the_result_is_a_valid_setting() {
        for rms in [-59.0, -45.0, -30.0, -18.0, -6.0] {
            for crest in [3.0, 12.0, 30.0] {
                let m = fed(&steady(20, rms, (rms + crest).min(0.0))).measurement().unwrap();
                let (g, _) = gain_for(&m);
                assert_eq!(g, clamp_gain(g), "rms {rms} crest {crest}");
            }
        }
    }

    #[test]
    fn reset_forgets_everything() {
        let mut m = fed(&steady(100, -30.0, -20.0));
        m.reset();
        assert_eq!(m.measurement(), None);
        assert_eq!(m.total, 0);
    }

    // ---- the calibration flow, driven with synthetic levels and a fake clock ----

    fn shared() -> SharedLoudness {
        Arc::new(Mutex::new(LoudnessMeter::default()))
    }

    fn feed(meter: &SharedLoudness, windows: &[(f64, f64)]) {
        let mut m = meter.lock().unwrap();
        for (rms, peak) in windows {
            m.push(*rms, *peak);
        }
    }

    #[test]
    fn a_calibration_listens_for_ten_seconds_then_sets_the_gain() {
        let t0 = Instant::now();
        let meter = shared();
        let mut cal = Calibration::listening("Spotify".into(), None, meter.clone(), t0);
        assert_eq!(cal.remaining_secs(t0), Some(10));
        feed(&meter, &steady(100, -30.0, -18.0));
        assert_eq!(cal.poll(t0 + Duration::from_millis(9_900)), None, "not over before 10 s");
        assert_eq!(cal.remaining_secs(t0 + Duration::from_millis(9_900)), Some(1));
        assert_eq!(cal.remaining_secs(t0 + Duration::from_millis(4_000)), Some(6));
        match cal.poll(t0 + CALIBRATION_DURATION) {
            Some(CalibrationOutcome::Set { gain, limit, .. }) => {
                assert_eq!(gain, 3.98);
                assert_eq!(limit, GainLimit::None);
            }
            other => panic!("expected a measured gain, got {other:?}"),
        }
        assert_eq!(cal.remaining_secs(t0 + Duration::from_secs(30)), Some(0));
    }

    #[test]
    fn a_calibration_that_heard_nothing_changes_nothing() {
        let t0 = Instant::now();
        let meter = shared();
        let mut cal = Calibration::listening("Spotify".into(), None, meter.clone(), t0);
        feed(&meter, &steady(100, -700.0, -700.0));
        assert_eq!(cal.poll(t0 + CALIBRATION_DURATION), Some(CalibrationOutcome::Silent));
    }

    #[test]
    fn listening_starts_from_zero_even_if_the_meter_already_heard_something() {
        let t0 = Instant::now();
        let meter = shared();
        feed(&meter, &steady(100, -10.0, -1.0));
        let mut cal = Calibration::listening("x".into(), None, meter.clone(), t0);
        feed(&meter, &steady(100, -30.0, -18.0));
        let Some(CalibrationOutcome::Set { measurement, .. }) = cal.poll(t0 + CALIBRATION_DURATION) else { panic!() };
        assert!((measurement.rms_db + 30.0).abs() < 1e-9);
        assert_eq!(measurement.total_windows, 100);
    }

    #[tokio::test]
    async fn a_calibration_waits_for_its_tap_and_reports_a_failed_one() {
        let t0 = Instant::now();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut cal = Calibration::starting("Spotify".into(), rx);
        assert_eq!(cal.poll(t0), None);
        assert_eq!(cal.remaining_secs(t0), None, "no countdown while the tap starts");
        tx.send(Err("never linked".to_string())).ok();
        assert_eq!(cal.poll(t0), Some(CalibrationOutcome::Failed("never linked".into())));

        let (tx, rx) = tokio::sync::oneshot::channel::<TapResult>();
        let mut cal = Calibration::starting("Spotify".into(), rx);
        drop(tx);
        assert!(matches!(cal.poll(t0), Some(CalibrationOutcome::Failed(_))));
    }
}
