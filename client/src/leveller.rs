//! Viewer-side loudness levelling (task #119): a slow automatic gain control (AGC) followed by an
//! instant peak limiter, run on the decoded audio of every watched stream just before the viewer's
//! own volume slider (`decode::run_audio_pipeline`: `... ! F32LE ! [leveller probe] ! volume`).
//!
//! The DSP ([`Leveller`]) is pure: it works on interleaved `f32` buffers, owns no GStreamer type
//! and is unit-tested with synthetic signals. [`install`] hangs it on a pad as a buffer probe that
//! edits the samples in place, so it adds **no latency and no buffer** (it processes each buffer as
//! it passes, with no lookahead).
//!
//! # Why this shape (and not `audioloudnorm` / `rglimiter` / `audiodynamic`)
//!
//! - gst-plugins-rs `audioloudnorm` (EBU R128 normaliser) works with a ~3 s lookahead buffer: far
//!   too much delay for a live stream with a picture, and it is not in every distribution.
//! - `rglimiter` is a soft-knee waveshaper from -6 dBFS (colours everything loud, no real ceiling
//!   guarantee for large overshoots); `audiodynamic` is a static compressor with no notion of a
//!   target loudness. Neither can level.
//! - So: ~60 lines of DSP we control. Every number below is a deliberate trade, documented at its
//!   constant and in `superdoc/features/audio/levelling-and-volume.md`.
//!
//! # The two stages
//!
//! 1. **AGC** (feed-forward, so its detectors see the input and are independent of its own gain).
//!    Aims the short-term RMS at [`TARGET_DB`], gain limited to [`MIN_GAIN_DB`]..[`MAX_GAIN_DB`].
//!    *Slow up, fast down*: the gain rises at most [`RISE_DB_PER_S`] (and only while the signal is
//!    above the [`GATE_DB`] noise gate, so a quiet room is not pumped up), falls at up to
//!    [`FALL_DB_PER_S`] when the 400 ms level is above target, and is pulled down within a few ms
//!    ([`ATTACK_MS`]) when a fast detector sees the level jump more than [`HEADROOM_DB`] above the
//!    target. The AGC starts at 0 dB, so a stream is never boosted before it has been heard.
//! 2. **Peak limiter**, per sample, zero attack: the gain for the current sample is the minimum of
//!    "what is needed to keep this sample under [`CEILING_DB`]" and a released memory of earlier
//!    reductions ([`LIMITER_RELEASE_MS`]). The output therefore **never** exceeds the ceiling, even
//!    for the first sample of a sudden transient, before the AGC reacted. A final clamp removes any
//!    float rounding. Cost: a transient that overshoots is flattened for the few ms the AGC needs
//!    to catch up (the intended trade for the viewer's ears).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use gstreamer as gst;
use gstreamer::prelude::*;

/// Short-term RMS the AGC aims at, dBFS. -18 dBFS RMS is roughly -16 LUFS for typical stereo
/// content (the usual streaming level) and leaves 17 dB for peaks below the ceiling.
pub const TARGET_DB: f64 = -18.0;
/// Most the AGC may add, dB. More would drag background noise up and make the limiter work all the time.
pub const MAX_GAIN_DB: f64 = 12.0;
/// Most the AGC may take away, dB (a game at full blast; the viewer's own slider is below this).
pub const MIN_GAIN_DB: f64 = -20.0;
/// Hard output ceiling, dBFS. Nothing leaves the leveller above this, ever.
pub const CEILING_DB: f64 = -1.0;
/// The gain is not *raised* while the 400 ms level is below this, dBFS (near-silence, room noise,
/// the gap between two tracks): there is nothing to level and the noise floor must stay a floor.
pub const GATE_DB: f64 = -50.0;
/// Fastest the gain rises, dB per second ("slow up"): quiet passages are not pumped up instantly.
pub const RISE_DB_PER_S: f64 = 4.0;
/// Fastest the gain falls while the 400 ms level is above target, dB per second.
pub const FALL_DB_PER_S: f64 = 12.0;
/// How far above [`TARGET_DB`] the fast detector (5 ms attack) may read before the gain is pulled
/// down at once. Lets a drum hit or a plosive through without pumping; a real jump is caught.
pub const HEADROOM_DB: f64 = 4.0;
/// Time constant of that quick pull-down, ms ("fast down", a few ms).
pub const ATTACK_MS: f64 = 5.0;
/// The limiter lets go of a reduction over this long, ms (long enough not to ripple on a bass note).
pub const LIMITER_RELEASE_MS: f64 = 100.0;

