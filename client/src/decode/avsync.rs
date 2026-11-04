//! Viewer-side audio/video sync: audio is delayed so it lands with the matching picture, and the
//! delay follows the video's latency as it changes.
//!
//! Full write-up with the `Why:` notes: `superdoc/features/audio-sharing.md`.
//!
//! # The model
//!
//! Every media frame carries a capture timestamp (`pts`, wire spec section 6.4). Since 2026-11-01
//! all of one *sender process's* streams stamp it from **one clock**: microseconds since
//! [`sender_clock_epoch`] (video: `capture.rs` destination epoch; audio: `audio.rs`). So for a
//! video frame and an audio packet captured at the same instant, `pts` is equal.
//!
//! The viewer cannot see that clock's offset to its own, but it does not need to. For each stream it
//! tracks the *latency floor* `arrival - pts` (a windowed minimum, [`LatencyFloor`]). The unknown
//! clock offset is identical in both floors, so their difference is the real latency difference:
//!
//! ```text
//! delta = floor_video - floor_audio        // how much later than audio the picture arrives
//! extra audio delay = clamp(delta - NOMINAL_AUDIO_LEAD, 0, MAX_AUDIO_SYNC_DELAY)
//! ```
//!
//! The video floor includes everything the sender did to the frame (x264 lookahead, encode) because
//! `pts` is the *capture* time, not the encode-output time. That is the whole point: the x264
//! "Best quality" lookahead (about 0.7 to 1 s) is invisible to the viewer except through this number.
//!
//! The two sides find each other through [`AvSync::for_peer`], keyed by the sender's `ConnId`, so
//! `StreamDecoder` and `AudioDecoder` (created independently by `session.rs`) need no wiring.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Instant;

use crate::protocol::ConnId;

/// Process-wide sender clock origin. Every wire PTS this process sends (all streams, all rooms)
/// is `now - sender_clock_epoch()` in microseconds at capture time, so a viewer may compare audio
/// and video PTS of one sender. Fixed at first use and never moved.
pub fn sender_clock_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Microseconds on the sender clock at `now`.
pub fn sender_clock_us(now: Instant) -> u64 {
    now.saturating_duration_since(sender_clock_epoch()).as_micros() as u64
}

/// Largest extra delay ever applied to audio (us). Past this, audio leads the picture rather than
/// being delayed more: 2 s of lag between hearing and doing (voice chat over the share) is worse
/// than a lip-sync miss on an extreme encoder setting.
pub const MAX_AUDIO_SYNC_DELAY_US: i64 = 2_000_000;

/// The playout lead the audio path adds on its own (`decode.rs` `AUDIO_TARGET_LEAD_US`); already
/// part of the audio's delay, so it is subtracted from what must be added.
pub const NOMINAL_AUDIO_LEAD_US: i64 = 100_000;

/// A measured `delta` outside `[PLAUSIBLE_MIN, PLAUSIBLE_MAX]` means the two PTS are not on one
/// clock (an older sender, which stamps audio from its audio-pipeline start): apply no delay
/// rather than a random one.
pub const PLAUSIBLE_DELTA_MIN_US: i64 = -1_000_000;
pub const PLAUSIBLE_DELTA_MAX_US: i64 = MAX_AUDIO_SYNC_DELAY_US + 1_000_000;

/// Latency-floor window. Short enough that a rise in video latency (settings change, encoder
/// rebuild) is followed within about this long; a fall is followed at once (a minimum).
const FLOOR_WINDOW_US: i64 = 3_000_000;
const FLOOR_BUCKET_US: i64 = 250_000;

/// The applied delay only changes when the target is at least this far from it. Matches
/// `opusdec`'s 40 ms timestamp tolerance plus margin: smaller corrections would be ignored by the
/// decoder anyway, and chasing estimator noise would turn into audible seams.
pub const DELAY_DEADBAND_US: i64 = 60_000;

