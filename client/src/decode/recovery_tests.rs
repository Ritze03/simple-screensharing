//! Unit tests for the viewer-recovery state machine (`recovery.rs`): injected
//! `Instant`s, no sleeping, no GStreamer.

use std::time::{Duration, Instant};

use super::recovery::*;
use crate::fragment::FrameLoss;

const MS: fn(u64) -> Duration = Duration::from_millis;
const FRAME: u64 = 16; // ms, ~60 fps

fn obs(keyframe: bool) -> FrameObs {
    FrameObs { keyframe, stalled: false, loss: FrameLoss::default() }
}

fn lost(keyframe: bool) -> FrameLoss {
    FrameLoss { frames: if keyframe { 1 } else { 3 }, keyframe, ..Default::default() }
}

/// A scripted viewer: 60 fps arrivals, the decoder echoing every fed AU, a
/// tick every 100 ms, a scheduled keyframe every 2 s (unless `no_keyframes`).
struct Sim {
    r: Recovery,
    now: Instant,
    decoded: u64,
    next_key: Instant,
    since_tick: u64,
    actions: Vec<(Duration, RecoveryAction)>,
    t0: Instant,
    decoder_wedged: bool,
}

impl Sim {
    fn new() -> Self {
        let t0 = Instant::now();
        Self {
            r: Recovery::new(t0),
            now: t0,
            decoded: 0,
            next_key: t0,
            since_tick: 0,
            actions: vec![],
            t0,
            decoder_wedged: false,
        }
    }

    /// One frame at `self.now`, with `loss`; ticks when due. Returns whether it was fed.
    fn frame(&mut self, loss: FrameLoss, keyframes: bool) -> bool {
        let key = keyframes && self.now >= self.next_key;
        if key {
            self.next_key = self.now + MS(2000);
        }
        let fed = self.r.on_frame(self.now, FrameObs { keyframe: key, stalled: false, loss });
        if fed {
            self.r.on_fed(self.now);
            if !self.decoder_wedged {
                self.decoded += 1;
            }
        }
        fed
    }

    fn tick(&mut self) -> Option<RecoveryAction> {
        let a = self.r.tick(self.now, self.decoded, false);
        if let Some(a) = a {
            self.actions.push((self.now - self.t0, a));
            if a == RecoveryAction::ResetDecoder {
                self.decoder_wedged = false;
            }
        }
        a
    }

    /// Run `ms` of a stream where every frame arrives (60 fps, ticks every 100 ms).
    fn run(&mut self, ms: u64, keyframes: bool) {
        let end = self.now + MS(ms);
        while self.now < end {
            self.now += MS(FRAME);
            self.frame(FrameLoss::default(), keyframes);
            self.since_tick += FRAME;
            if self.since_tick >= 100 {
                self.since_tick = 0;
                self.tick();
            }
        }
    }

    /// Silence for `ms`, ticking every 100 ms.
    fn silence(&mut self, ms: u64) -> Vec<RecoveryAction> {
        let end = self.now + MS(ms);
        let mut out = vec![];
        while self.now < end {
            self.now += MS(100);
            out.extend(self.tick());
        }
        out
    }
}

#[test]
fn a_clean_stream_never_acts_and_learns_the_gop() {
    let mut s = Sim::new();
    s.run(30_000, true);
    assert!(s.actions.is_empty(), "{:?}", s.actions);
    assert_eq!(s.r.view_state(s.now), ViewState::Healthy);
    assert_eq!(s.r.asks_total, 0);
}

#[test]
fn deltas_before_the_first_keyframe_are_dropped() {
    let t = Instant::now();
    let mut r = Recovery::new(t);
    assert!(!r.on_frame(t, obs(false)));
    assert!(r.on_frame(t + MS(10), obs(true)));
    assert!(r.on_frame(t + MS(20), obs(false)));
}

#[test]
fn a_lost_keyframe_asks_at_once_keeps_decoding_and_the_new_idr_ends_it() {
    let mut s = Sim::new();
    s.run(3000, true);
    s.now += MS(FRAME);
    assert!(s.frame(lost(true), true), "loss evidence alone never freezes the picture (see the module docs)");
    assert_eq!(s.r.episode_reason(), Some(Reason::KeyframeLost));
    s.now += MS(FRAME);
    assert!(s.frame(FrameLoss::default(), false));
    s.now += MS(100);
    assert_eq!(s.tick(), Some(RecoveryAction::RequestKeyframe));
    // The requested IDR arrives ~1.2 s later.
    s.now += MS(1200);
    assert!(s.r.on_frame(s.now, obs(true)));
    assert!(s.r.episode_reason().is_none());
    assert!(s.r.on_frame(s.now + MS(16), obs(false)));
}

