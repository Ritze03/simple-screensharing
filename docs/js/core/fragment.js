// Media datagram fragmentation (sender) and reassembly (receiver), spec §6;
// mirrors `client/src/fragment.rs`.

import { MAX_MEDIA_DATAGRAM_LEN, MEDIA_HEADER_LEN, packMediaHeader } from "./protocol.js";

/**
 * Split one already-encrypted frame into wire-ready datagrams (header + slice).
 * Every datagram is at most `min(maxDatagramSize, 1000)` bytes (header included).
 * @returns {Uint8Array[]}
 */
export function fragmentFrame({ connId, streamId, frameId, flags, ciphertext, maxDatagramSize = MAX_MEDIA_DATAGRAM_LEN }) {
  const cap = Math.min(maxDatagramSize, MAX_MEDIA_DATAGRAM_LEN);
  const per = cap - MEDIA_HEADER_LEN;
  if (per <= 0) throw new Error("maxDatagramSize too small to fit even the media header");
  const count = Math.max(1, Math.ceil(ciphertext.length / per));
  if (count > 0xffff) throw new Error(`frame too large: ${count} fragments exceeds u16`);
  const out = new Array(count);
  for (let i = 0; i < count; i++) {
    const slice = ciphertext.subarray(i * per, Math.min(ciphertext.length, (i + 1) * per));
    const d = new Uint8Array(MEDIA_HEADER_LEN + slice.length);
    packMediaHeader(d, { flags, connId, streamId, frameId, fragmentIndex: i, fragmentCount: count });
    d.set(slice, MEDIA_HEADER_LEN);
    out[i] = d;
  }
  return out;
}

const MAX_INFLIGHT_FRAMES_PER_STREAM = 2; // spec §6.5: keep the newest 1-2 in-progress frames (legacy mode only)
const STALE_TIMEOUT_MS = 200; // legacy mode only

// ---- NACK mode (spec §6.6, #111); mirrors the constants of client/src/fragment.rs ----
/** A hole counts as a gap once this many fragment positions later have arrived. */
export const NACK_REORDER_DEPTH = 2;
/** A frame quiet this long has lost its tail (a paced keyframe sends a chunk every 2 ms). */
export const NACK_IDLE_MS = 20;
/** Times one fragment is asked for; the frame is abandoned one retry interval after the last ask. */
export const NACK_MAX_ROUNDS = 3;
/** More suspect fragments than this is congestion, not loss: abandoned without asking. */
export const NACK_MAX_REPAIR_FRAGMENTS = 128;
/** A frame nobody here has seen (row 151) has an unknown size: its Request names this many fragments, the most a relay or sender answers. */
export const NACK_WHOLE_FRAME_FRAGMENTS = 256;
/** Nothing waits longer than this for a frame, whatever else happens. */
export const NACK_HARD_LIMIT_MS = 1000;
/** Memory guard: frames held per stream. */
const NACK_MAX_FRAMES = 256;
/** Frame ids remembered per stream (delivered or given up on) so a late duplicate never opens a ghost frame. */
const NACK_DONE_CAP = 256;

/**
 * Time between two NACK rounds for one frame: long enough for the answer to come back even when it has to go on to the
 * sharer (viewer RTT + sharer RTT + processing). The viewer only knows its own RTT; the 60 ms and the 100 ms floor are the
 * allowance for the sharer's leg.
 */
export function nackRetryInterval(rttMs) {
  return Math.min(300, Math.max(100, rttMs * 2 + 60));
}

/** A bounded set of recent frame ids. */
class DoneIds {
  constructor() {
    this.set = new Set(); // insertion-ordered: the first element is the oldest
  }
  add(id) {
    this.set.add(id);
    if (this.set.size > NACK_DONE_CAP) this.set.delete(this.set.values().next().value);
  }
  has(id) {
    return this.set.has(id);
  }
}

/** Contiguous runs `[first, count]` of ascending indices. */
export function runsOf(indices) {
  const runs = [];
  for (const i of indices) {
    const last = runs[runs.length - 1];
    if (last && last[0] + last[1] === i) last[1]++;
    else runs.push([i, 1]);
  }
  return runs;
}

