// Adaptive jitter buffer for still-encoded video chunks, adapted from the former Electron frontend's
// `tile-video.js` (removed in the egui rework; same maths, tuned against this wire format).
//
// Chunks are held (cheap: KBs each, never decoded VideoFrames) and released for decode at
//   decodeAt = anchorArrival + (pts - anchorPts) + appliedBuffer
// where `appliedBuffer` eases toward `jitterEwma * 4 + 40 ms` (clamped to [0, maxJitterBufferMs]):
//   - growing is capped at MAX_BUFFER_EASE_STEP_MS per chunk (a jitter spike is an invisible drift);
//   - shrinking is capped at `catchUpSpeedPercent`% of wall-clock time, which is mathematically the
//     same as capping catch-up playback speed at 1 + percent/100 (derivation in tile-video.js).
// A decode-queue backstop drops a run of queued delta frames up to the next keyframe when the decoder
// itself falls behind (dropping a lone delta frame would corrupt every later reference).
//
// Differences from tile-video.js: one timer drains ALL due chunks (a per-chunk setTimeout is clamped
// to ~4 ms when nested, which is the whole frame budget at 240 fps), and the clock is injectable.

export const DEFAULT_MAX_JITTER_BUFFER_MS = 2000; // client/src/rooms.rs default, clamp 0..=5000
export const DEFAULT_CATCH_UP_SPEED_PERCENT = 5; //  client/src/rooms.rs default, clamp 5..=10
export const JITTER_BUFFER_FLOOR_MS = 40;
const JITTER_EWMA_ALPHA = 1 / 16; // RFC 3550 6.4.1
const MAX_BUFFER_EASE_STEP_MS = 20;
export const DECODE_QUEUE_CATCHUP_THRESHOLD = 3;
// Never hand the decoder more than this many pending decode() requests: a decoder slower than the stream (weak laptop,
// CPU contention) would otherwise accumulate an unbounded private backlog that the backstop above can never touch, and
// latency would grow without limit. Held chunks stay in the jitter buffer, where the backstop can skip to a keyframe.
export const MAX_DECODE_QUEUE = 8;

const clamp = (v, lo, hi) => Math.max(lo, Math.min(hi, v));

export function clampMaxJitterBufferMs(ms) {
  const n = Number(ms);
  return clamp(Number.isFinite(n) ? n : DEFAULT_MAX_JITTER_BUFFER_MS, 0, 5000);
}
export function clampCatchUpSpeedPercent(p) {
  const n = Number(p);
  return clamp(Number.isFinite(n) ? n : DEFAULT_CATCH_UP_SPEED_PERCENT, 5, 10);
}

export class JitterBuffer {
  /**
   * @param {{maxJitterBufferMs?:number, catchUpSpeedPercent?:number}} [opts]
   */
  constructor({ maxJitterBufferMs = DEFAULT_MAX_JITTER_BUFFER_MS, catchUpSpeedPercent = DEFAULT_CATCH_UP_SPEED_PERCENT } = {}) {
    this.maxJitterBufferMs = clampMaxJitterBufferMs(maxJitterBufferMs);
    this.catchUpSpeedFraction = clampCatchUpSpeedPercent(catchUpSpeedPercent) / 100;
    this.pending = []; // { decodeAtMs, keyframe, item }
    this.jitterEwmaMs = 0;
    this.appliedBufferMs = 0;
    this.lastBufferUpdateMs = null;
    this.anchorArrivalMs = null;
    this.anchorPtsUs = null;
    this.lastArrivalMs = null;
    this.lastPtsUs = null;
    this.catchUpDropped = 0;
  }

  setMaxJitterBufferMs(ms) {
    this.maxJitterBufferMs = clampMaxJitterBufferMs(ms);
  }
  setCatchUpSpeedPercent(p) {
    this.catchUpSpeedFraction = clampCatchUpSpeedPercent(p) / 100;
  }

  /** Drop everything queued and restart pacing (codec reconfigure, stream resync). */
  reset() {
    this.pending.length = 0;
    this.anchorArrivalMs = null;
    this.anchorPtsUs = null;
    this.lastBufferUpdateMs = null;
    this.lastArrivalMs = null;
    this.lastPtsUs = null;
  }

