//! The native viewer's playout buffer (row 136, 2026-11-04): holds still-encoded access units for a
//! measured, per-stream delay so a burst of late frames (a keyframe's transfer) reaches the decoder
//! evenly, and plays a little faster than live to shrink the delay again.
//!
//! Full write-up with the `Why:` notes: `superdoc/features/video-pipeline.md` "Playout buffer".
//!
//! # The model
//!
//! Browser reference: `docs/js/core/jitter.js`. Same shape (hold *encoded* chunks, release at
//! `line + applied`, `applied` eases up by at most 20 ms per frame and shrinks no faster than the
//! catch-up percentage of wall-clock time), one difference in what `applied` aims at:
//!
//! ```text
//! x         = arrival - pts                 (the sender's clock offset is in x, and cancels below)
//! baseline  = min x, creeping up 1 ms/s     (the path with nothing queued; follows clock skew)
//! late      = x - baseline                  (how far behind its own schedule this frame arrived)
//! peak      = max of `late` over the last 8 s
//! target    = peak * 1.15, clamped to [0, max]
//! release   = pts + baseline + applied      (never earlier than the previous release)
//! ```
//!
//! `Why a decaying peak and not the browser's `ewma * 4 + 40 ms`:` that formula averages `|jitter|`
//! over about 16 frames, and a keyframe stall is one big sample among hundreds of normal ones. For a
//! 100 ms stall once a second at 60 fps the average deviation is about 2 ms, the target about 48 ms:
//! half of the stall leaks through, once a second (checked with the numbers of row 133: keyframe
//! 1000 stalls about 180 ms). The buffer has to cover the *worst recent* lateness, not the mean, so
//! it follows the peak; the 8 s window lets it shrink again (at the catch-up speed) once the stalls stop, and is long
//! enough to remember a keyframe that comes every 2 s or more. The `+ 40 ms`
//! floor is dropped too: on a clean path the delay should stay near zero (user: "the playout buffer
//! ... dynamic").
//!
//! * A **still screen** adds nothing: no frames arrive, nothing is measured, and the next frame's
//!   `arrival - pts` is the baseline again (capture PTS is the sender's clock, not a frame count).
//! * The **first frame** is released at once (`applied` starts at 0).
//! * Frames are **never dropped** here. A frame later than its release is released on arrival.
//! * The configured maximum is a **hard cap**: a frame never waits longer than that since arrival,
//!   whatever `applied` is; 0 releases everything on arrival (today's behaviour before this buffer).
//! * A PTS discontinuity (a sharer restart, 5 s or more of lateness) restarts the measurement and
//!   releases what is held.
//!
//! Pure (`now` injected), so the maths is unit-tested without sleeping; `video::run` owns one per
//! stream and wakes for the head's due time.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Default and clamp of the user's maximum (Settings > Playback), the browser's numbers.
pub const DEFAULT_MAX_MS: u32 = 2000;
pub const MAX_MS_LIMIT: u32 = 5000;
/// Default and clamp of the catch-up speed percentage.
pub const DEFAULT_CATCH_UP_PERCENT: u32 = 5;
pub const CATCH_UP_RANGE: (u32, u32) = (5, 10);

/// `applied` grows by at most this per frame (browser: `MAX_BUFFER_EASE_STEP_MS`).
const EASE_STEP_MS: f64 = 20.0;
/// Safety margin on the peak.
const PEAK_MARGIN: f64 = 1.15;
/// How far back the worst lateness is remembered, in buckets of `PEAK_BUCKET_MS`.
const PEAK_WINDOW_MS: i64 = 8000;
const PEAK_BUCKET_MS: i64 = 500;
/// How fast the baseline may rise (sender/viewer clock skew; real skew is 0.1-0.2 ms/s).
const BASELINE_CREEP_MS_PER_S: f64 = 1.0;
/// Lateness beyond this is a discontinuity, not jitter.
const DISCONTINUITY_MS: f64 = 5000.0;
/// Held access units at most; past it the oldest is released early (memory bound).
const MAX_HELD: usize = 2048;

static MAX_MS: AtomicU32 = AtomicU32::new(DEFAULT_MAX_MS);
static CATCH_UP_PERCENT: AtomicU32 = AtomicU32::new(DEFAULT_CATCH_UP_PERCENT);