/**
 * Reassembles fragments for many (connId, streamId) streams. Two modes, like `client/src/fragment.rs`.
 *
 * **Legacy** (default): spec §6.5 drop-and-continue. A frame is delivered the moment its last fragment lands; a frame with any
 * missing fragment is dropped for good and `onDrop` is told, so the decoder can resync at the next keyframe. Each stream keeps
 * a `floor`: fragments of any frame at or below it belong to a frame that already completed or was given up on, and are ignored
 * (frame_id values are strictly increasing per stream: the sender's never-reused nonce counter).
 *
 * **NACK** ({@link Reassembler#setNack}, spec §6.6): an incomplete frame is repaired by {@link Reassembler#poll} asking for its
 * missing fragments, and later complete frames of that stream wait for it so the decoder still sees them in order. Frames can
 * now finish out of order, so the floor is replaced by a memory of the last 256 finished or abandoned frame ids. `onDrop` fires
 * when a frame is given up on (rounds exhausted, Miss, too many holes, 1 s limit), before the frames released behind it are
 * returned, so the caller sees "loss" before the frames that follow it.
 */
export class Reassembler {
  /** @param {{now?:()=>number, onDrop?:(connId:bigint, streamId:number, frameId:number)=>void}} [opts] */
  constructor({ now = () => performance.now(), onDrop = null } = {}) {
    this.now = now;
    this.onDrop = onDrop;
    this.streams = new Map(); // key -> { frames: Map(frameId -> buf), floor: number, done: DoneIds, owner: [connId, streamId] }
    this.droppedFrames = 0;
    this.nack = false;
    this.nackStats = { requests: 0, fragmentsRequested: 0, fragmentsRecovered: 0, framesRecovered: 0, framesAbandoned: 0 };
  }

  /** Switch NACK mode on (the relay acknowledged our Hello) or off. Frames already in flight carry on under the new rules. */
  setNack(on) {
    this.nack = !!on;
  }

  /** True while a NACK-mode frame is incomplete, i.e. {@link Reassembler#poll} has work to do. */
  hasPending() {
    if (!this.nack) return false;
    for (const st of this.streams.values()) for (const b of st.frames.values()) if (b.received < b.parts.length) return true;
    return false;
  }