#[test]
fn a_lost_delta_early_in_the_gop_asks_but_keeps_decoding() {
    let mut s = Sim::new();
    s.run(2500, true); // a keyframe at ~2.0 s, so ~0.5 s old
    s.now += MS(FRAME);
    assert!(s.frame(lost(false), true), "a single lost delta does not freeze the picture");
    assert_eq!(s.tick(), Some(RecoveryAction::RequestKeyframe));
}

#[test]
fn a_lost_delta_late_in_the_gop_waits_for_the_scheduled_keyframe() {
    let mut s = Sim::new();
    s.run(3500, true); // keyframes at 0, 2.0 s: now 1.5 s into the next GOP
    s.now += MS(FRAME);
    assert!(s.frame(lost(false), true));
    assert_eq!(s.tick(), None, "a request (~1.3 s) would arrive after the scheduled IDR (~0.5 s away)");
    assert!(s.r.episode_reason().is_none());
}

#[test]
fn requests_are_rate_limited_and_back_off() {
    let mut s = Sim::new();
    s.run(3000, true);
    s.now += MS(FRAME);
    s.frame(lost(true), true);
    let t_loss = s.now - s.t0;
    // Deltas keep arriving (and are dropped) but no keyframe ever does.
    s.run(60_000, false);
    let times: Vec<Duration> = s.actions.iter().map(|(t, _)| *t - t_loss.min(*t)).collect();
    assert!(s.actions.len() >= 5, "{:?}", s.actions);
    let gaps: Vec<u128> = s.actions.windows(2).map(|w| (w[1].0 - w[0].0).as_millis()).collect();
    assert!(gaps.iter().all(|g| *g >= 1500), "never faster than 1.5 s: {gaps:?} ({times:?})");
    assert!((1500..1750).contains(&gaps[0]), "first re-ask after 1.5 s: {gaps:?}");
    assert!((3000..3250).contains(&gaps[1]), "then 3 s: {gaps:?}");
    assert!((9900..10_250).contains(gaps.last().unwrap()), "the backoff is capped at 10 s: {gaps:?}");
}

#[test]
fn two_unanswered_requests_escalate_to_one_decoder_reset_per_episode() {
    let mut s = Sim::new();
    s.run(3000, true);
    s.now += MS(FRAME);
    s.frame(lost(true), true);
    s.run(60_000, false);
    let kinds: Vec<RecoveryAction> = s.actions.iter().map(|a| a.1).collect();
    assert_eq!(&kinds[..3], &[RecoveryAction::RequestKeyframe, RecoveryAction::RequestKeyframe, RecoveryAction::ResetDecoder]);
    assert_eq!(s.r.resets_total, 1, "one reset per episode, a reset cannot create a keyframe the path does not deliver");
}

#[test]
fn fed_but_nothing_decoded_asks_after_a_second_and_resets_after_three_while_keyframes_arrive() {
    let mut s = Sim::new();
    s.run(3000, true);
    s.decoder_wedged = true; // output stops, input keeps flowing, keyframes included
    let t_wedge = s.now - s.t0;
    s.run(8000, true);
    let first = s.actions.first().copied().expect("must act");
    assert_eq!(first.1, RecoveryAction::RequestKeyframe);
    let after = first.0 - t_wedge;
    assert!(after >= MS(1000) && after < MS(1400), "L1 at ~1 s of fed-without-output: {after:?}");
    let reset = s.actions.iter().find(|a| a.1 == RecoveryAction::ResetDecoder).copied().expect("must reset");
    let after = reset.0 - t_wedge;
    assert!(after >= MS(3000) && after < MS(3700), "L2 at ~3 s: {after:?}");
    // The reset cured it in this script: no further resets.
    assert_eq!(s.r.resets_total, 1);
}

#[test]
fn a_decoder_that_resumes_cancels_the_undecoded_clock() {
    let t0 = Instant::now();
    let mut r = Recovery::new(t0);
    r.on_frame(t0, obs(true));
    for i in 0..10u64 {
        r.on_fed(t0 + MS(i * 16));
    }
    assert_eq!(r.tick(t0 + MS(900), 0, false), None);
    assert_eq!(r.tick(t0 + MS(1200), 3, false), None, "output appeared");
    assert_eq!(r.tick(t0 + MS(5000), 3, false), None, "nothing fed since, so nothing to be stuck on");
    assert_eq!(r.asks_total, 0);
}

#[test]
fn a_slow_trickle_is_not_a_stuck_decoder() {
    // Two AUs in 2 s with no output yet must not read as a wedge (needs >= 3 fed).
    let t0 = Instant::now();
    let mut r = Recovery::new(t0);
    r.on_frame(t0, obs(true));
    r.on_fed(t0);
    r.on_fed(t0 + MS(1000));
    assert_eq!(r.tick(t0 + MS(2000), 0, false), None);
}