/// Set the two user settings for every stream at once (called at start-up and from the sliders).
pub fn set_limits(max_ms: u32, catch_up_percent: u32) {
    MAX_MS.store(max_ms.min(MAX_MS_LIMIT), Ordering::Relaxed);
    CATCH_UP_PERCENT.store(catch_up_percent.clamp(CATCH_UP_RANGE.0, CATCH_UP_RANGE.1), Ordering::Relaxed);
}

/// The user's settings as the running buffers see them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_ms: u32,
    pub catch_up_percent: u32,
}

impl Limits {
    pub fn current() -> Self {
        Self { max_ms: MAX_MS.load(Ordering::Relaxed), catch_up_percent: CATCH_UP_PERCENT.load(Ordering::Relaxed) }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_ms: DEFAULT_MAX_MS, catch_up_percent: DEFAULT_CATCH_UP_PERCENT }
    }
}

#[derive(Debug)]
struct Held<T> {
    /// When the buffer wants it out, ms since `t0`.
    release_ms: f64,
    /// When it arrived, ms since `t0` (the hard cap counts from here).
    arrival_ms: f64,
    item: T,
}

/// The buffer. `T` is the held access unit.
#[derive(Debug)]
pub struct Playout<T> {
    t0: Option<Instant>,
    held: VecDeque<Held<T>>,
    /// `Some` once a frame was measured.
    baseline_ms: Option<f64>,
    last_ms: f64,
    /// (bucket index, largest `late` in it), oldest first.
    peaks: VecDeque<(i64, f64)>,
    applied_ms: f64,
    last_release_ms: f64,
}

impl<T> Default for Playout<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Playout<T> {
    pub fn new() -> Self {
        Self { t0: None, held: VecDeque::new(), baseline_ms: None, last_ms: 0.0, peaks: VecDeque::new(), applied_ms: 0.0, last_release_ms: f64::NEG_INFINITY }
    }

    fn ms(&mut self, now: Instant) -> f64 {
        let t0 = *self.t0.get_or_insert(now);
        now.saturating_duration_since(t0).as_secs_f64() * 1000.0
    }

