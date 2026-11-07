// Receive-side video pipeline for ONE watched stream:
//   decrypted access units -> (loss gate) -> jitter buffer -> WebCodecs VideoDecoder -> VideoFrame callback
//
// The codec (H.264 / H.265 / AV1) is sniffed from the first keyframe's bitstream; the WebCodecs codec
// string is derived from its SPS / sequence header and the decoder is re-configured whenever a later
// keyframe changes it (the sender may switch codec or encoder profile mid-stream).

import { RefChain, deriveCodecString } from "./codec.js";
import { nackRetryInterval } from "./fragment.js";
import { JitterBuffer } from "./jitter.js";

const FPS_WINDOW_MS = 1500;
const MAX_PADDING = 64; // same constant as client/src/decode/frame.rs
// Gecko (Firefox, Floorp ...) only: see VideoReceiver#run.
const IS_GECKO = typeof navigator !== "undefined" && /\bFirefox\/\d/.test(navigator.userAgent);
// This many decoder failures within the window: the hardware decoder gets replaced by a software one, where the browser has one.
const SOFTWARE_AFTER_FAILURES = 3;
const SOFTWARE_WINDOW_MS = 30_000;
// A frame lost whole (row 151): the frame ids skipped in front of the frame that showed the jump are asked for, at most this many, this many times.
const REPAIR_MAX_IDS = 32;
const REPAIR_ROUNDS = 2;
const REPAIR_MAX_RETRY_MS = 150; // a failed repair delays the keyframe wait, so less than a fragment repair gets (where the browser reports no RTT that is 300 ms)
const REPAIR_HARD_LIMIT_MS = 1000; // several repairs in a row: nothing is held longer than this

/**
 * The visible rectangle of a decoded frame, or null when the whole frame is visible. Mirrors the native viewer's
 * `visible_rect` (client/src/decode/frame.rs):
 * 1. a crop the decoder reports itself (`visibleRect` smaller than the coded size) wins;
 * 2. else the size the sender announced (`hint` = {width,height}), when the frame is larger than it by less
 *    than MAX_PADDING in each direction (and not smaller): the padding is at the bottom/right.
 * Why 2: the AMD VCN4 AV1 encoder codes 1920x1080 as 1920x1082 and signals no crop anywhere in the bitstream
 * (render_and_frame_size_different = 0, Mesa issue 9185); only the announced StreamInfo size says 1080.
 * @param {{codedWidth:number,codedHeight:number,visibleRect?:{x:number,y:number,width:number,height:number}|null}} f
 * @param {{width:number,height:number}|null} [hint]
 * @returns {{x:number,y:number,width:number,height:number}|null}
 */
export function visibleRectFor(f, hint) {
  const cw = f.codedWidth, ch = f.codedHeight;
  const r = f.visibleRect;
  if (r && r.width > 0 && r.height > 0 && (r.width < cw || r.height < ch)) return { x: r.x, y: r.y, width: r.width, height: r.height };
  const w = hint && hint.width, h = hint && hint.height;
  if (w > 0 && h > 0 && w <= cw && h <= ch && cw - w < MAX_PADDING && ch - h < MAX_PADDING && (w !== cw || h !== ch)) {
    return { x: 0, y: 0, width: w, height: h };
  }
  return null;
}

