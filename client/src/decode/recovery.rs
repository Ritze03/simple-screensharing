//! Viewer-side stream recovery: the pure state machine behind
//! [`StreamDecoder::tick`](super::StreamDecoder::tick).
//!
//! The wire is lossy and has no retransmission. Every codec/fps transition and
//! every stretch of loss needs one intact keyframe, and until this module the
//! native viewer had **no way to ask for another one** when it was lost: it
//! showed a corrupt or frozen picture until the sender's next scheduled IDR
//! (2 s GOP), or, after a codec switch whose IDR was lost, fed the new codec's
//! deltas into the old decoder for as long as the sender kept going. The only
//! primitive a viewer has is Unsubscribe + Subscribe (the relay then sends the
//! owner a `KeyframeRequest`, spec §9); the browser already uses it
//! (`docs/js/core/video-rx.js` `markLoss`, `room-client.js`). This is the native
//! counterpart, escalating:
//!
//! * **L1 keyframe request** ([`RecoveryAction::RequestKeyframe`]): on loss
//!   evidence, or when the decoder was fed but produced nothing.
//! * **L2 decoder reset** ([`RecoveryAction::ResetDecoder`]): rebuild the
//!   pipeline in place when requests do not help or keyframes arrive and nothing
//!   decodes. Also asks for a keyframe, a reset is useless without one.
//! * **L3 reconnect** ([`RecoveryAction::Reconnect`]): the stream was flowing and
//!   then nothing at all arrived for [`L3_SILENCE`]; the path is dead, do not
//!   wait for the QUIC idle timeout.
//!
//! Everything here takes an injected `now` and the decoder's counters as
//! arguments, so the timing is unit-tested without sleeping.
//!
//! # Thresholds, and why
//!
//! * [`L1_MIN_INTERVAL`] 1.5 s, same as the browser: every request makes the
//!   sharer encode and send an IDR (a few frames' worth of bytes) to everybody
//!   watching, so asking faster than the reply can possibly arrive (measured
//!   first picture 1.1-1.4 s) only multiplies keyframes.
//! * Re-ask backoff 1.5 / 3 / 6 / 10 s ([`REASK_BACKOFF`]) for *silent* failures
//!   (a request that got no answer at all: sharer gone, path down), so a dead
//!   path is not hammered. A request whose answer arrived *incomplete* (a lost
//!   IDR, which at 0.5-1 % loss and ~100 fragments is the usual failure) is
//!   proof of life and restarts the backoff: it re-asks after the 1.5 s minimum.
//! * Delta loss asks only for a *burst* ([`DELTA_BURST`] incomplete frames within
//!   1 s): a single lost delta is a local artifact the next IDR repairs, and in the
//!   A/B runs asking for each of them did not shorten the freezes but multiplied
//!   the keyframes. And then only when the request is expected to
//!   beat the scheduled IDR ([`L1_EXPECTED_LATENCY`] vs the observed keyframe
//!   interval): a request costs ~1.3 s, so one made 1.5 s into a 2 s GOP
//!   delivers its keyframe *after* the scheduled one would have, for nothing.
//!   And those asks use the same backoff, which is *not* reset by each
//!   recovery (only by [`DELTA_QUIET`] without evidence): on a link that keeps
//!   losing frames the requested IDRs (several times a delta's size) are what
//!   is lost next, and asking at the maximum rate was a permanent keyframe
//!   storm in the first rig run. A lost *keyframe* always asks (the scheduled
//!   one is the one that was lost).
//! * Fed-but-nothing-decoded: L1 after [`UNDECODED_L1`] (1 s, longer than a
//!   VA decoder's start-up and any sane output delay), L2 after
//!   [`UNDECODED_L2`] (3 s) while keyframes keep arriving.
//! * [`L3_SILENCE`] 8 s: a healthy sharer sends at least ~2 fps even on a static
//!   screen (capture keepalive, stalled re-send), so 8 s is four missed
//!   heartbeats. It only counts for a stream that was demonstrably flowing
//!   ([`FLOWING_MAX_GAP`]), is not flagged stalled, and (driver side) while no
//!   other stream of the room delivered either.
//!
//! # Holding the last good frame (and when not to)
//!
//! Deltas are dropped, and the renderer keeps the last good picture, only while
//! the *decoder has no valid state*: before the first keyframe, after a reset, after
//! the pipeline thread reported a failure (or the other bitstream family: Annex-B vs
//! OBU after a codec switch) and while fed AUs produce nothing. Loss evidence alone
//! (a lost delta, a lost IDR, a blackout) does **not** gate: the decoder keeps
//! decoding, the picture shows artifacts until the requested/scheduled IDR arrives.
//! `Why:` the first design gated on a lost IDR (as the browser does) and the rig
//! showed what that costs on a lossy link: an IDR of ~100 fragments at 1 % loss
//! arrives intact only ~37 % of the time, so "hold the last frame until an intact IDR"
//! meant multi-second freezes, which is exactly the "share hangs" report. A moving,
//! slightly corrupt picture that heals is the better failure; a decoder that is fed
//! the wrong codec produces no output at all and is caught by the undecoded rule.

