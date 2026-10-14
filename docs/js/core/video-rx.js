// Receive-side video pipeline for ONE watched stream:
//   decrypted access units -> (loss gate) -> jitter buffer -> WebCodecs VideoDecoder -> VideoFrame callback
//
// The codec (H.264 / H.265 / AV1) is sniffed from the first keyframe's bitstream; the WebCodecs codec
// string is derived from its SPS / sequence header and the decoder is re-configured whenever a later
// keyframe changes it (the sender may switch codec or encoder profile mid-stream).

import { deriveCodecString } from "./codec.js";
import { JitterBuffer } from "./jitter.js";

const FPS_WINDOW_MS = 1500;

export class VideoReceiver {
  /**
   * @param {object} o
   * @param {(frame:VideoFrame)=>void} o.onFrame   takes OWNERSHIP of the frame: call frame.close() promptly
   * @param {(type:string, detail?:any)=>void} [o.onEvent]  "codec" | "unsupported" | "decoder-error" | "need-keyframe"
   * @param {number} [o.maxJitterBufferMs]
   * @param {number} [o.catchUpSpeedPercent]
   * @param {"no-preference"|"prefer-hardware"|"prefer-software"} [o.hardwareAcceleration]
   */
  constructor({ onFrame, onEvent = () => {}, maxJitterBufferMs, catchUpSpeedPercent, hardwareAcceleration = "no-preference", now = () => performance.now() }) {
    this.onFrame = onFrame;
    this.onEvent = onEvent;
    this.hardwareAcceleration = hardwareAcceleration;
    this.now = now;
    this.jb = new JitterBuffer({ maxJitterBufferMs, catchUpSpeedPercent });
    this.decoder = null;
    this.config = null; // { codec: "h264"|..., string }
    this.needKeyframe = true; // true until a keyframe has been queued after start / loss / error
    this.unsupported = null; // codec string we know we cannot decode
    this.timer = null;
    this.closed = false;
    this.outTimes = [];
    this.inTimes = [];
    this.stats = {
      rxFrames: 0, rxBytes: 0, decoded: 0, skippedDeltas: 0, lossEvents: 0, decoderErrors: 0,
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
    VideoDecoder.isConfigSupported(cfg).then(
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
      optimizeForLatency: true,
      hardwareAcceleration: this.hardwareAcceleration,
    };
  }

  #createDecoder() {
    const decoder = new VideoDecoder({
      output: (frame) => {
        if (this.closed || decoder !== this.decoder) {
          frame.close();
          return;
        }
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
    try {
      decoder.configure(this.#decoderConfig());
    } catch (e) {
      this.#decoderFailed(e);
    }
  }

  #decoderFailed(e) {
    this.stats.decoderErrors++;
    this.needKeyframe = true;
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
    const { items } = this.jb.drain(this.now(), d.decodeQueueSize);
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

/**
 * Draws the most recent decoded frame on a canvas once per display refresh (requestAnimationFrame),
 * closing every frame it supersedes. At 240 fps on a 60 Hz display that is 1 draw instead of 4, and on
 * a 280 Hz display every frame is shown; either way the picture is the freshest one available.
 * Keeps no frame alive longer than one refresh, which matters because hardware decoders have a small
 * surface pool.
 */
export class CanvasRenderer {
  /** @param {HTMLCanvasElement|OffscreenCanvas} canvas */
  constructor(canvas, { onDraw = null } = {}) {
    this.canvas = canvas;
    this.ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
    this.latest = null;
    this.raf = 0;
    this.drawn = 0;
    this.onDraw = onDraw;
    this.stopped = false;
    this.tick = this.tick.bind(this);
  }

  /** Hand a frame over (takes ownership). */
  present(frame) {
    if (this.stopped) {
      frame.close();
      return;
    }
    if (this.latest) this.latest.close();
    this.latest = frame;
    if (!this.raf) this.raf = requestAnimationFrame(this.tick);
  }

  tick() {
    this.raf = 0;
    const f = this.latest;
    if (!f) return;
    this.latest = null;
    const w = f.displayWidth;
    const h = f.displayHeight;
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.canvas.width = w;
      this.canvas.height = h;
    }
    this.ctx.drawImage(f, 0, 0, w, h);
    f.close();
    this.drawn++;
    if (this.onDraw) this.onDraw();
  }

  stop() {
    this.stopped = true;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    if (this.latest) this.latest.close();
    this.latest = null;
  }
}