/// Fast power detector: rises in 5 ms, falls over 80 ms (a peak-ish RMS, no ripple on low tones).
const FAST_ATTACK_MS: f64 = 5.0;
const FAST_RELEASE_MS: f64 = 80.0;
/// Slow ("short-term") power detector time constant, ms.
const SLOW_MS: f64 = 400.0;
/// The gain is recomputed once per block (1 ms) and ramped linearly across it.
const BLOCK_MS: f64 = 1.0;

fn db_to_lin(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

fn pow_to_db(p: f64) -> f64 {
    10.0 * (p + 1e-12).log10()
}

/// One-pole coefficient for a time constant `ms` at `rate` Hz.
fn coeff(ms: f64, rate: f64) -> f64 {
    1.0 - (-1.0 / (ms * 1e-3 * rate)).exp()
}

/// The levelling DSP for one stream. Feed it interleaved `f32` frames of any chunk size; the result
/// is the same however the stream is chunked.
pub struct Leveller {
    channels: usize,
    block: usize,
    in_block: usize,
    c_fast_up: f64,
    c_fast_down: f64,
    c_slow: f64,
    c_attack: f64,
    c_limit: f64,
    /// Mean-square detectors (linear power of the input).
    p_fast: f64,
    p_slow: f64,
    /// Current AGC gain, dB (the target of the running block), and the per-sample linear ramp.
    gain_db: f64,
    g_lin: f64,
    g_step: f64,
    /// The limiter's remembered reduction (1.0 = none).
    lim: f64,
    ceiling: f64,
}

impl Leveller {
    /// A leveller for `channels` interleaved channels at `rate` Hz, at 0 dB gain.
    pub fn new(rate: u32, channels: usize) -> Self {
        let r = rate.max(8000) as f64;
        Self {
            channels: channels.max(1),
            block: ((BLOCK_MS * 1e-3 * r) as usize).max(1),
            in_block: 0,
            c_fast_up: coeff(FAST_ATTACK_MS, r),
            c_fast_down: coeff(FAST_RELEASE_MS, r),
            c_slow: coeff(SLOW_MS, r),
            c_attack: 1.0 - (-BLOCK_MS / ATTACK_MS).exp(),
            c_limit: coeff(LIMITER_RELEASE_MS, r),
            p_fast: 0.0,
            p_slow: 0.0,
            gain_db: 0.0,
            g_lin: 1.0,
            g_step: 0.0,
            lim: 1.0,
            ceiling: db_to_lin(CEILING_DB),
        }
    }

    /// The AGC gain right now, dB (what the tests and a debug log look at; the limiter is on top).
    pub fn gain_db(&self) -> f64 {
        self.gain_db
    }

    /// Back to the start state (0 dB, detectors empty): used when levelling is switched off, so
    /// switching it on again does not resume with a stale gain.
    pub fn reset(&mut self) {
        self.p_fast = 0.0;
        self.p_slow = 0.0;
        self.gain_db = 0.0;
        self.g_lin = 1.0;
        self.g_step = 0.0;
        self.lim = 1.0;
        self.in_block = 0;
    }

    /// Level `samples` (interleaved, `channels` per frame) in place. A trailing partial frame is
    /// left untouched.
    pub fn process(&mut self, samples: &mut [f32]) {
        let ch = self.channels;
        for frame in samples.chunks_exact_mut(ch) {
            let mut power = 0.0f64;
            for s in frame.iter_mut() {
                if !s.is_finite() {
                    *s = 0.0;
                }
                power += (*s as f64) * (*s as f64);
            }
            power /= ch as f64;
            let cf = if power > self.p_fast { self.c_fast_up } else { self.c_fast_down };
            self.p_fast = (self.p_fast + cf * (power - self.p_fast)).max(1e-18);
            self.p_slow = (self.p_slow + self.c_slow * (power - self.p_slow)).max(1e-18);

            self.g_lin += self.g_step;
            let mut peak = 0.0f64;
            for s in frame.iter_mut() {
                *s = (*s as f64 * self.g_lin) as f32;
                peak = peak.max((*s as f64).abs());
            }
            // Zero-attack limiter: this sample's own need wins at once, the memory only releases.
            let need = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };
            self.lim = (self.lim + (1.0 - self.lim) * self.c_limit).min(need);
            let c = self.ceiling as f32;
            for s in frame.iter_mut() {
                *s = (*s as f64 * self.lim) as f32;
                *s = s.clamp(-c, c);
            }

            self.in_block += 1;
            if self.in_block >= self.block {
                self.in_block = 0;
                self.update_gain();
            }
        }
    }

    /// Once per block: move [`Self::gain_db`] toward what the detectors ask for and set up the ramp.
    fn update_gain(&mut self) {
        let dt = BLOCK_MS * 1e-3;
        let slow_db = pow_to_db(self.p_slow);
        let fast_db = pow_to_db(self.p_fast);
        let want = (TARGET_DB - slow_db).clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        let mut g = self.gain_db;
        if want > g {
            // Slow up, and not at all in near-silence. Tapers into the target.
            if slow_db > GATE_DB {
                g += ((want - g) * 1.5).min(RISE_DB_PER_S) * dt;
            }
        } else {
            g -= ((g - want) * 3.0).min(FALL_DB_PER_S) * dt;
        }
        // Fast down: the quick detector says the output would be well above target.
        let cap = (TARGET_DB + HEADROOM_DB - fast_db).clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        if g > cap {
            g += (cap - g) * self.c_attack;
        }
        self.gain_db = g.clamp(MIN_GAIN_DB, MAX_GAIN_DB);
        let next = db_to_lin(self.gain_db);
        self.g_step = (next - self.g_lin) / self.block as f64;
    }
}