#[test]
fn a_blackout_is_gap_evidence_that_asks_when_frames_resume() {
    let mut s = Sim::new();
    s.run(3000, true);
    assert!(s.silence(5000).is_empty(), "no action during a 5 s blackout: nothing can be asked of a dead path, and no L3 before 8 s");
    s.now += MS(FRAME);
    assert!(s.frame(FrameLoss::default(), false), "frames resume: decode them, artifacts and all, while the keyframe is on its way");
    assert_eq!(s.r.episode_reason(), Some(Reason::Gap));
    s.now += MS(100);
    assert_eq!(s.tick(), Some(RecoveryAction::RequestKeyframe));
}

#[test]
fn a_short_gap_late_in_the_gop_does_not_ask() {
    let mut s = Sim::new();
    s.run(3500, true);
    s.silence(700);
    assert!(s.frame(FrameLoss::default(), false), "0.7 s hole: keep decoding");
    assert_eq!(s.tick(), None, "the scheduled IDR is ~0.5 s away");
}

#[test]
fn a_short_gap_early_in_the_gop_asks_without_gating() {
    let mut s = Sim::new();
    s.run(2100, true); // keyframe at ~2.0 s, 0.1 s old
    s.silence(550); // ... 0.65 s old when the frames resume: a request (1.3 s) still beats the schedule (2 s)
    assert!(s.frame(FrameLoss::default(), false));
    s.now += MS(100);
    assert_eq!(s.tick(), Some(RecoveryAction::RequestKeyframe));
}

#[test]
fn silence_on_a_flowing_stream_reconnects_after_eight_seconds_only() {
    let mut s = Sim::new();
    s.run(3000, true);
    let sil = s.silence(7800);
    assert!(sil.is_empty(), "{sil:?}");
    assert_eq!(s.r.view_state(s.now), ViewState::Recovering, "silence >= 3 s on a flowing stream reads as recovering");
    let sil = s.silence(500);
    assert_eq!(sil.first(), Some(&RecoveryAction::Reconnect));
}

#[test]
fn silence_never_reconnects_a_stream_that_was_not_flowing() {
    let t0 = Instant::now();
    // Sparse: one frame every 2 s (a damage-driven low-activity share).
    let mut r = Recovery::new(t0);
    for i in 0..20u64 {
        r.on_frame(t0 + Duration::from_secs(i * 2), obs(i == 0));
    }
    assert_eq!(r.tick(t0 + Duration::from_secs(100), 0, false), None, "sparse stream");

    // Never delivered / just started.
    let mut fresh = Recovery::new(t0);
    assert_eq!(fresh.tick(t0 + Duration::from_secs(60), 0, false), None, "never delivered");
    fresh.on_frame(t0 + Duration::from_secs(1), obs(true));
    assert_eq!(fresh.tick(t0 + Duration::from_secs(60), 0, false), None, "just started");

    // Stalled sharer: alive and says so.
    let mut s = Sim::new();
    s.run(3000, true);
    s.now += MS(FRAME);
    s.r.on_frame(s.now, FrameObs { keyframe: true, stalled: true, loss: FrameLoss::default() });
    assert_eq!(s.r.tick(s.now + Duration::from_secs(60), s.decoded, false), None);
    assert_eq!(s.r.view_state(s.now + Duration::from_secs(60)), ViewState::SharerStalled);
}

#[test]
fn a_stalled_sharer_resending_keyframes_is_not_loss() {
    let mut s = Sim::new();
    s.run(3000, true);
    for _ in 0..20 {
        s.now += MS(500);
        assert!(s.r.on_frame(s.now, FrameObs { keyframe: true, stalled: true, loss: FrameLoss::default() }));
        s.r.on_fed(s.now);
        s.decoded += 1;
        assert_eq!(s.r.tick(s.now, s.decoded, false), None);
    }
    s.now += MS(500);
    assert!(s.r.on_frame(s.now, obs(false)), "resuming after a stall (0.5 s gaps) is not loss evidence");
    assert!(s.r.episode_reason().is_none());
}

#[test]
fn the_decoder_asking_for_a_keyframe_is_honoured() {
    let mut s = Sim::new();
    s.run(3000, true);
    assert_eq!(s.r.tick(s.now + MS(10), s.decoded, true), Some(RecoveryAction::RequestKeyframe));
    assert!(s.r.gate_closed());
}

#[test]
fn view_state_shows_recovering_only_after_a_grace_period() {
    let mut s = Sim::new();
    s.run(3000, true);
    s.now += MS(FRAME);
    let t0 = s.now;
    s.frame(lost(true), true);
    assert_eq!(s.r.view_state(t0 + MS(1000)), ViewState::Healthy, "a normal recovery must not flash the label");
    assert_eq!(s.r.view_state(t0 + MS(1600)), ViewState::Recovering);
    s.r.on_frame(t0 + MS(1700), obs(true));
    assert_eq!(s.r.view_state(t0 + MS(1800)), ViewState::Healthy);
}

