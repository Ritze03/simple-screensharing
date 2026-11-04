//! The viewer's per-stream timing report (row 133): where a frame spends its time between its first
//! fragment and the screen, keyframes against the rest, as one `video timing` log line per
//! [`REPORT_EVERY`] plus one line with the frames around the worst moment.
//!
//! `Why:` "the stutter on keyframes is still there" (user, 2026-11-04) after a fix that was measured
//! on the rig only. The rig is loopback with a timer-paced output; only the user's machine can say
//! which stage hitches, and the uploaded client log is the one thing that comes back with no effort
//! from them. So every stage is measured all the time, at one mutex and a few instants per frame.
//!
//! The honest stutter number is `late`: shown time minus PTS, above the window's minimum. A gap
//! between shown frames alone proves nothing on a damage-driven source, where frames are simply
//! not evenly spaced; a frame that is *late* against the sender's own clock is a hitch.
//!
//! Pure (`now` injected), so it is tested without sleeping; `Shared` holds one per stream.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::fragment::FrameArrival;

/// How often a report is due.
const REPORT_EVERY: Duration = Duration::from_secs(10);
/// A frame younger than this may still be on its way to the screen: it waits for the next report.
const SETTLE: Duration = Duration::from_millis(300);
/// At most this many frames are kept (10 s at 240 fps is 2400).
const MAX_FRAMES: usize = 4096;
/// How far back a decoded or shown frame is looked for.
const SEARCH_BACK: usize = 256;
/// The frames right behind a keyframe that count as "after the keyframe": they queue behind it.
const AFTER_KEY: u32 = 3;
/// Frames before and after the worst one in the `worst` line.
const CONTEXT: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Key,
    AfterKey,
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    pts_us: u64,
    keyframe: bool,
    bytes: usize,
    arrival: Option<FrameArrival>,
    fed: Instant,
    decoded: Option<Instant>,
    shown: Option<Instant>,
}

/// Median, 99th percentile and maximum of a set of milliseconds.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct Spread(Vec<f64>);

impl Spread {
    pub(crate) fn add(&mut self, ms: f64) {
        self.0.push(ms);
    }

    pub(crate) fn count(&self) -> usize {
        self.0.len()
    }

    fn sorted(&self) -> Vec<f64> {
        let mut v = self.0.clone();
        v.sort_by(f64::total_cmp);
        v
    }

    fn median(&self) -> f64 {
        let v = self.sorted();
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    }

    /// `median/p99/max`.
    pub(crate) fn three(&self) -> String {
        let v = self.sorted();
        if v.is_empty() {
            return "-".into();
        }
        let p99 = v[(v.len() * 99 / 100).min(v.len() - 1)];
        format!("{:.1}/{:.1}/{:.1}", v[v.len() / 2], p99, v[v.len() - 1])
    }
}

/// One figure, split by where the frame stands relative to a keyframe.
#[derive(Debug, Default)]
struct ByPhase {
    key: Spread,
    after: Spread,
    other: Spread,
}

impl ByPhase {
    fn add(&mut self, phase: Phase, ms: f64) {
        match phase {
            Phase::Key => self.key.add(ms),
            Phase::AfterKey => self.after.add(ms),
            Phase::Other => self.other.add(ms),
        }
    }