  #stream(h) {
    const key = `${h.connId}:${h.streamId}`;
    let st = this.streams.get(key);
    if (!st) this.streams.set(key, (st = { frames: new Map(), floor: -1, done: new DoneIds(), owner: [h.connId, h.streamId] }));
    return st;
  }

  /**
   * Feed one parsed datagram. Returns the frames that became deliverable, in order: at most one in legacy mode, possibly
   * several in NACK mode (a repaired frame releases the ones held behind it). Each is
   * `{ connId, streamId, frameId, ciphertext, keyframe, stalled }`.
   * @param {{connId:bigint, streamId:number, frameId:number, fragmentIndex:number, fragmentCount:number, keyframe:boolean, stalled:boolean}} h
   * @param {Uint8Array} payload  this fragment's slice (copied: datagram buffers may be reused)
   */
  ingest(h, payload) {
    if (h.fragmentCount === 0) return [];
    if (h.fragmentIndex >= h.fragmentCount) return [];
    const st = this.#stream(h);
    return this.nack ? this.#ingestNack(h, st, payload) : this.#ingestLegacy(h, st, payload);
  }

  /** The pre-NACK entry point (the legacy-mode tests use it): the one completed frame, or null. */
  insert(h, payload) {
    const out = this.ingest(h, payload);
    return out.length ? out[0] : null;
  }

  #newBuf(h, now) {
    return {
      parts: new Array(h.fragmentCount).fill(null), received: 0, bytes: 0, firstSeen: now, touched: now, keyframe: h.keyframe, stalled: h.stalled,
      maxIndex: 0, attempts: 0, asks: null, askAt: null, giveUp: false,
    };
  }

  #complete(connId, streamId, id, buf) {
    const out = new Uint8Array(buf.bytes);
    let o = 0;
    for (const p of buf.parts) {
      out.set(p, o);
      o += p.length;
    }
    return { connId, streamId, frameId: id, ciphertext: out, keyframe: buf.keyframe, stalled: buf.stalled };
  }

  #ingestLegacy(h, st, payload) {
    const frames = st.frames;
    const now = this.now();

    // Stale-timeout backstop.
    for (const [id, buf] of frames) {
      if (now - buf.touched >= STALE_TIMEOUT_MS) this.#drop(h, st, id);
    }
    if (h.frameId <= st.floor) return []; // a straggler of a frame we already finished or abandoned

    let buf = frames.get(h.frameId);
    if (!buf) {
      buf = this.#newBuf(h, now);
      frames.set(h.frameId, buf);
    }
    if (h.fragmentCount !== buf.parts.length) return []; // inconsistent count for the same frame id
    if (buf.parts[h.fragmentIndex] === null) {
      buf.parts[h.fragmentIndex] = payload.slice();
      buf.received++;
      buf.bytes += payload.length;
    }
    buf.touched = now;
    if (h.keyframe) buf.keyframe = true;

    const complete = buf.received === buf.parts.length;

    // Evict older incomplete frames beyond the in-flight cap (their missing fragment never arrives).
    while (frames.size > MAX_INFLIGHT_FRAMES_PER_STREAM) {
      let oldest = null;
      for (const id of frames.keys()) if (oldest === null || id < oldest) oldest = id;
      if (oldest === h.frameId) break; // never evict the frame we just touched
      this.#drop(h, st, oldest);
    }

    if (!complete) return [];
    frames.delete(h.frameId);
    st.floor = Math.max(st.floor, h.frameId);
    // Anything older than the completed frame can never be decoded in order any more.
    for (const id of [...frames.keys()]) {
      if (id < h.frameId) this.#drop(h, st, id);
    }
    return [this.#complete(h.connId, h.streamId, h.frameId, buf)];
  }

  #ingestNack(h, st, payload) {
    if (st.done.has(h.frameId)) return []; // a retransmit that raced the original, or a straggler of an abandoned frame
    const now = this.now();
    let buf = st.frames.get(h.frameId);
    if (!buf) {
      buf = this.#newBuf(h, now);
      st.frames.set(h.frameId, buf);
    }
    if (h.fragmentCount !== buf.parts.length) return [];
    const i = h.fragmentIndex;
    if (buf.parts[i] === null) {
      buf.parts[i] = payload.slice();
      buf.received++;
      buf.bytes += payload.length;
      if (i > buf.maxIndex) buf.maxIndex = i;
      if (buf.asks && buf.asks[i] > 0) this.nackStats.fragmentsRecovered++;
    }
    buf.touched = now;
    if (h.keyframe) buf.keyframe = true;
    return buf.received === buf.parts.length ? this.#release(st) : [];
  }

  /** NACK mode: every complete frame older than the oldest incomplete one, in order. */
  #release(st) {
    let blocker = Infinity;
    for (const [id, b] of st.frames) if (b.received < b.parts.length && id < blocker) blocker = id;
    const ready = [];
    for (const [id, b] of st.frames) if (b.received === b.parts.length && id < blocker) ready.push(id);
    ready.sort((a, b) => a - b);
    const out = [];
    for (const id of ready) {
      const buf = st.frames.get(id);
      st.frames.delete(id);
      st.done.add(id);
      if (buf.attempts > 0) this.nackStats.framesRecovered++;
      out.push(this.#complete(st.owner[0], st.owner[1], id, buf));
    }
    return out;
  }

  /** NACK mode: the relay cannot supply (part of) the range's frame. It is given up at the next poll. */
  nackMiss({ connId, streamId, frameId, first = 0 }) {
    const buf = this.streams.get(`${connId}:${streamId}`)?.frames.get(frameId);
    if (buf && first < buf.parts.length) buf.giveUp = true; // past the frame's end: the tail of a whole-frame Request, nothing is missing
  }

  /** Missing fragment indices worth asking for now: a hole with >= 2 later fragments behind it, or every hole once the frame is quiet. */
  #suspects(buf, now) {
    const idle = now - buf.touched >= NACK_IDLE_MS;
    const out = [];
    for (let i = 0; i < buf.parts.length; i++) {
      if (buf.parts[i] === null && (idle || i + NACK_REORDER_DEPTH <= buf.maxIndex)) out.push(i);
    }
    return out;
  }

  /**
   * NACK mode, call every few milliseconds while {@link Reassembler#hasPending}: decides which frames need a (further) round of
   * Requests, gives up on the ones that are past saving, and returns the frames that became deliverable by that plus the Requests
   * to send (`{connId, streamId, frameId, first, count}`, one per contiguous run). `rttMs` sets the retry interval.
   * @returns {{frames: object[], requests: object[]}}
   */
  poll(rttMs = 150) {
    const frames = [];
    const requests = [];
    if (!this.nack) return { frames, requests };
    const now = this.now();
    const interval = nackRetryInterval(rttMs);
    for (const st of this.streams.values()) {
      const [connId, streamId] = st.owner;
      const abandon = [];
      for (const [frameId, f] of st.frames) {
        if (f.received === f.parts.length) continue;
        if (f.giveUp || now - f.firstSeen >= NACK_HARD_LIMIT_MS) {
          abandon.push(frameId);
          continue;
        }
        const suspects = this.#suspects(f, now);
        if (!suspects.length) continue;
        const asks = (i) => (f.asks ? f.asks[i] : 0);
        const askAt = (i) => (f.asks ? f.askAt[i] : 0);
        // Too many holes (congestion), or a fragment asked for as often as allowed and still missing a whole interval after the last ask.
        const exhausted = suspects.some((i) => asks(i) >= NACK_MAX_ROUNDS && now >= askAt(i) + interval);
        if (suspects.length > NACK_MAX_REPAIR_FRAGMENTS || exhausted) {
          abandon.push(frameId);
          continue;
        }
        // Ask for the fragments never asked for, and again for those whose last ask is older than the interval (a hole that
        // shows up late in a paced keyframe starts its own rounds).
        const due = suspects.filter((i) => asks(i) < NACK_MAX_ROUNDS && (asks(i) === 0 || now >= askAt(i) + interval));
        if (!due.length) continue;
        if (!f.asks) {
          f.asks = new Uint8Array(f.parts.length);
          f.askAt = new Float64Array(f.parts.length);
        }
        for (const i of due) {
          f.asks[i]++;
          f.askAt[i] = now;
        }
        f.attempts++;
        for (const [first, count] of runsOf(due)) {
          requests.push({ connId, streamId, frameId, first, count });
          this.nackStats.requests++;
          this.nackStats.fragmentsRequested += count;
        }
      }
      for (const id of abandon) this.#abandon(st, id);
      // Memory guard: a stream that never settles must not grow without bound.
      while (st.frames.size > NACK_MAX_FRAMES) {
        this.#abandon(st, st.frames.keys().next().value);
        abandon.push(0);
      }
      if (abandon.length) frames.push(...this.#release(st)); // the frames held behind an abandoned one
    }
    return { frames, requests };
  }

  #abandon(st, id) {
    const buf = st.frames.get(id);
    if (!buf) return;
    st.frames.delete(id);
    st.done.add(id);
    if (buf.attempts > 0) this.nackStats.framesAbandoned++;
    this.droppedFrames++;
    if (this.onDrop) this.onDrop(st.owner[0], st.owner[1], id);
  }

  /** Forget everything for a stream (it stopped, or we unsubscribed). */
  clearStream(connId, streamId) {
    this.streams.delete(`${connId}:${streamId}`);
  }

  clear() {
    this.streams.clear();
  }

  #drop(h, st, frameId) {
    st.frames.delete(frameId);
    st.floor = Math.max(st.floor, frameId);
    this.droppedFrames++;
    if (this.onDrop) this.onDrop(h.connId, h.streamId, frameId);
  }
}

