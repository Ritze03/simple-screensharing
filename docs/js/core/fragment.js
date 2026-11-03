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

const MAX_INFLIGHT_FRAMES_PER_STREAM = 2; // spec §6.5: keep the newest 1-2 in-progress frames
const STALE_TIMEOUT_MS = 200;

/**
 * Reassembles fragments for many (connId, streamId) streams. A frame with any missing fragment is
 * dropped for good (no retransmission); `onDrop` is told about each such frame so the decoder can
 * resync at the next keyframe.
 *
 * frame_id values are strictly increasing per (connId, streamId) (they are the sender's never-reused
 * nonce counter), so each stream keeps a `floor`: fragments of any frame at or below it belong to a
 * frame that already completed or was already given up on, and are ignored.
 */
export class Reassembler {
  /** @param {{now?:()=>number, onDrop?:(connId:bigint, streamId:number, frameId:number)=>void}} [opts] */
  constructor({ now = () => performance.now(), onDrop = null } = {}) {
    this.now = now;
    this.onDrop = onDrop;
    this.streams = new Map(); // key -> { frames: Map(frameId -> buf), floor: number }
    this.droppedFrames = 0;
  }

  /**
   * Feed one parsed datagram. Returns `{ ciphertext, keyframe, stalled }` when a frame completed, else null.
   * @param {{connId:bigint, streamId:number, frameId:number, fragmentIndex:number, fragmentCount:number, keyframe:boolean, stalled:boolean}} h
   * @param {Uint8Array} payload  this fragment's slice (copied: datagram buffers may be reused)
   */
  insert(h, payload) {
    if (h.fragmentCount === 0) return null;
    if (h.fragmentIndex >= h.fragmentCount) return null;
    const key = `${h.connId}:${h.streamId}`;
    let st = this.streams.get(key);
    if (!st) this.streams.set(key, (st = { frames: new Map(), floor: -1 }));
    const frames = st.frames;
    const now = this.now();

    // Stale-timeout backstop.
    for (const [id, buf] of frames) {
      if (now - buf.touched >= STALE_TIMEOUT_MS) this.#drop(h, st, id);
    }
    if (h.frameId <= st.floor) return null; // a straggler of a frame we already finished or abandoned

    let buf = frames.get(h.frameId);
    if (!buf) {
      buf = { parts: new Array(h.fragmentCount).fill(null), received: 0, bytes: 0, touched: now, keyframe: h.keyframe, stalled: h.stalled };
      frames.set(h.frameId, buf);
    }
    if (h.fragmentCount !== buf.parts.length) return null; // inconsistent count for the same frame id
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

    if (!complete) return null;
    frames.delete(h.frameId);
    st.floor = Math.max(st.floor, h.frameId);
    // Anything older than the completed frame can never be decoded in order any more.
    for (const id of [...frames.keys()]) {
      if (id < h.frameId) this.#drop(h, st, id);
    }
    const out = new Uint8Array(buf.bytes);
    let o = 0;
    for (const p of buf.parts) {
      out.set(p, o);
      o += p.length;
    }
    return { ciphertext: out, keyframe: buf.keyframe, stalled: buf.stalled };
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