use std::time::{Duration, Instant};

use crate::fragment::FrameLoss;

pub(super) const L1_MIN_INTERVAL: Duration = Duration::from_millis(1500);
/// Delay before the n-th re-ask within one episode (index `asks - 1`, last repeats).
pub(super) const REASK_BACKOFF: [Duration; 4] = [
    Duration::from_millis(1500),
    Duration::from_millis(3000),
    Duration::from_millis(6000),
    Duration::from_millis(10_000),
];
/// How long a keyframe request is expected to take end to end.
pub(super) const L1_EXPECTED_LATENCY: Duration = Duration::from_millis(1300);
/// Keyframe interval assumed until two keyframes have been observed (spec §5 GOP).
const DEFAULT_GOP: Duration = Duration::from_secs(2);
pub(super) const UNDECODED_L1: Duration = Duration::from_secs(1);
pub(super) const UNDECODED_L2: Duration = Duration::from_secs(3);
/// Fewest AUs fed without any decoded frame for the above to count (a 1 fps
/// trickle is not a stuck decoder).
const UNDECODED_MIN_FED: u32 = 3;
/// A decoder is not reset more often than this.
pub(super) const L2_MIN_INTERVAL: Duration = Duration::from_secs(5);
pub(super) const L3_SILENCE: Duration = Duration::from_secs(8);
/// An arrival gap this long on a flowing stream counts as loss evidence ...
const GAP_LOSS: Duration = Duration::from_millis(500);
/// ... and a hole this long asks for a keyframe whatever the GOP schedule says.
const GAP_BLACKOUT: Duration = Duration::from_secs(1);
/// Mean inter-arrival gap below which a stream counts as "flowing" (>= 2 fps).
pub(super) const FLOWING_MAX_GAP: Duration = Duration::from_millis(500);
/// Frames needed before a stream counts as flowing at all.
const FLOWING_MIN_FRAMES: u32 = 10;
const MS500: Duration = Duration::from_millis(500);
/// Incomplete delta frames within [`DELTA_BURST_WINDOW`] that count as "the picture is badly broken".
const DELTA_BURST: u32 = 3;
const DELTA_BURST_WINDOW: Duration = Duration::from_secs(1);
/// Delta-loss requests start over after this long without qualifying evidence.
const DELTA_QUIET: Duration = Duration::from_secs(10);
/// UI shows "Recovering" once an episode has lasted this long (a normal
/// recovery is over by then on a decent link; flashing it on every hiccup is noise).
pub(super) const RECOVERING_SHOW_AFTER: Duration = Duration::from_millis(1500);
/// No frames for this long on a flowing stream also reads as recovering.
pub(super) const SILENCE_SHOW_AFTER: Duration = Duration::from_secs(3);

/// What the driver must do, returned by [`Recovery::tick`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Unsubscribe + Subscribe the stream (the relay then asks the sharer for an IDR).
    RequestKeyframe,
    /// Same request, and the decoder pipeline was rebuilt in place (already done by the caller of `tick`).
    ResetDecoder,
    /// The path is dead: reconnect the room instead of waiting for the QUIC idle timeout.
    Reconnect,
}

/// What a tile should say about the stream's health.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewState {
    Healthy,
    /// Waiting for a keyframe / decoder restart / the path to come back.
    Recovering,
    /// The sharer flagged its capture as stalled (advisory, never acted on).
    SharerStalled,
}

/// Why an episode started (logs, tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    KeyframeLost,
    DeltaLoss,
    Gap,
    IngestDrop,
    Undecoded,
    DecoderNeedsKeyframe,
}

/// What `on_frame` needs to know about an arriving frame.
#[derive(Debug, Clone, Copy)]
pub(super) struct FrameObs {
    pub keyframe: bool,
    pub stalled: bool,
    pub loss: FrameLoss,
}

#[derive(Debug, Clone, Copy)]
struct Episode {
    since: Instant,
    reason: Reason,
    asks: u32,
    last_ask: Option<Instant>,
    /// A decoder reset was already done for this episode.
    reset_done: bool,
}