    fn text(&self) -> String {
        format!("key={} after={} other={}", self.key.three(), self.after.three(), self.other.three())
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn between(from: Instant, to: Instant) -> f64 {
    ms(to.saturating_duration_since(from))
}

/// Records one stream's frames and turns them into the report lines.
#[derive(Debug, Default)]
pub(super) struct Timeline {
    frames: VecDeque<Entry>,
    last_report: Option<Instant>,
    /// Frames since the last keyframe, carried across reports.
    since_key: u32,
    last_complete: Option<Instant>,
    last_decoded: Option<Instant>,
    last_shown: Option<(Instant, u64)>,
}

impl Timeline {
    /// The access unit went to the decoder thread.
    pub(super) fn note_fed(&mut self, now: Instant, pts_us: u64, keyframe: bool, bytes: usize, arrival: Option<FrameArrival>) {
        if self.frames.len() >= MAX_FRAMES {
            self.frames.pop_front();
        }
        self.frames.push_back(Entry { pts_us, keyframe, bytes, arrival, fed: now, decoded: None, shown: None });
    }

    fn find(&mut self, pts_us: u64) -> Option<&mut Entry> {
        self.frames.iter_mut().rev().take(SEARCH_BACK).find(|e| e.pts_us == pts_us)
    }

    /// The frame left the decoder.
    pub(super) fn note_decoded(&mut self, now: Instant, pts_us: u64) {
        if let Some(e) = self.find(pts_us) {
            e.decoded.get_or_insert(now);
        }
    }

    /// The frame went to the GPU for the next repaint. A frame replaced before that is never shown.
    pub(super) fn note_shown(&mut self, now: Instant, pts_us: u64) {
        if let Some(e) = self.find(pts_us) {
            e.shown.get_or_insert(now);
        }
    }

    /// The report lines, when one is due and there is anything to say.
    pub(super) fn report(&mut self, now: Instant) -> Option<Vec<String>> {
        let last = *self.last_report.get_or_insert(now);
        let window = now.saturating_duration_since(last);
        if window < REPORT_EVERY {
            return None;
        }
        self.last_report = Some(now);
        let settled = self.frames.iter().take_while(|e| now.saturating_duration_since(e.fed) >= SETTLE).count();
        let frames: Vec<Entry> = self.frames.drain(..settled).collect();
        if frames.is_empty() {
            return None;
        }
        Some(self.analyze(&frames, window, now))
    }

    fn analyze(&mut self, frames: &[Entry], window: Duration, now: Instant) -> Vec<String> {
        let phases: Vec<Phase> = frames
            .iter()
            .map(|e| {
                if e.keyframe {
                    self.since_key = 0;
                    Phase::Key
                } else {
                    self.since_key = self.since_key.saturating_add(1);
                    if self.since_key <= AFTER_KEY { Phase::AfterKey } else { Phase::Other }
                }
            })
            .collect();

        // Everything is compared with the sender's clock through one offset per window: an Instant
        // as microseconds after `base`, minus the PTS. The smallest value is the path's floor.
        let base = frames[0].fed;
        let behind = |at: Instant, pts: u64| at.saturating_duration_since(base).as_secs_f64() * 1e6 - pts as f64;
        let floor = |of: &dyn Fn(&Entry) -> Option<Instant>| {
            frames.iter().filter_map(|e| of(e).map(|at| behind(at, e.pts_us))).fold(f64::INFINITY, f64::min)
        };
        let complete_at = |e: &Entry| e.arrival.map(|a| a.complete);
        let shown_at = |e: &Entry| e.shown;
        let (complete_floor, shown_floor) = (floor(&complete_at), floor(&shown_at));

        let (mut span, mut decode, mut complete_gap, mut out_gap, mut shown_gap) =
            (ByPhase::default(), ByPhase::default(), ByPhase::default(), ByPhase::default(), ByPhase::default());
        let (mut net_late, mut late, mut queue, mut pts_gap) =
            (ByPhase::default(), ByPhase::default(), Spread::default(), Spread::default());
        let (mut key_bytes, mut delta_bytes) = (Spread::default(), Spread::default());
        let (mut held, mut held_worst, mut repaired, mut undecoded, mut unshown) = (0u32, 0f64, 0u32, 0u32, 0u32);
        let mut worst: Option<(f64, usize)> = None;
        for (i, (e, &phase)) in frames.iter().zip(&phases).enumerate() {
            if e.keyframe { key_bytes.add(e.bytes as f64 / 1000.0) } else { delta_bytes.add(e.bytes as f64 / 1000.0) }
            if let Some(a) = e.arrival {
                span.add(phase, between(a.first_fragment, a.complete));
                queue.add(between(a.released, e.fed));
                net_late.add(phase, (behind(a.complete, e.pts_us) - complete_floor) / 1000.0);
                if let Some(prev) = self.last_complete.replace(a.complete) {
                    complete_gap.add(phase, between(prev, a.complete));
                }
                let hold = between(a.complete, a.released);
                if hold >= 0.5 {
                    held += 1;
                    held_worst = held_worst.max(hold);
                }
                repaired += u32::from(a.repaired);
            }
            match e.decoded {
                Some(at) => {
                    decode.add(phase, between(e.fed, at));
                    if let Some(prev) = self.last_decoded.replace(at) {
                        out_gap.add(phase, between(prev, at));
                    }
                }
                None => undecoded += 1,
            }
            match e.shown {
                Some(at) => {
                    let value = (behind(at, e.pts_us) - shown_floor) / 1000.0;
                    late.add(phase, value);
                    if worst.is_none_or(|(w, _)| value > w) {
                        worst = Some((value, i));
                    }
                    if let Some((prev, prev_pts)) = self.last_shown.replace((at, e.pts_us)) {
                        shown_gap.add(phase, between(prev, at));
                        pts_gap.add(e.pts_us.saturating_sub(prev_pts) as f64 / 1000.0);
                    }
                }
                None => unshown += u32::from(e.decoded.is_some()),
            }
        }

        // A clearly late frame, and how many of those are at a keyframe: says at once whether the
        // hitches belong to keyframes at all.
        let threshold = (late.other.median() * 2.0).max(late.other.median() + 8.0);
        let is_late = |v: &&f64| **v > threshold;
        let late_key = late.key.0.iter().filter(is_late).count() + late.after.0.iter().filter(is_late).count();
        let late_other = late.other.0.iter().filter(is_late).count();

        // Wall clock of the floor: on one machine, against the sender's line, the one-way time.
        let unix_now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64() * 1e6;
        let base_unix = unix_now - now.saturating_duration_since(base).as_secs_f64() * 1e6;
        let keys = frames.iter().filter(|e| e.keyframe).count();
        let mut lines = vec![format!(
            "win_s={:.1} frames={} keyframes={keys} key_kB={} delta_kB={} pts_gap_ms={} span_ms[{}] net_late_ms[{}] arrive_gap_ms[{}] \
             queue_ms={} decode_ms[{}] out_gap_ms[{}] shown_gap_ms[{}] late_ms[{}] late_frames key={late_key} other={late_other} \
             held={held} held_worst_ms={held_worst:.1} repaired={repaired} undecoded={undecoded} unshown={unshown} pts_unix_floor_us={:.0}",
            window.as_secs_f64(),
            frames.len(),
            key_bytes.three(),
            delta_bytes.three(),
            pts_gap.three(),
            span.text(),
            net_late.text(),
            complete_gap.text(),
            queue.three(),
            decode.text(),
            out_gap.text(),
            shown_gap.text(),
            late.text(),
            base_unix + complete_floor,
        )];
        if let Some((value, i)) = worst {
            lines.push(worst_line(frames, i, value));
        }
        lines
    }
}

/// The frames around the worst one, each stage in ms after the first listed frame's arrival:
/// `pts K|d kB complete/decoded/shown`, `-` when a frame never got there.
fn worst_line(frames: &[Entry], i: usize, late_ms: f64) -> String {
    let from = i.saturating_sub(CONTEXT);
    let to = (i + CONTEXT + 1).min(frames.len());
    let origin = frames[from].arrival.map_or(frames[from].fed, |a| a.first_fragment);
    let at = |t: Option<Instant>| t.map_or("-".to_string(), |t| format!("{:.1}", between(origin, t)));
    let mut out = format!("worst late_ms={late_ms:.1}:");
    for (n, e) in frames[from..to].iter().enumerate() {
        let _ = write!(
            out,
            " {}{}[pts+{:.1} {}{:.1}kB {}/{}/{}]",
            if from + n == i { "*" } else { "" },
            n,
            e.pts_us.saturating_sub(frames[from].pts_us) as f64 / 1000.0,
            if e.keyframe { "K " } else { "" },
            e.bytes as f64 / 1000.0,
            at(e.arrival.map(|a| a.complete)),
            at(e.decoded),
            at(e.shown),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arrival(at: Instant) -> Option<FrameArrival> {
        Some(FrameArrival { first_fragment: at, complete: at, released: at, repaired: false })
    }

    #[test]
    fn nothing_is_said_before_a_report_is_due() {
        let mut t = Timeline::default();
        let start = Instant::now();
        assert!(t.report(start).is_none());
        t.note_fed(start, 0, true, 1000, arrival(start));
        assert!(t.report(start + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn a_late_keyframe_shows_as_late_at_the_keyframe_and_nowhere_else() {
        let mut t = Timeline::default();
        let start = Instant::now();
        assert!(t.report(start).is_none());
        for i in 0..600u64 {
            let at = start + Duration::from_micros(i * 16_000);
            let keyframe = i % 30 == 0;
            // Every keyframe reaches the screen 40 ms later than the others do.
            let shown = at + Duration::from_millis(if keyframe { 45 } else { 5 });
            t.note_fed(at, i * 16_000, keyframe, if keyframe { 200_000 } else { 2_000 }, arrival(at));
            t.note_decoded(at + Duration::from_millis(3), i * 16_000);
            t.note_shown(shown, i * 16_000);
        }
        let lines = t.report(start + Duration::from_secs(11)).expect("due");
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("frames=600 keyframes=20"), "{}", lines[0]);
        assert!(lines[0].contains("late_frames key=20 other=0"), "{}", lines[0]);
        assert!(lines[1].starts_with("worst late_ms=40.0"), "{}", lines[1]);
        assert!(lines[1].contains("K 200.0kB"), "{}", lines[1]);
    }

    #[test]
    fn frames_still_on_their_way_wait_for_the_next_report() {
        let mut t = Timeline::default();
        let start = Instant::now();
        assert!(t.report(start).is_none());
        let late = start + Duration::from_secs(10);
        t.note_fed(start, 0, true, 1000, arrival(start));
        t.note_fed(late, 1000, false, 1000, arrival(late));
        let lines = t.report(late + Duration::from_millis(1)).expect("due");
        assert!(lines[0].contains("frames=1 "), "{}", lines[0]);
        assert_eq!(t.frames.len(), 1);
    }

    #[test]
    fn a_frame_never_decoded_or_shown_is_counted_not_a_crash() {
        let mut t = Timeline::default();
        let start = Instant::now();
        assert!(t.report(start).is_none());
        t.note_fed(start, 0, true, 1000, None);
        t.note_fed(start, 1, false, 1000, arrival(start));
        t.note_decoded(start, 1);
        let lines = t.report(start + Duration::from_secs(10)).expect("due");
        assert!(lines[0].contains("undecoded=1 unshown=1"), "{}", lines[0]);
        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn the_window_stays_bounded() {
        let mut t = Timeline::default();
        let start = Instant::now();
        for i in 0..(MAX_FRAMES as u64 + 10) {
            t.note_fed(start, i, false, 1, None);
        }
        assert_eq!(t.frames.len(), MAX_FRAMES);
    }
}
