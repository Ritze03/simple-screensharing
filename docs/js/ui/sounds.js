// Notification sounds (task I9): four short generated sounds, so there are no files to ship. Someone joins / leaves one of MY streams
// (soft sine pairs, up and down), someone else starts / stops a stream (rounder triangle notes, a rising chord and a low fall).
// The definitions are the replica's (`superdoc/ui/replica/index.html` `SOUNDS`) and the native client's (`client/src/sounds.rs`): keep the
// three equal. The edge rules (`RoomSounds`) are a port of the native `sounds::RoomSounds`, same names and same tests.

import { S, dropLog } from "./store.js";

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
 * How long a disappearance waits before it is news (row 217): a viewer who vanishes from the lists, or another person's stream that stops,
 * only sounds leave / stop once this has passed; whoever (or whatever) is back within it made no sound at all. Native twin: `sounds::GRACE`.
 * Why a grace: the relay reports every Unsubscribe and every re-announced stream as a changed list, and several things nobody means as "leaving"
 * do that: a viewer's keyframe request is Unsubscribe + Subscribe (this page handles each list datagram on its own, so it played leave and join
 * for it every time), my own capture restart (the viewers come back to a new stream), another person's share restarting (stop + start), a
 * viewer who reconnects. Why 5 s: it covers a lost refresh datagram (the relay repeats a list every 2 s) and a restart on its first retry
 * (1.4-4.1 s to a picture); a real leave is heard 5 s late, which a notice can afford.
 */
export const GRACE_MS = 5000;

/**
 * Stream identities for `RoomSounds.streams`: the core's `stableKey` (room + sharer name + kind + label), which survives a restart under a new
 * stream id, numbered when several streams of one sharer are identical (`#2`, `#3`) so that one of them stopping still counts.
 */
export function soundStreamKeys(streams) {
  const seen = new Map();
  return streams.filter((s) => !s.self).map((s) => {
    const k = s.stableKey || s.key;
    const n = (seen.get(k) || 0) + 1;
    seen.set(k, n);
    return n > 1 ? `${k}#${n}` : k;
  });
}

/**
 * One room's sound edges. Fed with what the room's events say, then `drain` says what to play. Pure: no clock (every call gets `nowMs`), no
 * AudioContext. Rules: a sound for someone joining or leaving one of MY streams (the relay's viewer lists) and for SOMEONE ELSE's stream
 * starting or stopping; never for my own streams, the join snapshot (`streams(keys, false)` only sets the baseline), a label change (same
 * key), or what a resumed room replays (`quiet`). A leave or a stop waits `GRACE_MS` and is cancelled by the same viewer (the same member
 * name: a reconnect has a new id) or stream (the same stable key) coming back.
 */
export class RoomSounds {
  constructor() {
    this.quietUntil = 0;
    this.keys = new Set(); // other people's stream keys, as of the last call
    this.stopped = new Map(); // key -> when: streams that stopped less than GRACE_MS ago
    this.viewerSet = new Map(); // everyone watching at least one of my streams: id -> name (or null)
    this.gone = []; // { id, name, at, silent }: viewers who vanished less than GRACE_MS ago
    this.mine = new Set(); // my stream ids
    this.pending = []; // [sound, cause]
  }
  /** A room that just resumed: nothing sounds before `until` (ms). */
  quiet(until) {
    this.quietUntil = until;
  }
  /** The streams of other people (`soundStreamKeys`). `joined`: the room is live, not still taking its snapshot. */
  streams(keys, joined, nowMs = Date.now()) {
    const now = new Set(keys);
    if (joined) {
      for (const k of now) {
        if (this.keys.has(k)) continue;
        if (!this.stopped.delete(k)) this.pending.push(["start", `stream started: ${k}`]); // back within the grace: a restart, not news
      }
      for (const k of this.keys) if (!now.has(k)) this.stopped.set(k, nowMs);
    } else this.stopped.clear();
    this.keys = now;
  }
  /**
   * Who watches what: `mine` = my stream ids here, `lists` = Map(stream id -> viewer ids), `nameOf(id)` = the member's name or null. A person
   * counts once however many of my streams they watch. A person who leaves because MY stream stopped is not "leaving", nor is their return to
   * its replacement within the grace a join.
   */
  viewers(mine, lists, nowMs = Date.now(), nameOf = () => null) {
    const seen = new Set();
    for (const [id, v] of lists) if (mine.has(id)) for (const c of v) seen.add(c);
    const ownStop = [...this.mine].some((id) => !mine.has(id));
    const joined = [];
    for (const c of seen) {
      if (this.viewerSet.has(c)) continue;
      const name = nameOf(c) || null;
      const i = this.gone.findIndex((g) => g.id === c || (name !== null && g.name === name));
      if (i >= 0) this.gone.splice(i, 1);
      else joined.push(name || c);
      this.viewerSet.set(c, name);
    }
    if (joined.length) this.pending.push(["join", `started watching my streams: ${joined.sort().join(", ")}`]);
    for (const [c, name] of [...this.viewerSet]) {
      if (seen.has(c)) continue;
      this.viewerSet.delete(c);
      this.gone.push({ id: c, name, at: nowMs, silent: ownStop });
    }
    for (const [c, name] of this.viewerSet) if (name === null) this.viewerSet.set(c, nameOf(c) || null);
    this.mine = new Set(mine);
  }
  /** Whether a leave or a stop is waiting out its grace (the caller must drain again within `GRACE_MS`). */
  gracePending() {
    return this.gone.length > 0 || this.stopped.size > 0;
  }
  /** What to play for this pass with each one's cause: `[[name, cause]]`, each sound once, in the fixed order; none while quiet. */
  drainCauses(nowMs) {
    const due = this.gone.filter((g) => nowMs - g.at >= GRACE_MS);
    this.gone = this.gone.filter((g) => nowMs - g.at < GRACE_MS);
    const left = due.filter((g) => !g.silent).map((g) => g.name || g.id);
    if (left.length) this.pending.push(["leave", `stopped watching my streams, away for ${GRACE_MS / 1000} s: ${left.join(", ")}`]);
    const ended = [...this.stopped].filter(([, at]) => nowMs - at >= GRACE_MS).map(([k]) => k).sort();
    for (const k of ended) this.stopped.delete(k);
    if (ended.length) this.pending.push(["stop", `stream stopped, away for ${GRACE_MS / 1000} s: ${ended.join(", ")}`]);
    const pending = this.pending;
    this.pending = [];
    if (nowMs < this.quietUntil) return [];
    return SOUND_NAMES.filter((n) => pending.some(([p]) => p === n)).map((n) => [n, pending.filter(([p]) => p === n).map(([, c]) => c).join("; ")]);
  }
  /** `drainCauses`, the sound names only. */
  drain(nowMs) {
    return this.drainCauses(nowMs).map(([n]) => n);
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

/** Play what `sounds` (a `RoomSounds`) has collected, log each sound with its cause (Settings > Connection, `dropLog`; `room` is the room's name),
 *  and come back after the grace while a leave or a stop is still waiting it out. */
export function flushSounds(sounds, room = "") {
  clearTimeout(sounds.timer);
  sounds.timer = null;
  for (const [n, cause] of sounds.drainCauses(Date.now())) {
    dropLog("sound", `${room}: ${n} sound: ${cause}`);
    SoundOut.play(n);
  }
  if (sounds.gracePending()) sounds.timer = setTimeout(() => flushSounds(sounds, room), 1000);
}