export class VideoReceiver {
  /**
   * @param {object} o
   * @param {(frame:VideoFrame)=>void} o.onFrame   takes OWNERSHIP of the frame: call frame.close() promptly
   * @param {(type:string, detail?:any)=>void} [o.onEvent]  "codec" | "unsupported" | "decoder-error" | "need-keyframe" | "decoder-mode"
   * @param {number} [o.maxJitterBufferMs]
   * @param {number} [o.catchUpSpeedPercent]
   * @param {"no-preference"|"prefer-hardware"|"prefer-software"} [o.hardwareAcceleration]
   * @param {import("./avsync.js").AvSync} [o.avSync]  this sender's A/V sync state (audio follows this video)
   * @param {(frameIds:number[])=>boolean} [o.requestFrames]  ask for frames lost whole; false = cannot (no NACK), the keyframe wait starts at once
   * @param {()=>number} [o.rttMs]  the connection's RTT, sets the time between two rounds of asking
   */
  constructor({ onFrame, announcedSize = () => null, onEvent = () => {}, maxJitterBufferMs, catchUpSpeedPercent, hardwareAcceleration = "no-preference", lowLatency = true, adaptiveLatency = IS_GECKO, now = () => performance.now(), DecoderClass = null, avSync = null, requestFrames = () => false, rttMs = () => 150 }) {
    this.avSync = avSync; // shared with this sender's audio receiver (avsync.js); null = no A/V sync
    if (avSync) avSync.videoStarted();
    this.DecoderClass = DecoderClass; // tests inject a fake decoder; default is the global VideoDecoder
    this.onFrame = onFrame;
    this.announcedSize = announcedSize; // () => {width,height}|null: the size the sender announced for this stream
    this.onEvent = onEvent;
    this.requestFrames = requestFrames;
    this.rttMs = rttMs;
    this.hardwareAcceleration = hardwareAcceleration;
    this.lowLatency = lowLatency; // VideoDecoder `optimizeForLatency`; see #run for why it can be switched off once
    this.adaptiveLatency = adaptiveLatency;
    this.now = now;
    this.jb = new JitterBuffer({ maxJitterBufferMs, catchUpSpeedPercent });
    this.decoder = null;
    this.config = null; // { codec: "h264"|..., string }
    this.needKeyframe = true; // true until a keyframe has been queued after start / loss / error
    this.waitingSince = now(); // when we started waiting for a keyframe
    this.lastWaitEvent = -Infinity;
    this.unsupported = null; // codec string we know we cannot decode
    this.timer = null;
    this.closed = false;
    this.outTimes = [];
    this.inTimes = [];
    this.stats = {
      rxFrames: 0, rxBytes: 0, decoded: 0, skippedDeltas: 0, lossEvents: 0, decoderErrors: 0, throughputMode: false,
      width: 0, height: 0, codec: null, codecString: null, lastPtsUs: 0, gaps: 0, ptsBack: 0, repaired: 0, stale: 0,
    };
    this.rxBytesWindow = [];
    this.chain = new RefChain(); // sees a frame that was lost whole (see pushFrame)
    this.lastFrameId = null; // the last frame that went on towards the decoder
    this.lastSubmittedPts = -Infinity;
    this.held = []; // frames waiting for a repair, oldest id first
    this.repair = null; // { gap, idStep, ids: the ids still unanswered, to, rounds, since, bytes, timer }
    this.keyTimes = []; // the last four intervals between keyframes, ms (#gopMs)
    this.lastKeyAt = null;
    this.sinceKey = 0; // frames submitted since the last keyframe
    this.sinceBreak = null; // frames submitted since the last loss, gap or decoder failure; null = none yet
    this.outputs = 0; // pictures this decoder has produced; 0 = it failed straight after configure
    this.lastQueue = 0; // decodeQueueSize at the last submit: an errored decoder is closed and reports 0
    this.failTimes = [];
  }

  setMaxJitterBufferMs(ms) {
    this.jb.setMaxJitterBufferMs(ms);
  }
  setCatchUpSpeedPercent(p) {
    this.jb.setCatchUpSpeedPercent(p);
  }

  /** A frame was lost on the wire: references are broken, so wait for the next keyframe. */
  markLoss() {
    if (this.closed) return;
    this.stats.lossEvents++;
    if (!this.needKeyframe) this.#awaitKeyframe({ reason: "loss" });
  }

  #awaitKeyframe(detail) {
    this.#endRepair();
    this.needKeyframe = true;
    this.waitingSince = this.now();
    this.sinceBreak = 0;
    this.jb.reset(); // queued deltas would decode against a hole
    this.onEvent("need-keyframe", { ...detail, gopMs: this.#gopMs() });
  }

  /** The keyframe interval as seen here, null until it is known: the longest of the last four, so a requested keyframe does not make it look short. */
  #gopMs() {
    return this.keyTimes.length === 4 ? Math.max(...this.keyTimes) : null;
  }

