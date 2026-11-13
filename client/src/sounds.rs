//! Notification sounds (task I9): four short generated sounds, so there are no files to ship.
//! Someone joins / leaves one of *my* streams (soft sine pairs, up and down), someone else starts / stops a
//! stream (rounder triangle notes, a rising chord and a low fall). The definitions are the replica's
//! (`superdoc/ui/replica/index.html` `SOUNDS`) and the browser's (`docs/js/ui/sounds.js`): keep the three equal.
//!
//! Three small pieces, none of them needing a sound card to test:
//! - [`synth`], a pure `(sound, volume) -> samples` function;
//! - [`RoomSounds`], a pure edge detector per joined room (the events in, the sounds to play out);
//! - [`play`], one short GStreamer burst on its own thread, behind a [`Gate`].
//!
//! `Why playback is off until the binary says so` ([`enable_output`]): the project rule is that nothing in a test
//! ever plays on the host. The `ui` tests run the real `AppState` and `UiAction::TestSound` with the lib compiled
//! without `cfg(test)`, so a `cfg(test)` switch would not reach them. Only `ui::App::new` (the real binary) turns
//! the output on; everything else (tests, `--gallery`) gets a `play` that returns at once.
//! `SSS_SOUND_SINK=<element>` replaces `autoaudiosink` (the rig sets `fakesink`: its PipeWire is private and an
//! `autoaudiosink` that finds no server may fall back to the host's ALSA).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app::AppSrc;
use crate::protocol::{ConnId, StreamId};

/// What the replica's `SOUNDS` call `join`, `leave`, `start` and `stop`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sound {
    /// Someone started watching one of my streams.
    Join,
    /// Someone stopped watching all of my streams.
    Leave,
    /// Someone else started a stream.
    Start,
    /// Someone else stopped a stream.
    Stop,
}

impl Sound {
    pub const ALL: [Sound; 4] = [Sound::Join, Sound::Leave, Sound::Start, Sound::Stop];

    fn index(self) -> usize {
        self as usize
    }

    fn triangle(self) -> bool {
        matches!(self, Sound::Start | Sound::Stop)
    }

    /// The notes in Hz, one every [`NOTE_GAP`].
    fn notes(self) -> &'static [f64] {
        match self {
            Sound::Join => &[659.0, 880.0],
            Sound::Leave => &[880.0, 587.0],
            Sound::Start => &[523.0, 659.0, 784.0],
            Sound::Stop => &[392.0, 294.0],
        }
    }
}

/// The synthesis sample rate; the sink resamples.
pub const RATE: u32 = 48_000;
/// Seconds between the starts of two notes.
const NOTE_GAP: f64 = 0.09;
/// Seconds a note sounds (its oscillator stops here).
const NOTE_LEN: f64 = 0.24;
/// Seconds from silence to the peak.
const ATTACK: f64 = 0.012;
/// Seconds at which the exponential release has reached the floor.
const RELEASE_END: f64 = 0.22;
/// The gain a note starts and ends at (-80 dB: WebAudio's exponential ramps cannot start at 0).
const FLOOR: f64 = 0.0001;
/// A note's peak at volume 100: sounds are a notice, never loud (`peak = volume / 100 * CAP`).
pub const CAP: f64 = 0.3;

/// The volume range of the setting (percent, step 5 in the UI).
pub const MAX_VOLUME: u32 = 100;
pub const DEFAULT_VOLUME: u32 = 50;

/// The gain of a note `t` seconds after it started: the replica's `setValueAtTime(FLOOR)`, an exponential ramp to
/// `peak` at [`ATTACK`], another to [`FLOOR`] at [`RELEASE_END`], held there until the oscillator stops.
fn envelope(t: f64, peak: f64) -> f64 {
    if t < ATTACK {
        FLOOR * (peak / FLOOR).powf(t / ATTACK)
    } else if t < RELEASE_END {
        peak * (FLOOR / peak).powf((t - ATTACK) / (RELEASE_END - ATTACK))
    } else {
        FLOOR
    }
}