  #updateJitter(arrivalMs, ptsUs) {
    if (this.lastArrivalMs != null) {
      const deviation = arrivalMs - this.lastArrivalMs - (ptsUs - this.lastPtsUs) / 1000;
      this.jitterEwmaMs += (Math.abs(deviation) - this.jitterEwmaMs) * JITTER_EWMA_ALPHA;
    }
    this.lastArrivalMs = arrivalMs;
    this.lastPtsUs = ptsUs;
  }

  #updateApplied(nowMs) {
    const target = clamp(this.jitterEwmaMs * 4 + JITTER_BUFFER_FLOOR_MS, 0, this.maxJitterBufferMs);
    const diff = target - this.appliedBufferMs;
    if (diff >= 0) {
      this.appliedBufferMs += Math.min(diff, MAX_BUFFER_EASE_STEP_MS);
    } else {
      const elapsed = this.lastBufferUpdateMs == null ? 0 : Math.max(0, nowMs - this.lastBufferUpdateMs);
      this.appliedBufferMs += Math.max(diff, -(elapsed * this.catchUpSpeedFraction));
    }
    this.appliedBufferMs = clamp(this.appliedBufferMs, 0, this.maxJitterBufferMs);
    this.lastBufferUpdateMs = nowMs;
  }

  /** Queue one chunk. `ptsUs` is the sender's wire PTS, `nowMs` the arrival time (performance.now()). */
  push(item, { ptsUs, keyframe }, nowMs) {
    this.#updateJitter(nowMs, ptsUs);
    this.#updateApplied(nowMs);
    if (this.anchorArrivalMs == null) {
      this.anchorArrivalMs = nowMs;
      this.anchorPtsUs = ptsUs;
    }
    const decodeAtMs = this.anchorArrivalMs + (ptsUs - this.anchorPtsUs) / 1000 + this.appliedBufferMs;
    this.pending.push({ decodeAtMs, keyframe, item });
  }

  /** ms until the head chunk is due (0 if overdue), or null when empty. */
  nextDelay(nowMs) {
    return this.pending.length ? Math.max(0, this.pending[0].decodeAtMs - nowMs) : null;
  }

  /**
   * Pop every chunk due at `nowMs`, but at most `MAX_DECODE_QUEUE - decodeQueueSize` of them. `decodeQueueSize` is the
   * decoder's own backlog; when it is above the threshold and the head is a delta frame, the run of queued deltas up
   * to the next queued keyframe is discarded instead (always safe: a keyframe references nothing).
   * @returns {{items:any[], dropped:number}}
   */
  drain(nowMs, decodeQueueSize = 0) {
    let dropped = 0;
    if (decodeQueueSize > DECODE_QUEUE_CATCHUP_THRESHOLD && this.pending.length && this.pending[0].decodeAtMs <= nowMs && !this.pending[0].keyframe) {
      let nextKey = -1;
      for (let i = 1; i < this.pending.length; i++) {
        if (this.pending[i].keyframe) {
          nextKey = i;
          break;
        }
      }
      if (nextKey > 0) {
        this.pending.splice(0, nextKey);
        dropped = nextKey;
        this.catchUpDropped += nextKey;
      }
    }
    const items = [];
    const budget = Math.max(0, MAX_DECODE_QUEUE - decodeQueueSize);
    while (this.pending.length && this.pending[0].decodeAtMs <= nowMs && items.length < budget) items.push(this.pending.shift().item);
    return { items, dropped };
  }

  get size() {
    return this.pending.length;
  }

  /** How long the oldest held chunk has been overdue, ms (0 when nothing is held back), and whether a keyframe is queued. */
  overdue(nowMs) {
    if (!this.pending.length) return { ms: 0, hasKeyframe: false };
    return { ms: Math.max(0, nowMs - this.pending[0].decodeAtMs), hasKeyframe: this.pending.some((p) => p.keyframe) };
  }
}
