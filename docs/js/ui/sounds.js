// Notification sounds (task I9): four short generated sounds, so there are no files to ship. Someone joins / leaves one of MY streams
// (soft sine pairs, up and down), someone else starts / stops a stream (rounder triangle notes, a rising chord and a low fall).
// The definitions are the replica's (`superdoc/ui/replica/index.html` `SOUNDS`) and the native client's (`client/src/sounds.rs`): keep the
// three equal. The edge rules (`RoomSounds`) are a port of the native `sounds::RoomSounds`, same names and same tests.

import { S } from "./store.js";

export const SOUNDS = {
  join: { wave: "sine", notes: [659, 880] },
  leave: { wave: "sine", notes: [880, 587] },
  start: { wave: "triangle", notes: [523, 659, 784] },
  stop: { wave: "triangle", notes: [392, 294] },
};
export const SOUND_NAMES = ["join", "leave", "start", "stop"];
export const NOTE_GAP = 0.09; // seconds between the starts of two notes
export const SOUND_CAP = 0.3; // a note's peak at volume 100: peak = volume / 100 * SOUND_CAP
export const DEFAULT_SOUND_VOL = 50;
/** How long after a room came back from a dropped connection nothing sounds (a restarted relay replays everything). */
export const RESUME_QUIET_MS = 15_000;
/** The same sound is not started again within this long of its last start (8 people joining in a second are one sound). */
export const MIN_GAP_MS = 1000;

/** What to play: `{ wave, peak, notes: [{ at, hz }] }` (`at` in seconds from now), or `null` at volume 0 (nothing to start). Pure. */
export function soundPlan(name, volume) {
  const peak = (Math.min(Math.max(volume, 0), 100) / 100) * SOUND_CAP;
  const s = SOUNDS[name];
  if (!s || !peak) return null;
  return { wave: s.wave, peak, notes: s.notes.map((hz, i) => ({ at: i * NOTE_GAP, hz })) };
}

/** The stored volume as a clean 0-100 integer (anything else is the default). */
export const cleanSoundVol = (v) => (Number.isFinite(v) ? Math.min(100, Math.max(0, Math.round(v))) : DEFAULT_SOUND_VOL);

/**
 * One room's sound edges. Fed with what the room's events say, then `drain` says what to play. Pure: no clock, no AudioContext.
 * Rules: a sound for someone joining or leaving one of MY streams (the relay's viewer lists) and for SOMEONE ELSE's stream starting or
 * stopping; never for my own streams, the join snapshot (`streams(keys, false)` only sets the baseline), a label change (same key), or
 * what a resumed room replays (`quiet`).
 */
export class RoomSounds {
  constructor() {
    this.quietUntil = 0;
    this.keys = new Set(); // other people's stream keys, as of the last call
    this.viewerSet = new Set(); // everyone watching at least one of my streams
    this.mine = new Set(); // my stream ids
    this.pending = new Set();
  }
  /** A room that just resumed: nothing sounds before `until` (ms). */
  quiet(until) {
    this.quietUntil = until;
  }
  /** The streams of other people (keys). `joined`: the room is live, not still taking its snapshot. */
  streams(keys, joined) {
    const now = new Set(keys);
    if (joined) {
      for (const k of now) if (!this.keys.has(k)) this.pending.add("start");
      for (const k of this.keys) if (!now.has(k)) this.pending.add("stop");
    }
    this.keys = now;
  }
  /** Who watches what: `mine` = my stream ids here, `lists` = Map(stream id -> viewer ids). A person counts once however many of my streams they watch. */
  viewers(mine, lists) {
    const now = new Set();
    for (const [id, v] of lists) if (mine.has(id)) for (const c of v) now.add(c);
    const ownStop = [...this.mine].some((id) => !mine.has(id)); // my stream stopped: its people are not "leaving"
    if ([...now].some((c) => !this.viewerSet.has(c))) this.pending.add("join");
    if (!ownStop && [...this.viewerSet].some((c) => !now.has(c))) this.pending.add("leave");
    this.viewerSet = now;
    this.mine = new Set(mine);
  }
  /** What to play for this pass, each sound once, in the fixed order; none while quiet. */
  drain(nowMs) {
    const out = SOUND_NAMES.filter((n) => this.pending.has(n));
    this.pending.clear();
    return nowMs < this.quietUntil ? [] : out;
  }
}

/** Gate: `admit(name, now)` is false when that sound started less than `MIN_GAP_MS` ago. Pure. */
export class Gate {
  constructor() {
    this.last = {};
  }
  admit(name, nowMs) {
    if (name in this.last && nowMs - this.last[name] < MIN_GAP_MS) return false;
    this.last[name] = nowMs;
    return true;
  }
}

/* ---------------- playback (WebAudio, as the replica) ---------------- */

let ctx = null;
const gate = new Gate();
const ensureCtx = () => (ctx ??= new AudioContext());

function schedule(plan) {
  const c = ctx;
  for (const { at, hz } of plan.notes) {
    const t = c.currentTime + at, o = c.createOscillator(), g = c.createGain();
    o.type = plan.wave;
    o.frequency.value = hz;
    g.gain.setValueAtTime(0.0001, t);
    g.gain.exponentialRampToValueAtTime(plan.peak, t + 0.012);
    g.gain.exponentialRampToValueAtTime(0.0001, t + 0.22);
    o.connect(g).connect(c.destination);
    o.start(t);
    o.stop(t + 0.24);
  }
}

/** The one place a sound reaches the speakers. The dev harness / tests replace `SoundOut.play` and only look at what would be played.
 *  Nothing creates an AudioContext before a sound is really played (no click handlers, no boot-time context): a page that is only
 *  driven by a script never touches the audio output. */
export const SoundOut = {
  /** `test`: a click on a Test button (a user gesture, so a blocked context may be resumed). An event sound plays only once the page
   *  has had a click or key press (`navigator.userActivation`: before that the browser blocks audio and a late sound would be wrong, so it is dropped). */
  async play(name, { test = false } = {}) {
    const plan = soundPlan(name, S.settings.soundVol);
    if (!plan || (!test && !gate.admit(name, Date.now()))) return;
    try {
      if (!test && !(navigator.userActivation && navigator.userActivation.hasBeenActive)) return;
      ensureCtx();
      if (ctx.state !== "running") {
        if (!test) return void ctx.resume().catch(() => {});
        await ctx.resume();
      }
      schedule(plan);
    } catch {
      /* no audio output: nothing to do */
    }
  },
};

/** Play what `sounds` (a `RoomSounds`) has collected. */
export function flushSounds(sounds) {
  for (const n of sounds.drain(Date.now())) SoundOut.play(n);
}