/// The samples (mono, [`RATE`]) of `sound` at `volume` percent (0-100, more is 100). Empty at volume 0: a silent
/// sound is not worth a pipeline. Notes overlap (each starts [`NOTE_GAP`] after the previous one) and add up.
pub fn synth(sound: Sound, volume: u32) -> Vec<f32> {
    let peak = f64::from(volume.min(MAX_VOLUME)) / 100.0 * CAP;
    if peak <= 0.0 {
        return Vec::new();
    }
    let notes = sound.notes();
    let rate = f64::from(RATE);
    let note_samples = (NOTE_LEN * rate) as usize;
    let mut out = vec![0.0f32; ((notes.len() - 1) as f64 * NOTE_GAP * rate) as usize + note_samples];
    for (i, hz) in notes.iter().enumerate() {
        let start = (i as f64 * NOTE_GAP * rate) as usize;
        for n in 0..note_samples {
            let t = n as f64 / rate;
            let phase = (std::f64::consts::TAU * hz * t).sin();
            // WebAudio's triangle rises from 0 like the sine does: asin of the sine is that wave.
            let wave = if sound.triangle() { phase.asin() * 2.0 / std::f64::consts::PI } else { phase };
            out[start + n] += (wave * envelope(t, peak)) as f32;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Edge detection
// ---------------------------------------------------------------------------

/// How long after a room came back from a dropped connection nothing sounds: a relay that restarted has
/// everybody re-announce and re-watch over the next seconds ([`crate::session::RECONNECT_BACKOFF_CAP`] is the
/// longest an attempt waits), and none of that is news.
pub const RESUME_QUIET: Duration = Duration::from_secs(15);

/// One joined room's sound edges. Fed with what the room's events say, once per event pass
/// (`AppState::drain_events`); [`Self::drain`] says what to play. Pure: no clock of its own, no sound card.
///
/// The rules (user brief, D1): a sound for someone joining or leaving one of **my** streams (the viewer lists the
/// relay sends, spec 9.1) and for **someone else's** stream starting or stopping. Never for my own streams, the
/// join snapshot (it is not an event), a stream announced again under the same id (a label change), a stream the
/// roster never had, or what a resumed room replays ([`RESUME_QUIET`]).
#[derive(Debug, Default)]
pub struct RoomSounds {
    /// Nothing sounds before this (and what is fed until then only moves the baselines).
    quiet_until: Option<Instant>,
    /// Everyone watching at least one of my streams, as of the last pass.
    viewers: HashSet<ConnId>,
    /// My streams in this room, as of the last pass.
    mine: HashSet<StreamId>,
    pending: Vec<Sound>,
}

impl RoomSounds {
    /// A room that just resumed: stay quiet until `until`.
    pub fn quiet_until(&mut self, until: Instant) {
        self.quiet_until = Some(until);
    }

    /// The relay announced a stream. `mine`: it is ours; `known`: the roster already had this owner and stream id.
    pub fn stream_started(&mut self, mine: bool, known: bool) {
        if !mine && !known {
            self.pending.push(Sound::Start);
        }
    }

    /// The relay says a stream stopped. `known`: the roster had it (a stop for a stream it never saw is silent).
    pub fn stream_stopped(&mut self, mine: bool, known: bool) {
        if !mine && known {
            self.pending.push(Sound::Stop);
        }
    }

    /// Who watches what, once per pass: `mine` are my streams here and `lists` the relay's last viewer list of
    /// each stream it has one for. A person counts once however many of my streams they watch, and lists are
    /// compared as sets (the relay repeats one every 2 s), so a repeat is no change.
    /// A person who leaves because **my** stream stopped does not count as leaving: nothing sounds for the
    /// people of a stream that is gone from `mine` since the last pass.
    pub fn viewers<'a>(&mut self, mine: HashSet<StreamId>, lists: impl IntoIterator<Item = (StreamId, &'a [ConnId])>) {
        let now: HashSet<ConnId> = lists.into_iter().filter(|(id, _)| mine.contains(id)).flat_map(|(_, v)| v.iter().copied()).collect();
        let own_stop = self.mine.iter().any(|id| !mine.contains(id));
        if now.iter().any(|v| !self.viewers.contains(v)) {
            self.pending.push(Sound::Join);
        }
        if !own_stop && self.viewers.iter().any(|v| !now.contains(v)) {
            self.pending.push(Sound::Leave);
        }
        self.viewers = now;
        self.mine = mine;
    }

    /// [`Self::drain`], but nothing comes out unless `audible`. Always drains, so what happened while
    /// silent is gone and cannot sound later (picking up a call is not a burst of old news).
    pub fn drain_for(&mut self, now: Instant, audible: bool) -> Vec<Sound> {
        let played = self.drain(now);
        if audible { played } else { Vec::new() }
    }

    /// What to play for this pass: each sound at most once however many events asked for it (8 members joining
    /// in one frame are one sound), none while quiet.
    pub fn drain(&mut self, now: Instant) -> Vec<Sound> {
        let pending = std::mem::take(&mut self.pending);
        if self.quiet_until.is_some_and(|q| now < q) {
            return Vec::new();
        }
        Sound::ALL.into_iter().filter(|s| pending.contains(s)).collect()
    }
}

// ---------------------------------------------------------------------------
// Playback
// ---------------------------------------------------------------------------

/// The same sound is not started again within this long of its last start: 8 members joining over a second
/// are a sound, not a drum roll.
const MIN_GAP: Duration = Duration::from_millis(1000);
/// A burst that has not ended after this long is abandoned (a sink that never starts must not pile up threads).
const BURST_TIMEOUT: Duration = Duration::from_secs(5);

/// At most one burst of each sound playing, and [`MIN_GAP`] between two starts of one sound. Pure.
#[derive(Debug, Default)]
struct Gate {
    last: [Option<Instant>; 4],
    busy: [bool; 4],
}

impl Gate {
    fn admit(&mut self, sound: Sound, now: Instant) -> bool {
        let i = sound.index();
        if self.busy[i] || self.last[i].is_some_and(|t| now.duration_since(t) < MIN_GAP) {
            return false;
        }
        self.busy[i] = true;
        self.last[i] = Some(now);
        true
    }

    fn finish(&mut self, sound: Sound) {
        self.busy[sound.index()] = false;
    }
}

static GATE: Mutex<Gate> = Mutex::new(Gate { last: [None; 4], busy: [false; 4] });
static VOLUME: AtomicU32 = AtomicU32::new(DEFAULT_VOLUME);
static OUTPUT: AtomicBool = AtomicBool::new(false);
/// How often each sound was asked for (before the volume, output and gate checks): what the relay e2e test reads
/// to prove the events reach [`play`] without a sound card.
static REQUESTED: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

pub fn requested(sound: Sound) -> u32 {
    REQUESTED[sound.index()].load(Ordering::Relaxed)
}

/// The notification volume, percent; applied to every sound played from now on (the settings slider calls it on
/// every step of a drag).
pub fn set_volume(percent: u32) {
    VOLUME.store(percent.min(MAX_VOLUME), Ordering::Relaxed);
}

pub fn volume() -> u32 {
    VOLUME.load(Ordering::Relaxed)
}

/// Let [`play`] reach a sound card. Called by the real binary only (see the module doc).
pub fn enable_output() {
    OUTPUT.store(true, Ordering::Relaxed);
}

/// Play `sound` at the current volume, in the background: nothing at volume 0 or while the output is not enabled,
/// nothing when [`Gate`] says it played just now. A sink that cannot start is logged once and ignored.
pub fn play(sound: Sound) {
    REQUESTED[sound.index()].fetch_add(1, Ordering::Relaxed);
    let volume = volume();
    if volume == 0 || !OUTPUT.load(Ordering::Relaxed) {
        return;
    }
    if !GATE.lock().is_ok_and(|mut g| g.admit(sound, Instant::now())) {
        return;
    }
    let spawned = thread::Builder::new().name("notify-sound".into()).spawn(move || {
        if let Err(e) = burst(&synth(sound, volume)) {
            static ONCE: Once = Once::new();
            ONCE.call_once(|| tracing::warn!("notification sounds cannot play (further failures are not logged): {e:#}"));
        }
        if let Ok(mut g) = GATE.lock() {
            g.finish(sound);
        }
    });
    if spawned.is_err()
        && let Ok(mut g) = GATE.lock()
    {
        g.finish(sound);
    }
}

/// [`play`] each of `sounds`.
pub fn play_all(sounds: &[Sound]) {
    for s in sounds {
        play(*s);
    }
}

/// One burst: `appsrc ! audioconvert ! audioresample ! sink`, a single buffer, EOS, and the pipeline is dropped
/// when the sink has played it to the end.
fn burst(samples: &[f32]) -> Result<()> {
    let sink_name = std::env::var("SSS_SOUND_SINK").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "autoaudiosink".into());
    burst_to(samples, &sink_name)
}