pub(super) struct Recovery {
    created_at: Instant,
    last_arrival: Option<Instant>,
    frames_seen: u32,
    /// Smoothed inter-arrival gap, ms.
    gap_ema_ms: f64,
    last_stalled: bool,
    last_keyframe_at: Option<Instant>,
    /// Largest of the recent keyframe-to-keyframe intervals.
    gop_est: Duration,
    recent_intervals: [Duration; 3],
    /// Deltas are dropped until a keyframe arrives: only while the decoder has no valid state
    /// (start, reset, it failed, it takes nothing in).
    gate: bool,
    episode: Option<Episode>,
    /// Delta-loss requests since the last quiet period, and when the last one was made.
    delta_asks: u32,
    burst_since: Option<Instant>,
    burst_frames: u32,
    last_delta_ask_at: Option<Instant>,
    last_delta_evidence: Option<Instant>,
    recovered: Option<(Duration, u32)>,
    last_ask_at: Option<Instant>,
    last_reset_at: Option<Instant>,
    // decode progress
    decoded_seen: u64,
    fed_since_progress: u32,
    unprogressed_since: Option<Instant>,
    /// Counters for logs / tests.
    pub asks_total: u32,
    pub resets_total: u32,
}

impl Recovery {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            created_at: now,
            last_arrival: None,
            frames_seen: 0,
            gap_ema_ms: 0.0,
            last_stalled: false,
            last_keyframe_at: None,
            gop_est: DEFAULT_GOP,
            recent_intervals: [DEFAULT_GOP; 3],
            gate: true, // nothing decodes before the first keyframe
            episode: None,
            delta_asks: 0,
            burst_since: None,
            burst_frames: 0,
            last_delta_ask_at: None,
            last_delta_evidence: None,
            recovered: None,
            last_ask_at: None,
            last_reset_at: None,
            decoded_seen: 0,
            fed_since_progress: 0,
            unprogressed_since: None,
            asks_total: 0,
            resets_total: 0,
        }
    }

    /// Whether the stream has been delivering at >= 2 fps recently enough to
    /// treat silence as abnormal.
    fn flowing(&self) -> bool {
        self.frames_seen >= FLOWING_MIN_FRAMES && !self.last_stalled && self.gap_ema_ms < FLOWING_MAX_GAP.as_millis() as f64
    }

    pub(super) fn last_arrival(&self) -> Option<Instant> {
        self.last_arrival
    }

    #[cfg(test)]
    pub(super) fn gate_closed(&self) -> bool {
        self.gate
    }

    /// The decoder cannot continue without a keyframe: drop deltas (and keep the last
    /// picture on screen) until one arrives.
    fn close_gate(&mut self) {
        self.gate = true;
    }

    /// Open a recovery episode (a keyframe is wanted) unless one is open. The keyframe
    /// gate is separate: see [`Self::close_gate`].
    fn begin(&mut self, now: Instant, reason: Reason) {
        if self.episode.is_none() {
            self.episode = Some(Episode { since: now, reason, asks: 0, last_ask: None, reset_done: false });
        }
    }

    /// A frame arrived. Returns whether it should be handed to the decoder
    /// (`false`: dropped while a keyframe is missing).
    pub(super) fn on_frame(&mut self, now: Instant, obs: FrameObs) -> bool {
        // Gap evidence first: it uses the state from *before* this arrival.
        if let Some(prev) = self.last_arrival {
            let gap = now.saturating_duration_since(prev);
            let prev_stalled = self.last_stalled;
            if self.flowing() && !prev_stalled && !obs.stalled && !obs.keyframe && gap >= GAP_LOSS && gap.as_millis() as f64 >= 8.0 * self.gap_ema_ms {
                if gap >= GAP_BLACKOUT {
                    // Seconds of frames are missing: always worth a keyframe, whatever the schedule.
                    self.begin(now, Reason::Gap);
                } else {
                    // A smaller hole (>= 0.5 s of frames: dozens): counts as a burst by itself.
                    self.delta_loss(now, Reason::Gap, DELTA_BURST);
                }
            }
            let g = (gap.as_millis() as f64).min(1000.0);
            self.gap_ema_ms = if self.frames_seen <= 1 { g } else { self.gap_ema_ms * 0.9 + g * 0.1 };
        }
        self.last_arrival = Some(now);
        self.last_stalled = obs.stalled;
        self.frames_seen = self.frames_seen.saturating_add(1);

        if obs.keyframe {
            if !obs.stalled {
                if let Some(prev) = self.last_keyframe_at {
                    let interval = now.saturating_duration_since(prev);
                    // Intervals under 0.8 s are requested keyframes and bursts, not the GOP.
                    if interval >= Duration::from_millis(800) {
                        self.recent_intervals.rotate_left(1);
                        self.recent_intervals[2] = interval.min(Duration::from_secs(10));
                        self.gop_est = *self.recent_intervals.iter().max().expect("non-empty");
                    }
                }
            }
            self.last_keyframe_at = Some(now);
            // A fresh IDR repairs everything before it, whatever was lost.
            if let Some(ep) = self.episode.take() {
                self.recovered = Some((now.saturating_duration_since(ep.since), ep.asks));
            }
            self.gate = false;
            return true;
        }

        if obs.loss.keyframe {
            self.keyframe_lost(now);
        } else if !obs.loss.is_none() {
            self.delta_loss(now, Reason::DeltaLoss, obs.loss.frames);
        }
        !self.gate
    }

    /// A keyframe arrived incomplete (or was dropped before the decoder). Always asks:
    /// the scheduled one is the one that was lost, and after a codec switch nothing
    /// decodes without it. The partial keyframe also proves the path and the sharer are
    /// alive, so an ongoing episode's backoff starts over (it is for *silence*).
    fn keyframe_lost(&mut self, now: Instant) {
        match &mut self.episode {
            Some(ep) => {
                ep.asks = 0;
                ep.last_ask = None;
            }
            None => self.begin(now, Reason::KeyframeLost),
        }
    }

    /// A delta frame (or several) was lost. Asks only when a request is expected to beat
    /// the scheduled keyframe, and with a backoff that is *not* reset by each recovery:
    /// on a link that keeps losing frames the requested IDRs (several times a delta's
    /// size) are what gets lost next, and asking at the maximum rate would just turn
    /// that into a permanent keyframe storm (seen in the first rig run).
    ///
    /// Only a *burst* counts ([`DELTA_BURST`] incomplete frames within [`DELTA_BURST_WINDOW`]):
    /// a single lost delta is a local artifact that the next IDR repairs, and in the A/B runs
    /// (`SSS_DISABLE_VIEWER_RECOVERY`) asking for every one of them did not shorten the
    /// freezes but multiplied the keyframes (and the sharer's pause/resume churn).
    fn delta_loss(&mut self, now: Instant, reason: Reason, weight: u32) {
        if self.burst_since.is_none_or(|t| now.saturating_duration_since(t) > DELTA_BURST_WINDOW) {
            self.burst_since = Some(now);
            self.burst_frames = 0;
        }
        self.burst_frames = self.burst_frames.saturating_add(weight);
        if self.burst_frames < DELTA_BURST {
            return;
        }
        let age = self.last_keyframe_at.map(|t| now.saturating_duration_since(t));
        if !age.is_none_or(|a| a + L1_EXPECTED_LATENCY < self.gop_est) {
            return;
        }
        if self.last_delta_evidence.is_some_and(|t| now.saturating_duration_since(t) >= DELTA_QUIET) {
            self.delta_asks = 0;
        }
        self.last_delta_evidence = Some(now);
        let wait = match self.delta_asks {
            0 => Duration::ZERO,
            n => REASK_BACKOFF[(n as usize - 1).min(REASK_BACKOFF.len() - 1)],
        };
        if self.last_delta_ask_at.is_some_and(|t| now.saturating_duration_since(t) < wait) {
            return;
        }
        if self.episode.is_none() {
            self.delta_asks += 1;
            self.last_delta_ask_at = Some(now);
            self.begin(now, reason);
        }
    }

    /// An access unit was dropped before the decoder (ingest channel full).
    pub(super) fn on_ingest_drop(&mut self, now: Instant, keyframe: bool) {
        if keyframe {
            self.keyframe_lost(now);
        } else {
            self.delta_loss(now, Reason::IngestDrop, 1);
        }
    }

    /// Set when a keyframe ended an episode: how long it took and how many requests it needed.
    pub(super) fn take_recovered(&mut self) -> Option<(Duration, u32)> {
        self.recovered.take()
    }

    /// An access unit was handed to the decoder pipeline.
    pub(super) fn on_fed(&mut self, now: Instant) {
        if self.fed_since_progress == 0 {
            self.unprogressed_since = Some(now);
        }
        self.fed_since_progress = self.fed_since_progress.saturating_add(1);
    }

    /// The decoder pipeline was rebuilt: its history starts over.
    pub(super) fn on_reset(&mut self, now: Instant) {
        self.gate = true;
        self.fed_since_progress = 0;
        self.unprogressed_since = None;
        self.last_reset_at = Some(now);
        self.resets_total += 1;
        match &mut self.episode {
            Some(ep) => ep.reset_done = true,
            None => {
                self.episode =
                    Some(Episode { since: now, reason: Reason::Undecoded, asks: 0, last_ask: None, reset_done: true })
            }
        }
    }

    fn ask_due(&self, now: Instant) -> bool {
        let Some(ep) = self.episode else { return false };
        if self.last_ask_at.is_some_and(|t| now.saturating_duration_since(t) < L1_MIN_INTERVAL) {
            return false;
        }
        match ep.last_ask {
            None => true,
            Some(t) => {
                let idx = (ep.asks.saturating_sub(1) as usize).min(REASK_BACKOFF.len() - 1);
                now.saturating_duration_since(t) >= REASK_BACKOFF[idx]
            }
        }
    }

    fn note_ask(&mut self, now: Instant) {
        self.last_ask_at = Some(now);
        self.asks_total += 1;
        if let Some(ep) = &mut self.episode {
            ep.asks += 1;
            ep.last_ask = Some(now);
        }
    }

    /// Periodic driver. `decoded_total` is the decoder's output counter,
    /// `decoder_needs_keyframe` the pipeline thread's "dropping deltas until an
    /// IDR" flag. Returns what to do now, at most one action per call.
    ///
    /// For [`RecoveryAction::ResetDecoder`] the caller must rebuild the pipeline
    /// (this has already recorded it as done).
    pub(super) fn tick(&mut self, now: Instant, decoded_total: u64, decoder_needs_keyframe: bool) -> Option<RecoveryAction> {
        // --- decode progress ------------------------------------------------
        if decoded_total > self.decoded_seen {
            self.decoded_seen = decoded_total;
            self.fed_since_progress = 0;
            self.unprogressed_since = None;
        }
        let undecoded_for = (self.fed_since_progress >= UNDECODED_MIN_FED)
            .then_some(self.unprogressed_since)
            .flatten()
            .map(|t| now.saturating_duration_since(t));

        // --- L3: the path is dead -------------------------------------------
        // Silence only means that for a stream that was flowing before it
        // (`flowing()` is frozen at the last arrival, which is what it is during silence).
        if let Some(last) = self.last_arrival
            && self.flowing()
            && now.saturating_duration_since(last) >= L3_SILENCE
        {
            return Some(RecoveryAction::Reconnect);
        }

        // --- episode triggers -----------------------------------------------
        // (Not within 0.5 s of a keyframe: the pipeline thread clears its flag a little after
        // the keyframe was handed over, and that must not read as a new failure.)
        if decoder_needs_keyframe && self.last_keyframe_at.is_none_or(|k| now.saturating_duration_since(k) > MS500) {
            self.close_gate();
            self.begin(now, Reason::DecoderNeedsKeyframe);
        }
        if undecoded_for.is_some_and(|d| d >= UNDECODED_L1) {
            self.close_gate();
            self.begin(now, Reason::Undecoded);
        }

        // --- L2: rebuild the decoder ----------------------------------------
        let reset_ok = self.last_reset_at.is_none_or(|t| now.saturating_duration_since(t) >= L2_MIN_INTERVAL);
        let stuck_with_keyframes = undecoded_for.is_some_and(|d| d >= UNDECODED_L2)
            && self.last_keyframe_at.zip(self.unprogressed_since).is_some_and(|(k, u)| k > u);
        // Once per episode: a reset cannot conjure a keyframe the path is not delivering.
        let asks_unanswered = self.episode.is_some_and(|e| e.asks >= 2 && !e.reset_done);
        if reset_ok && (stuck_with_keyframes || (asks_unanswered && self.ask_due(now))) {
            self.on_reset(now);
            self.note_ask(now);
            return Some(RecoveryAction::ResetDecoder);
        }

        // --- L1: ask for a keyframe -----------------------------------------
        if self.ask_due(now) {
            self.note_ask(now);
            return Some(RecoveryAction::RequestKeyframe);
        }
        None
    }

    /// What a tile should show.
    pub(super) fn view_state(&self, now: Instant) -> ViewState {
        if self.last_stalled {
            return ViewState::SharerStalled;
        }
        if self.episode.is_some_and(|e| now.saturating_duration_since(e.since) >= RECOVERING_SHOW_AFTER) {
            return ViewState::Recovering;
        }
        let silent_for = now.saturating_duration_since(self.last_arrival.unwrap_or(self.created_at));
        if self.flowing() && silent_for >= SILENCE_SHOW_AFTER {
            return ViewState::Recovering;
        }
        ViewState::Healthy
    }

    /// Why the current episode started (logs, tests).
    pub(super) fn episode_reason(&self) -> Option<Reason> {
        self.episode.map(|e| e.reason)
    }
}