// ---- send pacing: mirrors `pace_plan` in client/src/capture.rs (#102) ----
const PACE_MIN_DATAGRAMS = 24; // below ~24 kB a frame goes out at once
const PACE_MAX_WINDOW_MS = 60; // a keyframe is spread over at least min(60 ms, 4 frame periods)
const PACE_MAX_RATE_WINDOW_MS = 600; // no frame is held longer than this
const PACE_STEP_MS = 2;

/**
 * How to spread a frame of `count` datagrams / `bytes`: `{ perChunk, gapMs }`, or null to send them back to back.
 * A keyframe takes at least max(min(60 ms, 4 frame periods), bytes / (2.5 x configured bitrate)), capped at 600 ms; a delta
 * frame is paced only when that rate time exceeds 2 frame periods. A frame is lost if any fragment is, so a 1 MB IDR sent
 * as one burst into a shallow queue (or at 0.1 % random loss) rarely arrives whole.
 */
export function pacePlan(count, bytes, keyframe, fps, bitrateKbps) {
  if (count < PACE_MIN_DATAGRAMS) return null;
  const period = 1000 / Math.min(280, Math.max(1, fps));
  const byRate = (bytes * 3.2) / Math.max(1, bitrateKbps); // bytes * 8 bit / (2.5 * kbps) in ms
  let window;
  if (keyframe) window = Math.max(Math.min(PACE_MAX_WINDOW_MS, period * 4), byRate);
  else if (byRate > period * 2) window = byRate;
  else return null;
  const steps = Math.max(1, Math.floor(Math.min(window, PACE_MAX_RATE_WINDOW_MS) / PACE_STEP_MS));
  return steps < 2 ? null : { perChunk: Math.ceil(count / steps), gapMs: PACE_STEP_MS };
}
