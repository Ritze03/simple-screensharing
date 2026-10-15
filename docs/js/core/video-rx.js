// Receive-side video pipeline for ONE watched stream:
//   decrypted access units -> (loss gate) -> jitter buffer -> WebCodecs VideoDecoder -> VideoFrame callback
//
// The codec (H.264 / H.265 / AV1) is sniffed from the first keyframe's bitstream; the WebCodecs codec
// string is derived from its SPS / sequence header and the decoder is re-configured whenever a later
// keyframe changes it (the sender may switch codec or encoder profile mid-stream).

import { deriveCodecString } from "./codec.js";
import { JitterBuffer } from "./jitter.js";

const FPS_WINDOW_MS = 1500;
const MAX_PADDING = 64; // same constant as client/src/decode/frame.rs
// Gecko (Firefox, Floorp ...) only: see VideoReceiver#run.
const IS_GECKO = typeof navigator !== "undefined" && /\bFirefox\/\d/.test(navigator.userAgent);

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
   * @param {(type:string, detail?:any)=>void} [o.onEvent]  "codec" | "unsupported" | "decoder-error" | "need-keyframe"
   * @param {number} [o.maxJitterBufferMs]
   * @param {number} [o.catchUpSpeedPercent]
   * @param {"no-preference"|"prefer-hardware"|"prefer-software"} [o.hardwareAcceleration]
   */
  constructor({ onFrame, announcedSize = () => null, onEvent = () => {}, maxJitterBufferMs, catchUpSpeedPercent, hardwareAcceleration = "no-preference", lowLatency = true, adaptiveLatency = IS_GECKO, now = () => performance.now(), DecoderClass = null }) {
    this.DecoderClass = DecoderClass; // tests inject a fake decoder; default is the global VideoDecoder
    this.onFrame = onFrame;
    this.announcedSize = announcedSize; // () => {width,height}|null: the size the sender announced for this stream
    this.onEvent = onEvent;
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
      width: 0, height: 0, codec: null, codecString: null, lastPtsUs: 0,
    };
    this.rxBytesWindow = [];
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
    if (!this.needKeyframe) {
      this.needKeyframe = true;
      this.waitingSince = this.now();
      this.jb.reset(); // queued deltas would decode against a hole
      this.onEvent("need-keyframe", { reason: "loss" });
    }
  }

  /** Feed one decrypted access unit, in wire order. */
  pushFrame({ ptsUs, keyframe, payload }) {
    if (this.closed) return;
    const t = this.now();
    this.stats.rxFrames++;
    this.stats.rxBytes += payload.length;
    this.stats.lastPtsUs = ptsUs;
    this.#window(this.inTimes, t);
    this.rxBytesWindow.push([t, payload.length]);
    while (this.rxBytesWindow.length && this.rxBytesWindow[0][0] < t - FPS_WINDOW_MS) this.rxBytesWindow.shift();

    if (keyframe) {
      const info = deriveCodecString(payload);
      if (info && (!this.config || info.string !== this.config.string)) {
        this.#configure(info);
      }
      if (!this.config || this.unsupported === this.config.string) return;
      if (!this.decoder || this.decoder.state === "closed") this.#createDecoder();
      this.needKeyframe = false;
    } else if (this.needKeyframe || !this.config || this.unsupported === this.config.string) {
      this.stats.skippedDeltas++;
      // Still no keyframe a second after we started waiting: the one we asked for may itself have been lost, so say so
      // again (the room client re-requests, rate limited). Without this a lost recovery keyframe stalls until the next GOP.
      if (this.needKeyframe && t - this.waitingSince > 1000 && t - this.lastWaitEvent > 1000) {
        this.lastWaitEvent = t;
        this.onEvent("need-keyframe", { reason: "waiting" });
      }
      return;
    }
    const chunk = new EncodedVideoChunk({ type: keyframe ? "key" : "delta", timestamp: ptsUs, data: payload });
    this.jb.push(chunk, { ptsUs, keyframe }, t);
    this.#run();
    this.#schedule();
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
    this.stats.decoderErrors++;
    this.needKeyframe = true;
    this.waitingSince = this.now();
    this.jb.reset();
    const d = this.decoder;
    this.decoder = null;
    try {
      if (d && d.state !== "closed") d.close();
    } catch {}
    this.onEvent("decoder-error", { message: String(e && e.message ? e.message : e) });
    this.onEvent("need-keyframe", { reason: "decoder-error" });
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
    this.closed = true;
    if (this.timer != null) clearTimeout(this.timer);
    this.timer = null;
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