  /** Feed one decrypted access unit, in wire order (a repaired frame comes late and goes back to its place by `frameId`). */
  pushFrame({ ptsUs, keyframe, payload, stalled = false, frameId = null }) {
    if (this.closed) return;
    const t = this.now();
    this.stats.rxFrames++;
    this.stats.rxBytes += payload.length;
    this.stats.lastPtsUs = ptsUs;
    this.#window(this.inTimes, t);
    this.rxBytesWindow.push([t, payload.length]);
    while (this.rxBytesWindow.length && this.rxBytesWindow[0][0] < t - FPS_WINDOW_MS) this.rxBytesWindow.shift();

    const f = { ptsUs, keyframe, payload, stalled, frameId };
    if (!this.repair) {
      this.#take(f, t);
      return;
    }
    if (this.repair.ids.delete(frameId)) this.repair.bytes += payload.length;
    if (keyframe && frameId > this.lastFrameId) {
      this.held = this.held.filter((h) => h.frameId > frameId); // what was held in front of it is not needed any more
      this.#endRepair({ keep: true });
      this.#take(f, t);
      this.#releaseHeld(t);
      return;
    }
    let i = this.held.length;
    while (i > 0 && this.held[i - 1].frameId > frameId) i--;
    this.held.splice(i, 0, f);
    this.#releaseHeld(t);
  }

  /** Frames held for a repair, in order, as far as nothing is missing in front of them any more. */
  #releaseHeld(t) {
    while (this.held.length) {
      if (!this.#take(this.held.shift(), t)) return; // back in `held`: still a hole in front of it
    }
    const r = this.repair;
    if (!r) return;
    this.stats.repaired++;
    this.#endRepair();
    this.onEvent("repaired", { ...r.gap, idStep: r.idStep, ms: Math.round(t - r.since), rounds: r.rounds, bytes: r.bytes, repaired: this.stats.repaired, gaps: this.stats.gaps });
  }