/// Minimum time between two applied changes, so a settling estimate makes one step, not a staircase.
pub const DELAY_MIN_INTERVAL_US: i64 = 500_000;

/// Windowed minimum of `arrival - pts` (microseconds, signed: the clock offset is unknown).
///
/// `Why a minimum, not a mean or median:` the floor is "the path when nothing is queued"; jitter
/// only ever adds. The same floor logic as the browser (`docs/js/core/avsync.js`).
/// The last value is **held** when samples stop (idle screen, silent source): no information is
/// not a reason to move the delay.
#[derive(Debug, Default, Clone)]
pub struct LatencyFloor {
    buckets: VecDeque<(i64, i64)>,
    held: Option<i64>,
}

impl LatencyFloor {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one frame/packet that arrived at `now_us` (any monotonic local clock, microseconds).
    pub fn observe(&mut self, now_us: i64, pts_us: u64) {
        let d = now_us.saturating_sub(pts_us.min(i64::MAX as u64) as i64);
        let b = now_us.div_euclid(FLOOR_BUCKET_US);
        let keep_from = b - FLOOR_WINDOW_US / FLOOR_BUCKET_US;
        while self.buckets.front().is_some_and(|&(idx, _)| idx <= keep_from) {
            self.buckets.pop_front();
        }
        match self.buckets.back_mut() {
            Some((idx, min)) if *idx == b => *min = (*min).min(d),
            _ => self.buckets.push_back((b, d)),
        }
        self.held = self.buckets.iter().map(|&(_, m)| m).min();
    }

    /// The current floor, or the last one if samples stopped; `None` before the first sample.
    pub fn floor(&self) -> Option<i64> {
        self.held
    }
}

/// Extra audio delay (us) that lines audio up with the picture, from the two latency floors.
/// Pure. `video_alive` is false when no video decoder of this sender exists (audio-only listen,
/// or the video watch ended): then there is no picture to match.
pub fn target_delay_us(video_floor: Option<i64>, audio_floor: Option<i64>, video_alive: bool) -> i64 {
    let (true, Some(v), Some(a)) = (video_alive, video_floor, audio_floor) else { return 0 };
    let delta = v.saturating_sub(a);
    if !(PLAUSIBLE_DELTA_MIN_US..=PLAUSIBLE_DELTA_MAX_US).contains(&delta) {
        return 0;
    }
    (delta - NOMINAL_AUDIO_LEAD_US).clamp(0, MAX_AUDIO_SYNC_DELAY_US)
}

/// Turns the noisy target into the delay actually applied: deadband plus a minimum interval.
/// Owned by the audio pipeline thread.
#[derive(Debug, Default, Clone)]
pub struct DelayController {
    applied_us: i64,
    last_change_us: Option<i64>,
}

impl DelayController {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn applied_us(&self) -> i64 {
        self.applied_us
    }

    /// Offer a new target at `now_us`; returns the delay to use from now on.
    pub fn update(&mut self, target_us: i64, now_us: i64) -> i64 {
        let target = target_us.clamp(0, MAX_AUDIO_SYNC_DELAY_US);
        let settled = self.last_change_us.is_none_or(|t| now_us.saturating_sub(t) >= DELAY_MIN_INTERVAL_US);
        if settled && (target - self.applied_us).abs() >= DELAY_DEADBAND_US {
            self.applied_us = target;
            self.last_change_us = Some(now_us);
        }
        self.applied_us
    }
}

#[derive(Debug, Default)]
struct Inner {
    video: LatencyFloor,
    audio: LatencyFloor,
    video_decoders: usize,
}

/// One sender's A/V sync state, shared by its video and audio decoders. Cheap to lock (a few
/// field updates), touched once per received frame/packet.
#[derive(Debug)]
pub struct AvSync {
    inner: Mutex<Inner>,
    epoch: Instant,
}