    /// The delay currently applied to every frame, ms (what A/V sync must add to the audio).
    pub fn applied_ms(&self) -> f64 {
        self.applied_ms
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    /// Everything held, out in order, and the measurement forgotten (a decoder reset, recovery).
    pub fn drain_all(&mut self) -> Vec<T> {
        let out = self.held.drain(..).map(|h| h.item).collect();
        *self = Self::new();
        out
    }

    /// Drop everything held without releasing it (the `Reset` message: pre-reset input).
    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Queue one access unit that arrived at `now` with sender timestamp `pts_us`.
    pub fn push(&mut self, now: Instant, pts_us: u64, item: T, lim: Limits) {
        let t = self.ms(now);
        let pts = pts_us as f64 / 1000.0;
        let x = t - pts;
        let max = f64::from(lim.max_ms.min(MAX_MS_LIMIT));

        if self.baseline_ms.is_some_and(|b| x - b > DISCONTINUITY_MS) {
            // Release what is held now and measure again from this frame.
            for h in &mut self.held {
                h.release_ms = f64::NEG_INFINITY;
            }
            self.baseline_ms = None;
            self.peaks.clear();
            self.applied_ms = 0.0;
            self.last_release_ms = f64::NEG_INFINITY;
        }
        let dt = (t - self.last_ms).max(0.0);
        self.last_ms = t;
        let baseline = match self.baseline_ms {
            None => x,
            Some(b) => (b + BASELINE_CREEP_MS_PER_S * dt / 1000.0).min(x),
        };
        self.baseline_ms = Some(baseline);
        let late = x - baseline;
        let bucket = (t as i64).div_euclid(PEAK_BUCKET_MS);
        while self.peaks.front().is_some_and(|&(b, _)| b <= bucket - PEAK_WINDOW_MS / PEAK_BUCKET_MS) {
            self.peaks.pop_front();
        }
        match self.peaks.back_mut() {
            Some((b, m)) if *b == bucket => *m = m.max(late),
            _ => self.peaks.push_back((bucket, late)),
        }
        let peak = self.peaks.iter().map(|&(_, m)| m).fold(0.0, f64::max);

        let target = (peak * PEAK_MARGIN).clamp(0.0, max);
        let diff = target - self.applied_ms;
        if diff >= 0.0 {
            self.applied_ms += diff.min(EASE_STEP_MS);
        } else {
            // Shrinking is capped at the catch-up percentage of wall-clock time: the same thing as
            // playing at 1 + percent/100 times live speed while the delay falls.
            let step = dt * f64::from(lim.catch_up_percent.clamp(CATCH_UP_RANGE.0, CATCH_UP_RANGE.1)) / 100.0;
            self.applied_ms += diff.max(-step);
        }
        self.applied_ms = self.applied_ms.clamp(0.0, max);

        let release = (pts + baseline + self.applied_ms).max(self.last_release_ms);
        self.last_release_ms = release;
        self.held.push_back(Held { release_ms: release, arrival_ms: t, item });
        if self.held.len() > MAX_HELD {
            if let Some(h) = self.held.front_mut() {
                h.release_ms = f64::NEG_INFINITY;
            }
        }
    }

    fn due_ms(h: &Held<T>, lim: Limits) -> f64 {
        h.release_ms.min(h.arrival_ms + f64::from(lim.max_ms.min(MAX_MS_LIMIT)))
    }

    /// Every access unit due at `now`, in order.
    pub fn poll(&mut self, now: Instant, lim: Limits) -> Vec<T> {
        let t = self.ms(now);
        let mut out = Vec::new();
        while self.held.front().is_some_and(|h| Self::due_ms(h, lim) <= t + 1e-6) {
            out.push(self.held.pop_front().expect("checked").item);
        }
        out
    }

    /// How long until the head is due (zero when overdue); `None` when nothing is held.
    pub fn next_due(&mut self, now: Instant, lim: Limits) -> Option<Duration> {
        let t = self.ms(now);
        let h = self.held.front()?;
        Some(Duration::from_secs_f64((Self::due_ms(h, lim) - t).max(0.0) / 1000.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One simulated frame: sender pts (ms), keyframe, when it reached the viewer (ms).
    struct Sim {
        t0: Instant,
        p: Playout<u64>,
        lim: Limits,
        /// (pts_us, released_at_ms)
        out: Vec<(u64, f64)>,
        /// (pts_ms, arrival_ms, applied after push)
        log: Vec<(f64, f64, f64)>,
    }

    impl Sim {
        fn new(lim: Limits) -> Self {
            Self { t0: Instant::now(), p: Playout::new(), lim, out: vec![], log: vec![] }
        }
        fn at(&self, ms: f64) -> Instant {
            self.t0 + Duration::from_secs_f64(ms / 1000.0)
        }
        /// Runs the clock from `from` to `to` in 0.25 ms steps, collecting releases.
        fn run_to(&mut self, from: f64, to: f64) {
            let mut t = from;
            while t <= to {
                let now = self.at(t);
                for pts_us in self.p.poll(now, self.lim) {
                    self.out.push((pts_us, t));
                }
                t += 0.25;
            }
        }
        /// Feed `frames` = (pts_ms, arrival_ms) in arrival order, polling continuously.
        fn feed(&mut self, frames: &[(f64, f64)]) {
            let mut clock = frames.first().map_or(0.0, |f| f.1);
            for &(pts, arr) in frames {
                self.run_to(clock, arr);
                clock = arr;
                self.p.push(self.at(arr), (pts * 1000.0) as u64, (pts * 1000.0) as u64, self.lim);
                self.log.push((pts, arr, self.p.applied_ms()));
            }
            let end = clock + 3000.0;
            self.run_to(clock, end);
        }
    }

    /// The row-133 link: every `key_every` ms the sender emits a keyframe that takes `key_ms` to
    /// cross the uplink; frames behind it arrive when it is through. `fps` frames per second.
    fn bottleneck(fps: f64, secs: f64, key_every: f64, key_ms: f64, base: f64) -> Vec<(f64, f64)> {
        let mut frames = vec![];
        let mut link_free = 0.0f64;
        let mut next_key = key_every;
        let n = (fps * secs) as usize;
        for k in 0..n {
            let pts = k as f64 * 1000.0 / fps;
            let key = k == 0 || pts >= next_key;
            if key && k > 0 {
                next_key += key_every;
            }
            let tx = if key { key_ms } else { 0.3 };
            let arr = (pts + base).max(link_free) + tx;
            link_free = arr;
            frames.push((pts, arr));
        }
        frames
    }

    /// Spread of `released - pts` over the frames from `from_ms` on: max minus min.
    fn out_spread(s: &Sim, from_ms: f64) -> f64 {
        let v: Vec<f64> = s.out.iter().filter(|o| o.0 as f64 / 1000.0 >= from_ms).map(|o| o.1 - o.0 as f64 / 1000.0).collect();
        v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
    }

    #[test]
    fn a_clean_steady_stream_stays_near_zero_extra_delay() {
        let mut s = Sim::new(Limits::default());
        // 60 fps, arrival jitter up to 2 ms.
        let frames: Vec<(f64, f64)> =
            (0..600).map(|k| (k as f64 * 16.667, k as f64 * 16.667 + 5.0 + ((k * 7) % 5) as f64 * 0.5)).collect();
        s.feed(&frames);
        let worst = s.log.iter().map(|l| l.2).fold(0.0, f64::max);
        assert!(worst < 4.0, "applied delay {worst} ms on a clean path");
        assert_eq!(s.out.len(), 600, "nothing dropped");
    }

    #[test]
    fn a_keyframe_stall_every_250_ms_is_covered_and_frames_come_out_evenly() {
        let mut s = Sim::new(Limits::default());
        s.feed(&bottleneck(60.0, 12.0, 250.0, 100.0, 5.0));
        // After 3 s of learning every frame leaves at the same offset from its pts: even playout.
        let spread = out_spread(&s, 3000.0);
        assert!(spread < 3.0, "release-minus-pts spread {spread} ms");
        let applied = s.log.last().unwrap().2;
        assert!((95.0..=130.0).contains(&applied), "applied {applied}");
        assert_eq!(s.out.len(), 720, "nothing dropped");
        // Output is in pts order.
        assert!(s.out.windows(2).all(|w| w[0].0 <= w[1].0));
    }

    #[test]
    fn a_one_second_keyframe_interval_with_a_180_ms_stall_is_covered_too() {
        // The browser's ewma*4+40 would give ~50 ms here; the peak must give ~180.
        let mut s = Sim::new(Limits::default());
        s.feed(&bottleneck(60.0, 20.0, 1000.0, 180.0, 5.0));
        let spread = out_spread(&s, 5000.0);
        assert!(spread < 4.0, "spread {spread} ms");
        let applied = s.log.last().unwrap().2;
        assert!(applied >= 175.0, "applied {applied}");
    }

    #[test]
    fn catch_up_shrinks_no_faster_than_the_percentage() {
        let mut s = Sim::new(Limits { max_ms: 2000, catch_up_percent: 5 });
        // Stalls for 6 s, then a clean 20 s at 60 fps.
        let mut frames = bottleneck(60.0, 6.0, 250.0, 100.0, 5.0);
        let last = *frames.last().unwrap();
        for k in 1..=1200 {
            let pts = last.0 + k as f64 * 16.667;
            frames.push((pts, pts + 5.0 + (last.1 - last.0 - 5.0).max(0.0) * 0.0));
        }
        s.feed(&frames);
        let after: Vec<&(f64, f64, f64)> = s.log.iter().filter(|l| l.0 > last.0).collect();
        // Over any one second of wall time the applied delay fell by at most 5 % of it (50 ms).
        for w in after.windows(61) {
            let drop = w[0].2 - w[60].2;
            let wall = w[60].1 - w[0].1;
            assert!(drop <= wall * 0.05 + 1.0, "fell {drop} ms in {wall} ms");
        }
        // And it did get back to nothing.
        assert!(after.last().unwrap().2 < 1.0, "ended at {}", after.last().unwrap().2);
        assert!(after.first().unwrap().2 > 60.0);
        // Percentage setting is honoured: 10 % gets there about twice as fast.
        let mut fast = Sim::new(Limits { max_ms: 2000, catch_up_percent: 10 });
        fast.feed(&frames);
        // Time the delay takes to fall from 80 % to 20 % of what it was when the stalls ended.
        let fall = |l: &[(f64, f64, f64)]| {
            let top = l.iter().filter(|e| e.0 > last.0 - 1000.0 && e.0 <= last.0).map(|e| e.2).fold(0.0, f64::max);
            let after = || l.iter().filter(|e| e.0 > last.0);
            let from = after().find(|e| e.2 <= top * 0.8).unwrap().1;
            let to = after().find(|e| e.2 <= top * 0.2).unwrap().1;
            to - from
        };
        let (slow_t, fast_t) = (fall(&s.log), fall(&fast.log));
        assert!(fast_t < slow_t * 0.7, "10 % took {fast_t} ms, 5 % took {slow_t} ms");
    }

    #[test]
    fn a_still_screen_does_not_grow_the_buffer() {
        let mut s = Sim::new(Limits::default());
        // 2 s of motion at 60 fps, then one frame every 3 s (damage-driven), arrival 5 ms after capture.
        let mut frames: Vec<(f64, f64)> = (0..120).map(|k| (k as f64 * 16.667, k as f64 * 16.667 + 5.0)).collect();
        for k in 1..=10 {
            let pts = 2000.0 + k as f64 * 3000.0;
            frames.push((pts, pts + 5.0));
        }
        let pts = 2000.0 + 11.0 * 3000.0;
        for k in 0..60 {
            frames.push((pts + k as f64 * 16.667, pts + k as f64 * 16.667 + 5.0));
        }
        s.feed(&frames);
        assert!(s.log.iter().all(|l| l.2 < 1.0), "grew to {:?}", s.log.iter().map(|l| l.2).fold(0.0, f64::max));
    }

    #[test]
    fn the_first_frame_is_shown_at_once_even_when_it_is_a_late_keyframe() {
        let mut p = Playout::new();
        let t0 = Instant::now();
        p.push(t0, 1_000_000, 1u8, Limits::default());
        assert_eq!(p.poll(t0, Limits::default()), vec![1]);
    }

    #[test]
    fn the_configured_maximum_is_a_hard_cap() {
        let lim = Limits { max_ms: 60, catch_up_percent: 5 };
        let mut s = Sim::new(lim);
        s.feed(&bottleneck(60.0, 12.0, 250.0, 200.0, 5.0));
        assert!(s.log.iter().all(|l| l.2 <= 60.0 + 1e-9));
        // No frame waited longer than the cap after it arrived.
        let arr: std::collections::HashMap<u64, f64> = s.log.iter().map(|l| ((l.0 * 1000.0) as u64, l.1)).collect();
        for (pts, at) in &s.out {
            let a = arr[pts];
            assert!(at - a <= 60.25 + 1e-6, "held {} ms", at - a);
        }
    }

    #[test]
    fn a_maximum_of_zero_turns_the_buffer_off() {
        let mut s = Sim::new(Limits { max_ms: 0, catch_up_percent: 5 });
        s.feed(&bottleneck(60.0, 6.0, 250.0, 100.0, 5.0));
        let arr: std::collections::HashMap<u64, f64> = s.log.iter().map(|l| ((l.0 * 1000.0) as u64, l.1)).collect();
        for (pts, at) in &s.out {
            assert!(at - arr[pts] <= 0.3, "held");
        }
        assert!(s.log.iter().all(|l| l.2 == 0.0));
    }

    #[test]
    fn lowering_the_maximum_releases_held_frames_at_once() {
        let t0 = Instant::now();
        let mut p = Playout::new();
        let big = Limits { max_ms: 2000, catch_up_percent: 5 };
        // Learn a 100 ms stall first.
        for k in 0..30u64 {
            let late = if k == 5 { 100.0 } else { 0.0 };
            p.push(t0 + Duration::from_secs_f64((k as f64 * 16.667 + late) / 1000.0), k * 16_667, k, big);
        }
        let now = t0 + Duration::from_millis(500);
        assert!(p.next_due(now, big).is_some());
        let small = Limits { max_ms: 0, catch_up_percent: 5 };
        assert_eq!(p.next_due(now, small), Some(Duration::ZERO));
        assert!(!p.poll(now, small).is_empty());
    }

    #[test]
    fn a_pts_restart_releases_the_held_and_measures_again() {
        let t0 = Instant::now();
        let lim = Limits::default();
        let mut p = Playout::new();
        for k in 0..20u64 {
            p.push(t0 + Duration::from_millis(k * 16), 100_000_000 + k * 16_000, k, lim);
        }
        // The sharer restarted: pts is tiny again, 10 s of "lateness".
        p.push(t0 + Duration::from_millis(330), 500, 99, lim);
        let out = p.poll(t0 + Duration::from_millis(330), lim);
        assert_eq!(out.last(), Some(&99));
        assert_eq!(p.applied_ms(), 0.0);
    }

    #[test]
    fn clock_skew_of_the_sender_does_not_build_a_delay() {
        // Sender clock 200 ppm slow and an arbitrary 40 s offset: relative arrival only.
        let mut s = Sim::new(Limits::default());
        let frames: Vec<(f64, f64)> = (0..3000).map(|k| {
            let pts = k as f64 * 16.667;
            (pts, pts * 1.0002 + 5.0)
        }).collect();
        s.feed(&frames);
        assert!(s.log.iter().all(|l| l.2 < 3.0));
    }
}