/// [`burst`] into the sink element `sink_name` (the tests use `fakesink`: the pipeline runs, nothing is audible).
fn burst_to(samples: &[f32], sink_name: &str) -> Result<()> {
    gst::init().context("failed to initialize GStreamer")?;
    let make = |name: &str| gst::ElementFactory::make(name).build().with_context(|| format!("{name} element unavailable"));
    let src = make("appsrc")?;
    let convert = make("audioconvert")?;
    let resample = make("audioresample")?;
    let sink = make(sink_name)?;
    let pipeline = gst::Pipeline::new();
    pipeline.add_many([&src, &convert, &resample, &sink])?;
    gst::Element::link_many([&src, &convert, &resample, &sink])?;
    let appsrc = src.dynamic_cast::<AppSrc>().map_err(|_| anyhow!("appsrc element was not an AppSrc"))?;
    appsrc.set_caps(Some(
        &gst::Caps::builder("audio/x-raw")
            .field("format", "F32LE")
            .field("layout", "interleaved")
            .field("rate", RATE as i32)
            .field("channels", 1i32)
            .build(),
    ));
    appsrc.set_format(gst::Format::Time);
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let bus = pipeline.bus().ok_or_else(|| anyhow!("pipeline has no bus"))?;
    pipeline.set_state(gst::State::Playing).context("the sound pipeline did not start")?;
    let result = (|| {
        appsrc.push_buffer(gst::Buffer::from_slice(bytes)).map_err(|e| anyhow!("push failed: {e:?}"))?;
        appsrc.end_of_stream().map_err(|e| anyhow!("end of stream failed: {e:?}"))?;
        let timeout = gst::ClockTime::from_nseconds(BURST_TIMEOUT.as_nanos() as u64);
        match bus.timed_pop_filtered(timeout, &[gst::MessageType::Eos, gst::MessageType::Error]) {
            Some(msg) => match msg.view() {
                gst::MessageView::Error(e) => Err(anyhow!("{}", e.error())),
                _ => Ok(()),
            },
            None => Err(anyhow!("the sound did not end within {BURST_TIMEOUT:?}")),
        }
    })();
    let _ = pipeline.set_state(gst::State::Null);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peak(s: &[f32]) -> f64 {
        s.iter().fold(0.0f64, |m, x| m.max(f64::from(x.abs())))
    }

    /// Rising zero crossings in the first `secs`: the frequency of the first note.
    fn crossings(s: &[f32], secs: f64) -> usize {
        let n = (secs * f64::from(RATE)) as usize;
        s[..n].windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count()
    }

    #[test]
    fn the_four_sounds_differ_in_length_or_samples() {
        let all: Vec<Vec<f32>> = Sound::ALL.iter().map(|s| synth(*s, 100)).collect();
        for i in 0..4 {
            assert!(!all[i].is_empty());
            for j in i + 1..4 {
                assert_ne!(all[i], all[j], "{:?} and {:?} sound the same", Sound::ALL[i], Sound::ALL[j]);
            }
        }
    }

    #[test]
    fn the_first_note_has_the_replicas_pitch() {
        for s in Sound::ALL {
            let hz = s.notes()[0];
            // 60 ms of the first note: only it sounds (the second starts at 90 ms).
            let got = crossings(&synth(s, 100), 0.06) as f64;
            assert!((got - hz * 0.06).abs() <= 2.0, "{s:?}: {got} crossings in 60 ms, want about {}", hz * 0.06);
        }
    }

    #[test]
    fn the_peak_follows_the_volume_and_the_replicas_cap() {
        for s in Sound::ALL {
            for v in [5u32, 25, 50, 100] {
                let want = f64::from(v) / 100.0 * 0.3;
                let got = peak(&synth(s, v));
                // The overlap of a note's tail with the next one's attack adds a little (the replica's gain curve:
                // about 3 % at volume 100, 8 % at 5, where the -80 dB floor is relatively higher).
                assert!(got > want * 0.85 && got < want * 1.15, "{s:?} at {v}: peak {got}, want about {want}");
            }
            assert!(peak(&synth(s, 1000)) < 0.3 * 1.06, "more than 100 is 100");
        }
    }

    #[test]
    fn volume_zero_is_silent_and_empty() {
        for s in Sound::ALL {
            assert!(synth(s, 0).is_empty());
        }
    }

    #[test]
    fn a_sound_starts_and_ends_without_a_click() {
        for s in Sound::ALL {
            let v = synth(s, 100);
            assert_eq!(v[0], 0.0, "{s:?} starts at zero");
            assert!(f64::from(v[v.len() - 1].abs()) <= FLOOR * 1.5, "{s:?} ends at the -80 dB floor, not at {}", v[v.len() - 1]);
            // No sample-to-sample jump beyond what the highest note's slope allows (2 pi f / rate of the peak).
            let max_step = v.windows(2).map(|w| f64::from((w[1] - w[0]).abs())).fold(0.0, f64::max);
            assert!(max_step < 0.3 * 0.2, "{s:?}: a step of {max_step} is a click");
        }
    }

    #[test]
    fn a_sound_is_about_half_a_second_at_most() {
        for s in Sound::ALL {
            let secs = synth(s, 100).len() as f64 / f64::from(RATE);
            assert!((0.3..0.5).contains(&secs), "{s:?} lasts {secs} s");
        }
    }

    fn lists(l: &[(StreamId, Vec<ConnId>)]) -> impl Iterator<Item = (StreamId, &[ConnId])> {
        l.iter().map(|(id, v)| (*id, v.as_slice()))
    }

    fn mine(ids: &[StreamId]) -> HashSet<StreamId> {
        ids.iter().copied().collect()
    }

    /// One pass of viewers, then what drains.
    fn pass(r: &mut RoomSounds, ids: &[StreamId], l: &[(StreamId, Vec<ConnId>)]) -> Vec<Sound> {
        r.viewers(mine(ids), lists(l));
        r.drain(Instant::now())
    }

    #[test]
    fn viewers_join_and_leave_once_per_change_and_repeats_are_silent() {
        let mut r = RoomSounds::default();
        assert!(pass(&mut r, &[3], &[]).is_empty(), "no list yet");
        assert_eq!(pass(&mut r, &[3], &[(3, vec![10])]), [Sound::Join]);
        // The relay repeats the list every 2 s: sets are compared, so nothing.
        for _ in 0..5 {
            assert!(pass(&mut r, &[3], &[(3, vec![10])]).is_empty());
        }
        assert_eq!(pass(&mut r, &[3], &[(3, vec![10, 11])]), [Sound::Join]);
        assert_eq!(pass(&mut r, &[3], &[(3, vec![11])]), [Sound::Leave]);
        assert_eq!(pass(&mut r, &[3], &[(3, vec![])]), [Sound::Leave]);
        assert!(pass(&mut r, &[3], &[(3, vec![])]).is_empty());
    }

    /// Not in the call: the detector still runs, nothing sounds, and picking up is no burst of old news.
    #[test]
    fn nothing_sounds_while_not_in_the_call_and_pick_up_is_no_burst() {
        let now = Instant::now();
        let mut r = RoomSounds::default();
        // Out of the call: a stream starts, someone watches mine, someone else's stream stops.
        r.stream_started(false, false);
        r.stream_stopped(false, true);
        r.viewers(mine(&[3]), lists(&[(3, vec![10])]));
        assert!(r.drain_for(now, false).is_empty());
        // Pick up: what happened before is gone, and the viewer is already part of the baseline.
        assert!(r.drain_for(now, true).is_empty(), "the discarded edges must not come back");
        assert!(pass(&mut r, &[3], &[(3, vec![10])]).is_empty(), "a viewer from before pick-up is no join");
        // After pick-up news sounds again.
        r.stream_started(false, false);
        assert_eq!(r.drain_for(now, true), [Sound::Start]);
    }

    #[test]
    fn a_person_across_several_streams_of_mine_counts_once() {
        let mut r = RoomSounds::default();
        // Watching the screen and then the webcam: one join, not two.
        assert_eq!(pass(&mut r, &[3, 4], &[(3, vec![10])]), [Sound::Join]);
        assert!(pass(&mut r, &[3, 4], &[(3, vec![10]), (4, vec![10])]).is_empty());
        // Stops watching one of them: still watching, nobody left.
        assert!(pass(&mut r, &[3, 4], &[(3, vec![]), (4, vec![10])]).is_empty());
        assert_eq!(pass(&mut r, &[3, 4], &[(3, vec![]), (4, vec![])]), [Sound::Leave]);
        // Both lists change in one pass: the sounds are still one each.
        assert_eq!(pass(&mut r, &[3, 4], &[(3, vec![10, 12]), (4, vec![11])]), [Sound::Join]);
        assert_eq!(pass(&mut r, &[3, 4], &[(3, vec![]), (4, vec![])]), [Sound::Leave]);
    }

    #[test]
    fn someone_joining_while_another_leaves_in_one_pass_sounds_both() {
        let mut r = RoomSounds::default();
        pass(&mut r, &[3], &[(3, vec![10])]);
        assert_eq!(pass(&mut r, &[3], &[(3, vec![11])]), [Sound::Join, Sound::Leave]);
    }

    #[test]
    fn stopping_my_own_stream_is_not_everyone_leaving() {
        let mut r = RoomSounds::default();
        pass(&mut r, &[3], &[(3, vec![10, 11])]);
        // I stopped stream 3; its lists are still there for a while and then go.
        assert!(pass(&mut r, &[], &[(3, vec![10, 11])]).is_empty());
        assert!(pass(&mut r, &[], &[]).is_empty());
        // A new stream of mine starts from nobody: whoever watches it is a join.
        assert_eq!(pass(&mut r, &[5], &[(5, vec![10])]), [Sound::Join]);
        // Stopping one of two streams while a viewer of the other leaves in the same pass: no leave sound
        // (the stop explains the people of that stream and one pass cannot tell them apart; rare).
        pass(&mut r, &[5, 6], &[(5, vec![10]), (6, vec![11])]);
        assert!(pass(&mut r, &[5], &[(5, vec![10])]).is_empty());
    }

    #[test]
    fn viewer_lists_of_streams_that_are_not_mine_are_ignored() {
        let mut r = RoomSounds::default();
        assert!(pass(&mut r, &[], &[(9, vec![10])]).is_empty());
        assert!(pass(&mut r, &[3], &[(9, vec![10])]).is_empty());
    }

    #[test]
    fn a_relay_without_lists_never_sounds_for_viewers() {
        let mut r = RoomSounds::default();
        for _ in 0..3 {
            assert!(pass(&mut r, &[3], &[]).is_empty());
        }
    }

    #[test]
    fn someone_elses_streams_start_and_stop() {
        let mut r = RoomSounds::default();
        r.stream_started(false, false);
        assert_eq!(r.drain(Instant::now()), [Sound::Start]);
        r.stream_stopped(false, true);
        assert_eq!(r.drain(Instant::now()), [Sound::Stop]);
        // My own stream, either way: nothing.
        r.stream_started(true, false);
        r.stream_stopped(true, true);
        // The same stream announced again (a label change), a stop of a stream the roster never had.
        r.stream_started(false, true);
        r.stream_stopped(false, false);
        assert!(r.drain(Instant::now()).is_empty());
    }

    #[test]
    fn many_events_in_one_pass_are_one_sound_each() {
        let mut r = RoomSounds::default();
        for _ in 0..8 {
            r.stream_started(false, false);
        }
        r.stream_stopped(false, true);
        assert_eq!(r.drain(Instant::now()), [Sound::Start, Sound::Stop]);
        assert!(r.drain(Instant::now()).is_empty(), "drained");
    }

    #[test]
    fn a_resumed_room_is_quiet_for_a_while_and_keeps_its_baselines() {
        let t0 = Instant::now();
        let mut r = RoomSounds::default();
        r.quiet_until(t0 + RESUME_QUIET);
        r.stream_started(false, false);
        r.viewers(mine(&[3]), lists(&[(3, vec![10])]));
        assert!(r.drain(t0 + Duration::from_secs(1)).is_empty(), "the replay is silent");
        // The viewer who came back is the baseline now: the repeat after the quiet time is no join.
        r.viewers(mine(&[3]), lists(&[(3, vec![10])]));
        assert!(r.drain(t0 + RESUME_QUIET + Duration::from_secs(1)).is_empty());
        r.stream_started(false, false);
        assert_eq!(r.drain(t0 + RESUME_QUIET + Duration::from_secs(1)), [Sound::Start]);
    }

    #[test]
    fn the_gate_lets_one_burst_of_a_sound_through_and_spaces_the_next() {
        let t0 = Instant::now();
        let mut g = Gate::default();
        assert!(g.admit(Sound::Join, t0));
        assert!(!g.admit(Sound::Join, t0 + Duration::from_millis(1)), "busy");
        assert!(g.admit(Sound::Leave, t0), "another sound is its own");
        g.finish(Sound::Join);
        assert!(!g.admit(Sound::Join, t0 + Duration::from_millis(500)), "too soon after the last start");
        assert!(g.admit(Sound::Join, t0 + MIN_GAP));
    }

    /// The playback pipeline (`appsrc ! audioconvert ! audioresample ! sink`) takes a synthesized sound to EOS. Into a
    /// `fakesink`: it never touches an audio device or PipeWire.
    #[test]
    fn a_burst_runs_to_the_end_through_a_fakesink() {
        for s in Sound::ALL {
            burst_to(&synth(s, 50), "fakesink").unwrap_or_else(|e| panic!("{s:?}: {e:#}"));
        }
    }

    #[test]
    fn nothing_plays_in_tests_and_a_zero_volume_makes_no_pipeline() {
        // The output switch is off in every test binary: `play` must return before it reaches GStreamer.
        assert!(!OUTPUT.load(Ordering::Relaxed));
        play(Sound::Join);
        play_all(&Sound::ALL);
        assert!(GATE.lock().unwrap().last.iter().all(Option::is_none), "no gate was even asked");
    }
}