impl AvSync {
    fn new() -> Self {
        Self { inner: Mutex::new(Inner::default()), epoch: Instant::now() }
    }

    /// The shared state of sender `conn`, created on first use. Decoders hold the `Arc`; the
    /// registry holds only a `Weak`, so the entry dies with the last decoder.
    pub fn for_peer(conn: ConnId) -> Arc<AvSync> {
        static REGISTRY: OnceLock<Mutex<HashMap<ConnId, Weak<AvSync>>>> = OnceLock::new();
        let mut map = REGISTRY.get_or_init(Default::default).lock().unwrap();
        map.retain(|_, w| w.strong_count() > 0);
        if let Some(existing) = map.get(&conn).and_then(Weak::upgrade) {
            return existing;
        }
        let fresh = Arc::new(AvSync::new());
        map.insert(conn, Arc::downgrade(&fresh));
        fresh
    }

    fn now_us(&self, now: Instant) -> i64 {
        now.saturating_duration_since(self.epoch).as_micros() as i64
    }

    /// A video decoder of this sender exists (call once; pair with [`Self::video_decoder_gone`]).
    pub fn video_decoder_started(&self) {
        self.inner.lock().unwrap().video_decoders += 1;
    }

    pub fn video_decoder_gone(&self) {
        let mut i = self.inner.lock().unwrap();
        i.video_decoders = i.video_decoders.saturating_sub(1);
    }

    /// A video frame arrived. `stalled` frames (the sender's capture-down keepalive re-sends of a
    /// cached IDR) are skipped: they carry no encoder latency and would pull the floor down.
    pub fn note_video(&self, now: Instant, pts_us: u64, stalled: bool) {
        if stalled {
            return;
        }
        let now_us = self.now_us(now);
        self.inner.lock().unwrap().video.observe(now_us, pts_us);
    }

    /// An audio packet arrived.
    pub fn note_audio(&self, now: Instant, pts_us: u64) {
        let now_us = self.now_us(now);
        self.inner.lock().unwrap().audio.observe(now_us, pts_us);
    }