/// The process-wide "Level loudness" switch (Settings > Playback, default on). Every audio
/// pipeline's probe reads it per buffer, so a change applies live to every stream.
pub static ENABLED: AtomicBool = AtomicBool::new(true);

/// Set the process-wide switch ([`ENABLED`]).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Per-pad probe state: the DSP, rebuilt when the caps change, and a scratch buffer.
struct Probe {
    leveller: Option<Leveller>,
    rate: i32,
    channels: i32,
    scratch: Vec<f32>,
}

/// Hang a [`Leveller`] on `pad` (which must carry interleaved `F32LE`): every buffer passing is
/// levelled in place while `enabled` is set, and passes untouched (and the DSP is reset) while not.
pub fn install(pad: &gst::Pad, enabled: &'static AtomicBool) {
    let state = Mutex::new(Probe { leveller: None, rate: 0, channels: 0, scratch: Vec::new() });
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
        if !enabled.load(Ordering::Relaxed) {
            if let Some(l) = st.leveller.as_mut() {
                l.reset();
            }
            return gst::PadProbeReturn::Ok;
        }
        let Some(gst::PadProbeData::Buffer(buffer)) = info.data.as_mut() else { return gst::PadProbeReturn::Ok };
        let Some(s) = pad.current_caps().and_then(|c| c.structure(0).map(|s| (s.get::<i32>("rate").ok(), s.get::<i32>("channels").ok()))) else {
            return gst::PadProbeReturn::Ok;
        };
        let (Some(rate), Some(channels)) = s else { return gst::PadProbeReturn::Ok };
        if rate <= 0 || channels <= 0 {
            return gst::PadProbeReturn::Ok;
        }
        if st.leveller.is_none() || st.rate != rate || st.channels != channels {
            st.leveller = Some(Leveller::new(rate as u32, channels as usize));
            st.rate = rate;
            st.channels = channels;
        }
        let Ok(mut map) = buffer.make_mut().map_writable() else { return gst::PadProbeReturn::Ok };
        let bytes = map.as_mut_slice();
        let n = bytes.len() / 4;
        let Probe { leveller, scratch, .. } = &mut *st;
        scratch.clear();
        scratch.extend(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
        if let Some(l) = leveller.as_mut() {
            l.process(&mut scratch[..n]);
        }
        for (b, v) in bytes.chunks_exact_mut(4).zip(scratch.iter()) {
            b.copy_from_slice(&v.to_le_bytes());
        }
        gst::PadProbeReturn::Ok
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    /// `secs` of a stereo sine at `rms_db` dBFS RMS (440 Hz, same in both channels), appended.
    fn sine(out: &mut Vec<f32>, secs: f64, rms_db: f64, phase: &mut f64) {
        let amp = db_to_lin(rms_db) * std::f64::consts::SQRT_2;
        for _ in 0..(secs * RATE as f64) as usize {
            let v = (amp * (*phase * std::f64::consts::TAU).sin()) as f32;
            *phase = (*phase + 440.0 / RATE as f64).fract();
            out.push(v);
            out.push(v);
        }
    }

    fn rms_db(samples: &[f32]) -> f64 {
        let p = samples.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / samples.len() as f64;
        10.0 * (p + 1e-20).log10()
    }

    fn peak(samples: &[f32]) -> f64 {
        samples.iter().map(|s| s.abs() as f64).fold(0.0, f64::max)
    }

    /// Run `input` through a fresh leveller in chunks of `chunk` samples.
    fn run(l: &mut Leveller, input: &[f32], chunk: usize) -> Vec<f32> {
        let mut out = input.to_vec();
        for c in out.chunks_mut(chunk) {
            l.process(c);
        }
        out
    }

    /// The point of the whole thing: a step from -40 dBFS to 0 dBFS never gets past the ceiling,
    /// not even in the first samples before the AGC reacts, and the AGC then settles near target.
    #[test]
    fn a_sudden_loud_step_never_exceeds_the_ceiling() {
        let mut l = Leveller::new(RATE, 2);
        let mut input = Vec::new();
        let mut ph = 0.0;
        sine(&mut input, 8.0, -40.0, &mut ph); // long enough for the gain to climb to its maximum
        let quiet = input.len();
        sine(&mut input, 0.0, 0.0, &mut ph);
        let amp_full = 1.0f64;
        for _ in 0..(3 * RATE as usize) {
            let v = (amp_full * (ph * std::f64::consts::TAU).sin()) as f32; // 0 dBFS peak
            ph = (ph + 440.0 / RATE as f64).fract();
            input.push(v);
            input.push(v);
        }
        let out = run(&mut l, &input, 960);
        assert!(l.gain_db() < MAX_GAIN_DB, "the step pulled the gain down: {}", l.gain_db());
        let ceiling = db_to_lin(CEILING_DB);
        let p = peak(&out[quiet..]);
        assert!(p <= ceiling + 1e-6, "output peaked at {p} (ceiling {ceiling}) after the step");
        // The very first loud samples are already under the ceiling (gain was at +12 dB there).
        assert!(peak(&out[quiet..quiet + 96]) <= ceiling + 1e-6);
        // After 1 s the AGC has caught up: the loud sine (-3 dBFS RMS) sits near the target.
        let tail = &out[quiet + 2 * RATE as usize * 2..];
        let r = rms_db(tail);
        assert!((r - TARGET_DB).abs() < 3.0, "settled at {r} dBFS RMS, target {TARGET_DB}");
    }

    /// Slow up: a quiet signal is raised at a few dB per second, never instantly.
    #[test]
    fn a_quiet_signal_is_raised_slowly() {
        let mut l = Leveller::new(RATE, 2);
        let mut input = Vec::new();
        let mut ph = 0.0;
        sine(&mut input, 0.5, -35.0, &mut ph);
        run(&mut l, &input, 480);
        let after_half_second = l.gain_db();
        assert!(after_half_second > 0.0, "it does rise");
        assert!(after_half_second <= RISE_DB_PER_S * 0.5 + 0.2, "after 0.5 s: {after_half_second} dB");

        let mut input = Vec::new();
        sine(&mut input, 1.5, -35.0, &mut ph);
        run(&mut l, &input, 480);
        assert!(l.gain_db() <= RISE_DB_PER_S * 2.0 + 0.3, "after 2 s: {} dB", l.gain_db());

        let mut input = Vec::new();
        sine(&mut input, 20.0, -35.0, &mut ph);
        run(&mut l, &input, 480);
        // -35 dBFS wants +17 dB; the maximum is +12.
        assert!((l.gain_db() - MAX_GAIN_DB).abs() < 0.05, "settled at {} dB", l.gain_db());
    }

    /// Near-silence (room noise, a gap) is never boosted: the gate keeps the gain where it was.
    #[test]
    fn silence_and_a_noise_floor_are_not_boosted() {
        let mut l = Leveller::new(RATE, 2);
        let silence = vec![0.0f32; 10 * RATE as usize * 2];
        let out = run(&mut l, &silence, 480);
        assert_eq!(l.gain_db(), 0.0);
        assert!(out.iter().all(|s| *s == 0.0));

        let mut input = Vec::new();
        let mut ph = 0.0;
        sine(&mut input, 10.0, -65.0, &mut ph); // below the -50 dBFS gate
        run(&mut l, &input, 480);
        assert_eq!(l.gain_db(), 0.0, "a -65 dBFS noise floor must stay a floor");
    }

    /// Fast down: sustained loud material is pulled to the target quickly, not at the slow rate.
    #[test]
    fn loud_material_is_pulled_down_fast() {
        let mut l = Leveller::new(RATE, 2);
        let mut input = Vec::new();
        let mut ph = 0.0;
        sine(&mut input, 0.1, -3.0, &mut ph); // loud from the first moment
        let out = run(&mut l, &input, 480);
        assert!(l.gain_db() < -5.0, "after 100 ms the gain is already {} dB", l.gain_db());
        assert!(peak(&out) <= db_to_lin(CEILING_DB) + 1e-6);
        let mut input = Vec::new();
        sine(&mut input, 3.0, -3.0, &mut ph);
        let out = run(&mut l, &input, 480);
        let r = rms_db(&out[out.len() - RATE as usize * 2..]);
        assert!((r - TARGET_DB).abs() < 2.0, "settled at {r} dBFS RMS");
    }

    /// Whatever the input does, the output stays under the ceiling and finite (random bursts at
    /// wildly different levels, plus NaN/inf/huge samples).
    #[test]
    fn output_never_exceeds_the_ceiling_for_any_input() {
        let mut l = Leveller::new(RATE, 2);
        let mut seed = 0x2545F491u32;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32) * 2.0 - 1.0
        };
        let ceiling = db_to_lin(CEILING_DB);
        for burst in 0..60 {
            let level = [0.001f32, 0.05, 1.0, 40.0][burst % 4];
            let n = 2 * (200 + (burst * 37) % 3000);
            let mut buf: Vec<f32> = (0..n).map(|_| rnd() * level).collect();
            if burst == 7 {
                buf[3] = f32::NAN;
                buf[4] = f32::INFINITY;
                buf[5] = f32::NEG_INFINITY;
            }
            l.process(&mut buf);
            assert!(buf.iter().all(|s| s.is_finite() && (s.abs() as f64) <= ceiling + 1e-6), "burst {burst} level {level}");
        }
    }

    /// The result does not depend on how the stream is cut into buffers.
    #[test]
    fn chunking_does_not_change_the_result() {
        let mut input = Vec::new();
        let mut ph = 0.0;
        sine(&mut input, 1.0, -30.0, &mut ph);
        sine(&mut input, 1.0, -6.0, &mut ph);
        let a = run(&mut Leveller::new(RATE, 2), &input, 960);
        let b = run(&mut Leveller::new(RATE, 2), &input, 2 * 7);
        assert_eq!(a, b);
    }

    /// A mono stream works the same (opus can deliver 1 channel).
    #[test]
    fn mono_is_supported() {
        let mut l = Leveller::new(RATE, 1);
        let mut buf: Vec<f32> = (0..RATE as usize).map(|i| (i as f32 * 0.06).sin() * 0.9).collect();
        l.process(&mut buf);
        assert!(peak(&buf) <= db_to_lin(CEILING_DB) + 1e-6);
    }

    /// The real probe on a real pad: `audiotestsrc ! F32LE ! (probe) ! appsink`, no audio device.
    /// A full-scale sine comes out under the ceiling with levelling on, and untouched with it off.
    #[test]
    fn the_probe_levels_buffers_in_a_pipeline_and_the_switch_turns_it_off() {
        use gstreamer_app as gst_app;
        gst::init().unwrap();
        static ON: AtomicBool = AtomicBool::new(true);
        static OFF: AtomicBool = AtomicBool::new(false);
        let peak_through = |flag: &'static AtomicBool| -> f64 {
            let p = gst::parse::launch(
                "audiotestsrc num-buffers=20 samplesperbuffer=480 wave=sine freq=440 volume=1.0 \
                 ! audio/x-raw,format=F32LE,rate=48000,channels=2,layout=interleaved \
                 ! identity name=tap ! appsink name=sink sync=false",
            )
            .unwrap()
            .downcast::<gst::Pipeline>()
            .unwrap();
            install(&p.by_name("tap").unwrap().static_pad("sink").unwrap(), flag);
            let sink = p.by_name("sink").unwrap().downcast::<gst_app::AppSink>().unwrap();
            p.set_state(gst::State::Playing).unwrap();
            let mut peak = 0.0f64;
            for _ in 0..20 {
                let s = sink.try_pull_sample(gst::ClockTime::from_seconds(5)).expect("sample");
                let map = s.buffer().unwrap().map_readable().unwrap();
                for b in map.as_slice().chunks_exact(4) {
                    peak = peak.max(f32::from_le_bytes([b[0], b[1], b[2], b[3]]).abs() as f64);
                }
            }
            let _ = p.set_state(gst::State::Null);
            peak
        };
        let on = peak_through(&ON);
        assert!(on <= db_to_lin(CEILING_DB) + 1e-6 && on > 0.1, "levelled peak {on}");
        let off = peak_through(&OFF);
        assert!(off > 0.99, "untouched peak {off}");
    }
}