  #endRepair({ keep = false } = {}) {
    if (this.repair) clearTimeout(this.repair.timer);
    this.repair = null;
    if (!keep) {
      this.stats.skippedDeltas += this.held.length;
      this.held = [];
    }
  }

  /** The frames `ids` went missing in front of `f`: hold it and ask. False when they cannot be asked for. */
  #startRepair(f, gap, idStep, ids, t) {
    if (!ids.length || !this.requestFrames(ids)) return false;
    const since = this.repair ? this.repair.since : t;
    if (this.repair) clearTimeout(this.repair.timer);
    this.repair = { gap, idStep, ids: new Set(ids), to: f.frameId, rounds: 1, since, bytes: 0, timer: null };
    this.held.unshift(f);
    this.#armRepair();
    return true;
  }

  #armRepair() {
    const r = this.repair;
    r.timer = setTimeout(() => this.#repairTick(r), Math.min(REPAIR_MAX_RETRY_MS, nackRetryInterval(this.rttMs())));
  }

  /** A round of asking went unanswered: ask again, or stop and wait for a keyframe as for any other loss. */
  #repairTick(r) {
    if (r !== this.repair || this.closed) return;
    if (r.rounds < REPAIR_ROUNDS && r.ids.size && this.now() - r.since < REPAIR_HARD_LIMIT_MS && this.requestFrames([...r.ids])) {
      r.rounds++;
      this.#armRepair();
      return;
    }
    this.#repairFailed(r);
  }

  #repairFailed(r) {
    this.#awaitKeyframe({ reason: "gap", ...r.gap, idStep: r.idStep, ms: Math.round(this.now() - r.since), rounds: r.rounds, repaired: this.stats.repaired, gaps: this.stats.gaps });
  }

  /** The relay has nothing for this frame id and no sender to ask: it belonged to another stream, or it is gone. */
  frameMissing(frameId) {
    const r = this.repair;
    if (r && r.ids.delete(frameId) && !r.ids.size) this.#repairFailed(r);
  }

  /**
   * One frame on its way to the decoder. False when it was held back for a repair (it is then in `held`), true when it is dealt with.
   * A frame older than the last one taken never goes on: the decoder gets its frames in order or not at all, whatever the network did.
   */
  #take(f, t) {
    const { ptsUs, keyframe, payload, stalled, frameId } = f;
    if (frameId !== null && this.lastFrameId !== null && frameId <= this.lastFrameId) {
      this.stats.stale++;
      return true;
    }
    if (keyframe) {
      const info = deriveCodecString(payload);
      if (info && (!this.config || info.string !== this.config.string)) {
        this.#configure(info);
      }
      if (!this.config || this.unsupported === this.config.string) return true;
      if (!this.decoder || this.decoder.state === "closed") this.#createDecoder();
      this.needKeyframe = false;
      this.chain.step(this.config.codec, payload, true);
      if (this.lastKeyAt !== null) this.keyTimes = [...this.keyTimes.slice(-3), t - this.lastKeyAt];
      this.lastKeyAt = t;
      this.sinceKey = 0;
    } else if (this.needKeyframe || !this.config || this.unsupported === this.config.string) {
      this.stats.skippedDeltas++;
      // Still no keyframe a second after we started waiting: the one we asked for may itself have been lost, so say so
      // again (the room client re-requests, rate limited). Without this a lost recovery keyframe stalls until the next GOP.
      if (this.needKeyframe && t - this.waitingSince > 1000 && t - this.lastWaitEvent > 1000) {
        this.lastWaitEvent = t;
        this.onEvent("need-keyframe", { reason: "waiting" });
      }
      return true;
    } else {
      // A whole frame went missing in front of this one. A delta on a broken reference can be fatal: measured with Chromium's own AV1
      // decoder ("Decoding error."), and WebKit closes the decoder on one bad frame ("Decoder failure"). So this frame does not go on:
      // the missing one is asked for by its id (row 151), and when that fails decoding stops as for any other loss.
      const idStep = frameId !== null && this.lastFrameId !== null ? frameId - this.lastFrameId : null;
      const gap = this.chain.step(this.config.codec, payload, false, idStep);
      if (gap) {
        if (this.repair && frameId <= this.repair.to) {
          this.held.unshift(f);
          return false;
        }
        this.stats.gaps++;
        const ids = idStep - 1 <= REPAIR_MAX_IDS ? Array.from({ length: idStep - 1 }, (_, i) => this.lastFrameId + 1 + i) : [];
        if (this.#startRepair(f, gap, idStep, ids, t)) return false;
        this.stats.skippedDeltas++;
        this.#awaitKeyframe({ reason: "gap", ...gap, idStep, repaired: this.stats.repaired, gaps: this.stats.gaps });
        return true;
      }
    }
    if (frameId !== null) this.lastFrameId = frameId;
    if (!keyframe && ptsUs <= this.lastSubmittedPts) this.stats.ptsBack++;
    this.lastSubmittedPts = ptsUs;
    this.sinceKey++;
    if (this.sinceBreak !== null) this.sinceBreak++;
    const chunk = new EncodedVideoChunk({ type: keyframe ? "key" : "delta", timestamp: ptsUs, data: payload });
    this.jb.push(chunk, { ptsUs, keyframe }, t);
    if (this.avSync) this.avSync.noteVideo(this.jb.lastDecodeAtMs, ptsUs, stalled);
    this.#run();
    this.#schedule();
    return true;
  }

  #window(arr, t) {
    arr.push(t);
    while (arr.length && arr[0] < t - FPS_WINDOW_MS) arr.shift();
  }

  #configure(info) {
    this.jb.reset();
    this.config = info;
    this.stats.codec = info.codec;
    this.stats.codecString = info.string;
    this.unsupported = null;
    this.onEvent("codec", { codec: info.codec, codecString: info.string });
    const cfg = this.#decoderConfig();
    (this.DecoderClass || VideoDecoder).isConfigSupported(cfg).then(
      (r) => {
        if (!r.supported && this.config && this.config.string === info.string) {
          this.unsupported = info.string;
          this.jb.reset();
          this.onEvent("unsupported", { codec: info.codec, codecString: info.string });
        }
      },
      () => {},
    );
    if (this.decoder && this.decoder.state === "configured") {
      try {
        this.decoder.configure(cfg);
      } catch (e) {
        this.#decoderFailed(e);
      }
    }
  }

  #decoderConfig() {
    return {
      codec: this.config.string,
      optimizeForLatency: this.lowLatency,
      hardwareAcceleration: this.hardwareAcceleration,
    };
  }

  /** Re-wrap a frame that has padding (see visibleRectFor) as a zero-copy VideoFrame of just the visible rect. */
  #cropped(frame) {
    let rect = null;
    try {
      rect = visibleRectFor(frame, this.announcedSize());
    } catch {}
    if (!rect) return frame;
    try {
      const out = new VideoFrame(frame, { visibleRect: rect });
      frame.close();
      return out;
    } catch {
      return frame; // showing the padding beats losing the frame
    }
  }

  #createDecoder() {
    const Decoder = this.DecoderClass || VideoDecoder;
    const decoder = new Decoder({
      output: (decoded) => {
        if (this.closed || decoder !== this.decoder) {
          decoded.close();
          return;
        }
        const frame = this.#cropped(decoded);
        this.outputs++;
        this.stats.decoded++;
        this.stats.width = frame.displayWidth;
        this.stats.height = frame.displayHeight;
        this.#window(this.outTimes, this.now());
        this.onFrame(frame);
      },
      error: (e) => {
        if (decoder === this.decoder) this.#decoderFailed(e);
      },
    });
    this.decoder = decoder;
    this.outputs = 0;
    // The decoder took a request off its queue: submit more of what the jitter buffer is holding back.
    decoder.ondequeue = () => {
      if (decoder === this.decoder && this.jb.size) {
        this.#run();
        this.#schedule();
      }
    };
    try {
      decoder.configure(this.#decoderConfig());
    } catch (e) {
      this.#decoderFailed(e);
    }
  }

  #decoderFailed(e) {
    const t = this.now();
    this.stats.decoderErrors++;
    const d = this.decoder;
    this.decoder = null;
    try {
      if (d && d.state !== "closed") d.close();
    } catch {}
    this.onEvent("decoder-error", {
      name: e && e.name, message: String(e && e.message ? e.message : e), count: this.stats.decoderErrors, codecString: this.stats.codecString,
      width: this.stats.width, height: this.stats.height, queue: this.lastQueue, keyAgoMs: this.lastKeyAt === null ? null : Math.round(t - this.lastKeyAt),
      sinceKey: this.sinceKey, sinceBreak: this.sinceBreak, outputs: this.outputs, hardwareAcceleration: this.hardwareAcceleration,
      gaps: this.stats.gaps, chain: this.chain.off ? "off" : this.chain.prev === null ? "none" : "on", ptsBack: this.stats.ptsBack, stale: this.stats.stale, repaired: this.stats.repaired,
    });
    this.#awaitKeyframe({ reason: "decoder-error" });
    this.#maybeSoftware(t);
  }

  /** Last resort for a hardware decoder that keeps failing. WebKit ignores the hint for H.264 and AV1; Android Chrome honours it where it has a software decoder for the codec. */
  #maybeSoftware(t) {
    this.failTimes = [...this.failTimes.filter((x) => x > t - SOFTWARE_WINDOW_MS), t];
    if (this.failTimes.length < SOFTWARE_AFTER_FAILURES || this.hardwareAcceleration === "prefer-software" || !this.config) return;
    const cfg = { ...this.#decoderConfig(), hardwareAcceleration: "prefer-software" };
    (this.DecoderClass || VideoDecoder).isConfigSupported(cfg).then(
      (r) => {
        if (!r.supported || this.closed) return;
        this.hardwareAcceleration = "prefer-software"; // the decoder made at the next keyframe
        this.onEvent("decoder-mode", { hardwareAcceleration: "prefer-software" });
      },
      () => {},
    );
  }

  #run() {
    const d = this.decoder;
    if (!d || d.state !== "configured") return;
    const t = this.now();
    const { items, dropped } = this.jb.drain(t, d.decodeQueueSize);
    // The decode-queue backstop just had to skip frames: this decoder is slower than the stream. In Gecko `optimizeForLatency`
    // makes the software H.264/AV1 decoder (ffmpeg / dav1d) single-threaded: measured 2026-10-15, Firefox 157, 1080p
    // 336 vs 1188 fps, 1440p 206 vs 957, 4K 95 vs 440 (Chromium: no difference). So after the first overload switch it off,
    // trading the few frames of reorder/thread latency for 3-5x the decode rate. The backstop only ever drops up to a keyframe,
    // so the chunks about to be submitted start with one, which is what a re-`configure` needs. One-way: a stream that
    // overloaded once will do so again, and the receiver lives only as long as the watch.
    if (dropped > 0 && this.lowLatency && this.adaptiveLatency) {
      this.lowLatency = false;
      this.stats.throughputMode = true;
      try {
        d.configure(this.#decoderConfig());
      } catch (e) {
        this.#decoderFailed(e);
        return;
      }
      this.onEvent("decoder-mode", { lowLatency: false });
    }
    // The decoder cannot keep up and the backlog has no keyframe to skip to: ask for one (the room client re-requests,
    // rate limited), otherwise latency would climb to a whole GOP before the backstop can drop anything.
    if (this.jb.size) {
      const o = this.jb.overdue(t);
      if (o.ms > 750 && !o.hasKeyframe && t - this.lastWaitEvent > 1000) {
        this.lastWaitEvent = t;
        this.onEvent("need-keyframe", { reason: "overload" });
      }
    }
    for (const chunk of items) {
      try {
        d.decode(chunk);
      } catch (e) {
        this.#decoderFailed(e);
        return;
      }
    }
    this.lastQueue = d.decodeQueueSize;
  }

  #schedule() {
    if (this.timer != null || this.closed) return;
    const delay = this.jb.nextDelay(this.now());
    if (delay == null) return;
    this.timer = setTimeout(() => {
      this.timer = null;
      this.#run();
      this.#schedule();
    }, delay);
  }

  /** Snapshot for the stats event. */
  getStats() {
    const t = this.now();
    const fps = (arr) => {
      const a = arr.filter((x) => x >= t - FPS_WINDOW_MS);
      if (a.length < 2) return 0;
      const span = t - a[0];
      return span > 0 ? ((a.length - 1) / span) * 1000 : 0;
    };
    let bytes = 0;
    for (const [, n] of this.rxBytesWindow) bytes += n;
    const spanMs = this.rxBytesWindow.length > 1 ? t - this.rxBytesWindow[0][0] : 0;
    return {
      ...this.stats,
      receivedFps: fps(this.inTimes),
      decodedFps: fps(this.outTimes),
      bitrateKbps: spanMs > 0 ? (bytes * 8) / spanMs : 0,
      jitterMs: this.jb.jitterEwmaMs,
      bufferMs: this.jb.appliedBufferMs,
      queued: this.jb.size,
      decodeQueue: this.decoder ? this.decoder.decodeQueueSize : 0,
      catchUpDropped: this.jb.catchUpDropped,
      needKeyframe: this.needKeyframe,
    };
  }

  close() {
    if (!this.closed && this.avSync) this.avSync.videoGone();
    this.closed = true;
    if (this.timer != null) clearTimeout(this.timer);
    this.timer = null;
    this.#endRepair();
    this.jb.reset();
    const d = this.decoder;
    this.decoder = null;
    try {
      if (d && d.state !== "closed") d.close();
    } catch {}
  }
}

// The renderer lives in renderer.js (WebGL first, 2D fallback); re-exported so existing imports keep working.
export { CanvasRenderer } from "./renderer.js";