    /// The extra delay audio wants right now (before [`DelayController`] smoothing).
    pub fn target_audio_delay_us(&self) -> i64 {
        let i = self.inner.lock().unwrap();
        target_delay_us(i.video.floor(), i.audio.floor(), i.video_decoders > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000;

    /// Row 136: the video decoder reports `arrival + playout delay`, so the picture's floor rises by
    /// the buffer and the audio is delayed by the same amount (up to the nominal lead).
    #[test]
    fn the_playout_buffer_delay_reaches_the_audio_delay() {
        let (mut video, mut audio) = (LatencyFloor::new(), LatencyFloor::new());
        feed(&mut audio, 0, 100, 50, 20 * MS);
        feed(&mut video, 0, 100, 60, 150 * MS);
        let without = target_delay_us(video.floor(), audio.floor(), true);
        let mut buffered = LatencyFloor::new();
        feed(&mut buffered, 0, 100, 60, 150 * MS + 180 * MS);
        let with = target_delay_us(buffered.floor(), audio.floor(), true);
        assert_eq!(with - without, 180 * MS);
    }

    /// Feed `n` packets at `fps`, each `lat` us late, starting at `t0` (sender clock = local clock).
    fn feed(f: &mut LatencyFloor, t0: i64, n: i64, fps: i64, lat: i64) -> i64 {
        let step = 1_000_000 / fps;
        let mut t = t0;
        for _ in 0..n {
            f.observe(t + lat, t as u64);
            t += step;
        }
        t
    }

    #[test]
    fn floor_is_the_minimum_not_the_jittery_latest() {
        let mut f = LatencyFloor::new();
        f.observe(100_000, 0); // d = 100 ms
        f.observe(180_000, 50_000); // d = 130 ms
        f.observe(230_000, 100_000); // d = 130 ms
        assert_eq!(f.floor(), Some(100_000));
    }

    #[test]
    fn floor_rises_after_the_window_when_latency_steps_up_and_falls_at_once() {
        let mut f = LatencyFloor::new();
        let t = feed(&mut f, 0, 120, 60, 50 * MS); // 2 s at 50 ms
        assert_eq!(f.floor(), Some(50 * MS));
        // Latency jumps to 750 ms: floor stays low until the 50 ms samples age out.
        let t = feed(&mut f, t, 60, 60, 750 * MS); // +1 s
        assert_eq!(f.floor(), Some(50 * MS));
        let t = feed(&mut f, t, 240, 60, 750 * MS); // +4 s, window fully replaced
        assert_eq!(f.floor(), Some(750 * MS));
        // A fall is followed immediately.
        feed(&mut f, t, 3, 60, 50 * MS);
        assert_eq!(f.floor(), Some(50 * MS));
    }

    #[test]
    fn floor_is_held_when_samples_stop() {
        let mut f = LatencyFloor::new();
        feed(&mut f, 0, 60, 60, 700 * MS);
        assert_eq!(f.floor(), Some(700 * MS));
        // Nothing for a minute: still the last value (idle screen must not move the audio).
        assert_eq!(f.floor(), Some(700 * MS));
    }

    #[test]
    fn floor_is_none_before_any_sample() {
        assert_eq!(LatencyFloor::new().floor(), None);
    }

    #[test]
    fn clock_offset_cancels_out_of_the_target() {
        // Both streams on one sender clock; the local clock is 5 h ahead. Video 750 ms late
        // (lookahead), audio 30 ms late.
        let offset = 5 * 3_600 * 1_000_000_i64;
        let (mut v, mut a) = (LatencyFloor::new(), LatencyFloor::new());
        feed(&mut v, offset, 120, 60, 750 * MS);
        feed(&mut a, offset, 100, 50, 30 * MS);
        let target = target_delay_us(v.floor(), a.floor(), true);
        assert_eq!(target, 750 * MS - 30 * MS - NOMINAL_AUDIO_LEAD_US);
    }

    #[test]
    fn target_is_zero_without_video_or_without_samples() {
        assert_eq!(target_delay_us(Some(800 * MS), Some(0), false), 0, "audio-only: no picture to match");
        assert_eq!(target_delay_us(None, Some(0), true), 0);
        assert_eq!(target_delay_us(Some(800 * MS), None, true), 0);
    }

    #[test]
    fn target_never_goes_negative_and_is_capped() {
        // Video faster than audio plus lead: nothing to add (audio is never advanced).
        assert_eq!(target_delay_us(Some(40 * MS), Some(30 * MS), true), 0);
        assert_eq!(target_delay_us(Some(-500 * MS), Some(0), true), 0);
        // 2.5 s of video latency: capped at 2 s.
        assert_eq!(target_delay_us(Some(2_500 * MS), Some(0), true), MAX_AUDIO_SYNC_DELAY_US);
    }

    #[test]
    fn incomparable_clocks_are_rejected() {
        // An older sender: audio PTS counts from the audio pipeline start, video from the
        // destination start, so the floors differ by minutes. Apply nothing.
        assert_eq!(target_delay_us(Some(1_000 * MS), Some(-200_000 * MS), true), 0);
        assert_eq!(target_delay_us(Some(-90_000 * MS), Some(500 * MS), true), 0);
    }

    #[test]
    fn controller_ignores_small_changes_and_steps_once_for_a_big_one() {
        let mut c = DelayController::new();
        assert_eq!(c.update(30 * MS, 0), 0, "inside the deadband");
        assert_eq!(c.update(700 * MS, 10 * MS), 700 * MS, "first real change applies at once");
        // Estimator wobble of +/-40 ms does nothing.
        assert_eq!(c.update(740 * MS, 2_000 * MS), 700 * MS);
        assert_eq!(c.update(665 * MS, 2_100 * MS), 700 * MS);
    }

    #[test]
    fn controller_rate_limits_consecutive_changes() {
        let mut c = DelayController::new();
        c.update(400 * MS, 0);
        // Estimate keeps climbing while settling: held until the interval passes, then one step.
        assert_eq!(c.update(600 * MS, 100 * MS), 400 * MS);
        assert_eq!(c.update(700 * MS, 400 * MS), 400 * MS);
        assert_eq!(c.update(700 * MS, 600 * MS), 700 * MS);
    }

    #[test]
    fn controller_returns_to_zero_when_video_goes_away() {
        let mut c = DelayController::new();
        c.update(700 * MS, 0);
        assert_eq!(c.update(0, 5_000 * MS), 0);
    }

    #[test]
    fn controller_follows_a_live_latency_change() {
        // Settings change mid-share: lookahead drops video latency 750 -> 200 ms, then back up.
        let (mut v, mut a) = (LatencyFloor::new(), LatencyFloor::new());
        let mut c = DelayController::new();
        let mut t = 0;
        let mut applied = Vec::new();
        for (video_lat, secs) in [(750 * MS, 6), (200 * MS, 6), (900 * MS, 6)] {
            for _ in 0..secs * 10 {
                // 100 ms ticks
                for dt in 0..2 {
                    feed(&mut v, t + dt * 50_000, 3, 60, video_lat);
                }
                feed(&mut a, t, 5, 50, 30 * MS);
                t += 100_000;
                let target = target_delay_us(v.floor(), a.floor(), true);
                c.update(target, t);
            }
            applied.push(c.applied_us());
        }
        assert_eq!(applied[0], 750 * MS - 30 * MS - NOMINAL_AUDIO_LEAD_US);
        assert_eq!(applied[1], 200 * MS - 30 * MS - NOMINAL_AUDIO_LEAD_US);
        assert_eq!(applied[2], 900 * MS - 30 * MS - NOMINAL_AUDIO_LEAD_US);
    }

    #[test]
    fn peers_share_state_and_the_registry_drops_dead_entries() {
        let id: ConnId = 0xA5A5_0001;
        let a = AvSync::for_peer(id);
        let b = AvSync::for_peer(id);
        assert!(Arc::ptr_eq(&a, &b));
        a.video_decoder_started();
        let t0 = Instant::now();
        // Same sender clock for both: audio packets carry capture time 700 ms *later* than the video
        // frame that arrived at the same moment, i.e. the picture is 700 ms behind.
        for i in 0..120u64 {
            let at = t0 + std::time::Duration::from_micros(i * 16_000);
            a.note_video(at, i * 16_000, false);
            b.note_audio(at, i * 16_000 + 700_000);
        }
        let target = a.target_audio_delay_us();
        assert!((target - (700 * MS - NOMINAL_AUDIO_LEAD_US)).abs() <= 20 * MS, "target {target}");
        a.video_decoder_gone();
        assert_eq!(a.target_audio_delay_us(), 0);
        drop((a, b));
        let c = AvSync::for_peer(id);
        assert_eq!(c.target_audio_delay_us(), 0, "fresh state after the last holder dropped");
    }

    #[test]
    fn stalled_keepalives_do_not_pull_the_video_floor_down() {
        let s = AvSync::for_peer(0xA5A5_0002);
        s.video_decoder_started();
        let t0 = Instant::now();
        s.note_video(t0, 0, false);
        let before = s.inner.lock().unwrap().video.floor();
        s.note_video(t0 + std::time::Duration::from_millis(500), 499_000_000, true);
        assert_eq!(s.inner.lock().unwrap().video.floor(), before);
    }

    #[test]
    fn sender_clock_is_monotonic_and_shared() {
        let a = sender_clock_us(Instant::now());
        let b = sender_clock_us(Instant::now());
        assert!(b >= a);
        assert_eq!(sender_clock_epoch(), sender_clock_epoch());
    }
}