#[test]
fn an_ingest_drop_is_treated_like_a_lost_delta() {
    let mut s = Sim::new();
    s.run(2300, true);
    s.r.on_ingest_drop(s.now, false);
    s.r.on_ingest_drop(s.now + MS(16), false);
    s.now += MS(100);
    assert_eq!(s.tick(), None, "two dropped AUs are not a burst");
    s.r.on_ingest_drop(s.now, false);
    s.now += MS(100);
    assert_eq!(s.tick(), Some(RecoveryAction::RequestKeyframe));
}

#[test]
fn a_single_lost_delta_never_asks() {
    let mut s = Sim::new();
    s.run(2300, true);
    for _ in 0..20 {
        s.run(200, false);
        s.frame(FrameLoss { frames: 1, ..Default::default() }, false);
    }
    assert!(s.actions.is_empty(), "isolated single losses are repaired by the scheduled keyframe: {:?}", s.actions);
}

#[test]
fn a_reset_closes_the_gate_and_starts_an_episode() {
    let mut s = Sim::new();
    s.run(3000, true);
    s.r.on_reset(s.now);
    assert!(s.r.gate_closed());
    assert!(s.r.episode_reason().is_some());
    assert_eq!(s.r.resets_total, 1);
}

#[test]
fn a_requested_keyframe_that_arrives_incomplete_is_asked_again_after_the_minimum_interval_not_the_backoff() {
    let mut s = Sim::new();
    s.run(3000, true);
    let mut asks = vec![];
    for round in 0..4 {
        s.now += MS(FRAME);
        s.frame(lost(true), false); // the answer to the previous request arrived, incomplete
        let end = s.now + MS(1450);
        while s.now < end {
            s.now += MS(100);
            if s.tick().is_some() {
                asks.push((round, s.now));
            }
        }
    }
    assert_eq!(asks.len(), 4, "one ask per lost IDR: {asks:?}");
    for w in asks.windows(2) {
        let gap = (w[1].1 - w[0].1).as_millis();
        assert!((1500..1800).contains(&gap), "re-asks are 1.5 s apart, not 1.5/3/6: {gap}");
    }
}

#[test]
fn losing_a_delta_every_gop_does_not_become_a_keyframe_storm() {
    let mut s = Sim::new();
    s.run(500, true);
    // 60 s of 60 fps, a keyframe every 2 s, and a lost delta ~0.3 s after every keyframe.
    let t0 = s.now;
    while s.now < t0 + Duration::from_secs(60) {
        s.run(2000, true);
        s.now += MS(FRAME);
        s.frame(lost(false), true);
    }
    let asks = s.r.asks_total;
    assert!(asks >= 3, "it still asks sometimes: {asks}");
    assert!(asks <= 12, "30 evidence events must not produce 30 requests: {asks}");
    let gaps: Vec<u128> = s.actions.windows(2).map(|w| (w[1].0 - w[0].0).as_millis()).collect();
    assert!(gaps.iter().all(|g| *g >= 1500), "{gaps:?}");
}

#[test]
fn delta_loss_requests_start_over_after_a_quiet_period() {
    let mut s = Sim::new();
    s.run(2300, true);
    s.frame(lost(false), true);
    s.run(200, false);
    let first = s.r.asks_total;
    assert_eq!(first, 1);
    // 15 s of nothing wrong (clean keyframes end the episode), then a loss again: asks at once.
    s.run(15_000, true);
    s.run(300, true);
    s.frame(lost(false), true);
    s.run(200, false);
    assert_eq!(s.r.asks_total, 2);
}

#[test]
fn fed_but_nothing_decoded_closes_the_gate_until_a_keyframe() {
    let mut s = Sim::new();
    s.run(3000, true);
    s.decoder_wedged = true;
    s.run(1500, false); // no scheduled keyframes: only deltas
    assert!(s.r.gate_closed(), "a decoder that takes nothing in must not be fed more deltas");
    assert!(s.r.on_frame(s.now + MS(16), obs(true)), "a keyframe always goes in");
    assert!(!s.r.gate_closed());
}

/// Browser-parity numbers the recovery contract promises, in one place.
#[test]
fn the_documented_thresholds() {
    assert_eq!(L1_MIN_INTERVAL, MS(1500));
    assert_eq!(REASK_BACKOFF.map(|d| d.as_millis()), [1500, 3000, 6000, 10_000]);
    assert_eq!(L3_SILENCE, Duration::from_secs(8));
    assert_eq!(UNDECODED_L1, Duration::from_secs(1));
    assert_eq!(UNDECODED_L2, Duration::from_secs(3));
}
