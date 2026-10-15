// Send-side video: MediaStreamTrack -> WebCodecs VideoEncoder -> access units in the exact byte shape
// native receivers sniff (client/src/decode/codec.rs):
//   H.264 / H.265  Annex-B, SPS/PPS(/VPS) inline on every keyframe     (avc/hevc: { format: "annexb" })
//   AV1            raw OBU temporal unit starting with a temporal delimiter (12 00), sequence header on keyframes
// Wire PTS is µs, monotonic, starting at ~0 (like the native sender's per-destination wire PTS).

import { sniffCodec } from "./codec.js";

export const DEFAULT_GOP_MS = 2000; // client/src/capture.rs DEFAULT_GOP_MILLIS
export const KEYFRAME_COALESCE_MS = 100; // spec §9: ignore a KeyframeRequest within ~100 ms of an IDR

/**
 * Native encoder presets (client/src/presets.rs) mapped onto what WebCodecs can express.
 *  - tune LowLatency   -> latencyMode "realtime"   (BestQuality -> "quality")
 *  - rate control: not a setting (#88). Always bitrateMode "variable" (native Dynamic = VA vbr). The native Static
 *    (CQP) mode was removed after it froze streams on motion; WebCodecs has no per-frame-QP path here anyway.
 *  - GOP               -> forced keyframe interval (WebCodecs has no GOP knob; we call encode({keyFrame}))
 *  - quality level (x264 speed-preset / VA target-usage): NATIVE-ONLY, no WebCodecs equivalent.
 */
export const ENCODER_PRESETS = Object.freeze({
  low_latency: { latencyMode: "realtime", gopMs: DEFAULT_GOP_MS },
  balanced: { latencyMode: "realtime", gopMs: DEFAULT_GOP_MS },
  quality: { latencyMode: "quality", gopMs: DEFAULT_GOP_MS },
  gaming: { latencyMode: "realtime", gopMs: 750 },
});

// H.264 levels: [name byte, MaxMBPS, MaxFS]
const AVC_LEVELS = [
  [0x1f, 108000, 3600], [0x28, 245760, 8192], [0x2a, 522240, 8704], [0x32, 589824, 22080],
  [0x33, 983040, 36864], [0x34, 2073600, 36864], [0x3c, 4177920, 139264], [0x3d, 8355840, 139264], [0x3e, 16711680, 139264],
];
function avcLevelFor(width, height, fps) {
  const mbs = Math.ceil(width / 16) * Math.ceil(height / 16);
  for (const [lvl, maxMbps, maxFs] of AVC_LEVELS) if (mbs <= maxFs && mbs * fps <= maxMbps) return lvl;
  return 0x3e;
}
const h2 = (n) => n.toString(16).padStart(2, "0");

/** Ordered candidate encoder configs per codec (first supported wins). */
export function encoderCandidates(codec, { width, height, fps }) {
  const lvl = avcLevelFor(width, height, fps);
  switch (codec) {
    case "h264":
      return [
        { codec: `avc1.6400${h2(lvl)}`, avc: { format: "annexb" } }, // High
        { codec: `avc1.4d00${h2(lvl)}`, avc: { format: "annexb" } }, // Main
        { codec: `avc1.4200${h2(lvl)}`, avc: { format: "annexb" } }, // Baseline
      ];
    case "h265":
      return [
        { codec: "hev1.1.6.L153.B0", hevc: { format: "annexb" } },
        { codec: "hvc1.1.6.L153.B0", hevc: { format: "annexb" } },
      ];
    case "av1":
      return [{ codec: "av01.0.13M.08" }, { codec: "av01.0.08M.08" }];
    default:
      throw new Error(`unknown codec ${codec}`);
  }
}

/**
 * Which codecs can this browser encode at the given size / rate? Resolves
 * `[{ codec, string, hardware }]`, best candidate per codec; unsupported codecs are omitted.
 */
export async function probeEncoders({ width = 1920, height = 1080, fps = 60, bitrateKbps = 8000, hardwareAcceleration = "no-preference" } = {}) {
  const out = [];
  if (typeof VideoEncoder === "undefined") return out;
  for (const codec of ["h264", "h265", "av1"]) {
    for (const cand of encoderCandidates(codec, { width, height, fps })) {
      const cfg = { ...cand, width, height, framerate: fps, bitrate: bitrateKbps * 1000, latencyMode: "realtime", hardwareAcceleration };
      try {
        const r = await VideoEncoder.isConfigSupported(cfg);
        if (r.supported) {
          let hardware = null;
          for (const hw of ["prefer-hardware", "prefer-software"]) {
            try {
              const h = await VideoEncoder.isConfigSupported({ ...cfg, hardwareAcceleration: hw });
              if (h.supported && hw === "prefer-hardware") hardware = "maybe";
            } catch {}
          }
          out.push({ codec, string: cand.codec, hardware });
          break;
        }
      } catch {}
    }
  }
  return out;
}

/** Shared sender-side media clock: wire PTS = capture timestamp - first timestamp seen, µs, monotonic. */
export class MediaClock {
  constructor() {
    this.origin = null;
    this.last = -1;
  }
  toPts(timestampUs) {
    if (this.origin == null) this.origin = timestampUs;
    let pts = Math.max(0, timestampUs - this.origin);
    if (pts <= this.last) pts = this.last + 1;
    this.last = pts;
    return pts;
  }
}

const AV1_TD = Uint8Array.of(0x12, 0x00);

export class VideoSender {
  /**
   * @param {object} o
   * @param {MediaStreamTrack} o.track
   * @param {"h264"|"h265"|"av1"} o.codec
   * @param {number} o.fps  requested frame rate (the encoder is also told; excess captured frames are skipped)
   * @param {number} o.bitrateKbps
   * @param {(u:{ptsUs:number, keyframe:boolean, payload:Uint8Array})=>void} o.onUnit
   * @param {()=>boolean} o.isActive         false = nobody watches: don't encode (previews still sample frames)
   * @param {(frame:VideoFrame)=>void} [o.onCapturedFrame]  called for EVERY captured frame before it is encoded or dropped
   */
  constructor({
    track, codec, fps, bitrateKbps, onUnit, isActive = () => true, onCapturedFrame = null, onError = () => {},
    clock = new MediaClock(), gopMs = DEFAULT_GOP_MS, latencyMode = "realtime",
    hardwareAcceleration = "no-preference", maxEncodeQueue = 4, now = () => performance.now(),
  }) {
    Object.assign(this, { track, codec, fps, bitrateKbps, onUnit, isActive, onCapturedFrame, onError, clock, gopMs, latencyMode, hardwareAcceleration, maxEncodeQueue, now });
    this.encoder = null;
    this.reader = null;
    this.stopped = false;
    this.cfg = null;
    this.width = 0;
    this.height = 0;
    this.wantKeyframe = true;
    this.lastKeyframeAt = -Infinity;
    this.lastEncodedTs = -Infinity;
    this.wasActive = false;
    this.lastFrame = null; // newest captured frame, kept open so a static source can still be re-encoded as a keyframe
    this.lastFrameAt = -Infinity;
    this.keepaliveTimer = null;
    this.stats = {
      captured: 0, encoded: 0, keepaliveKeyframes: 0, skippedIdle: 0, skippedRate: 0, droppedBackpressure: 0, keyframes: 0,
      bytes: 0, keyframesWithoutParams: 0, reconfigures: 0, codecString: null, av1TdAdded: 0, av1SeqHdrAdded: 0,
    };
    this.encTimes = [];
    this.byteTimes = [];
  }

  /** Ask for a keyframe on the next frame (coalesced: ignored if one was just produced). */
  requestKeyframe() {
    if (this.now() - this.lastKeyframeAt < KEYFRAME_COALESCE_MS) return;
    this.wantKeyframe = true;
  }

  /** Next frame is a keyframe, no coalescing (used when a frame was dropped locally and the delta chain is broken). */
  forceKeyframe() {
    this.wantKeyframe = true;
  }

  async start() {
    const s = this.track.getSettings();
    this.width = s.width || 1920;
    this.height = s.height || 1080;
    await this.#configure(this.width, this.height);
    const processor = new MediaStreamTrackProcessor({ track: this.track });
    this.reader = processor.readable.getReader();
    this.#pump();
    this.keepaliveTimer = setInterval(() => this.#keepalive(), 100);
  }

  // Screen capture is damage-driven: a static screen delivers NO frames, so a keyframe requested for a new viewer (or the
  // periodic GOP keyframe) would wait until something moves. Re-encode the newest frame as a keyframe instead.
  #keepalive() {
    const f = this.lastFrame;
    const enc = this.encoder;
    if (!f || this.stopped || !enc || enc.state !== "configured" || !this.isActive()) return;
    const t = this.now();
    const idleMs = t - this.lastFrameAt;
    if (idleMs < 250) return; // frames are flowing; the pump handles keyframes
    if (!(this.wantKeyframe || t - this.lastKeyframeAt >= this.gopMs)) return;
    if (enc.encodeQueueSize > this.maxEncodeQueue) return;
    let copy;
    try {
      copy = new VideoFrame(f, { timestamp: f.timestamp + Math.round(idleMs * 1000) });
      this.wantKeyframe = false;
      this.lastEncodedTs = copy.timestamp;
      this.stats.keepaliveKeyframes++;
      enc.encode(copy, { keyFrame: true });
    } catch (e) {
      // frame already closed / encoder closing: ignore, the next real frame takes over
    } finally {
      if (copy) copy.close();
    }
  }

  async #configure(width, height) {
    let chosen = null;
    for (const cand of encoderCandidates(this.codec, { width, height, fps: this.fps })) {
      const cfg = {
        ...cand, width, height, framerate: this.fps, bitrate: Math.round(this.bitrateKbps * 1000),
        bitrateMode: "variable", latencyMode: this.latencyMode, hardwareAcceleration: this.hardwareAcceleration,
      };
      let r;
      try {
        r = await VideoEncoder.isConfigSupported(cfg);
      } catch {
        continue;
      }
      if (r.supported) {
        chosen = r.config ? { ...cfg, ...r.config } : cfg;
        break;
      }
    }
    if (!chosen) throw new Error(`this browser cannot encode ${this.codec.toUpperCase()} at ${width}x${height}@${this.fps}`);
    if (this.encoder && this.encoder.state !== "closed") {
      try {
        this.encoder.close();
      } catch {}
    }
    this.cfg = chosen;
    this.width = width;
    this.height = height;
    this.stats.codecString = chosen.codec;
    this.encoder = new VideoEncoder({
      output: (chunk, meta) => this.#onChunk(chunk, meta),
      error: (e) => this.onError(e),
    });
    this.encoder.configure(chosen);
    this.wantKeyframe = true;
  }

  #onChunk(chunk, meta) {
    let payload = new Uint8Array(chunk.byteLength);
    chunk.copyTo(payload);
    const keyframe = chunk.type === "key";
    if (this.codec === "av1") payload = this.#fixAv1(payload, keyframe, meta);
    if (keyframe && !sniffCodec(payload)) this.stats.keyframesWithoutParams++;
    const t = this.now();
    this.stats.encoded++;
    this.stats.bytes += payload.length;
    if (keyframe) {
      this.stats.keyframes++;
      this.lastKeyframeAt = t;
    }
    this.encTimes.push(t);
    this.byteTimes.push([t, payload.length]);
    while (this.encTimes.length && this.encTimes[0] < t - 1500) this.encTimes.shift();
    while (this.byteTimes.length && this.byteTimes[0][0] < t - 1500) this.byteTimes.shift();
    this.onUnit({ ptsUs: this.clock.toPts(chunk.timestamp), keyframe, payload });
  }

  // Native AV1 units are `TD (12 00) [SEQ_HDR ...] FRAME...`; WebCodecs output may omit the TD, and a keyframe
  // must carry its sequence header (else take it from the decoder config's av1C `configOBUs`).
  #fixAv1(payload, keyframe, meta) {
    let out = payload;
    if (keyframe && !sniffCodec(out)) {
      const desc = meta && meta.decoderConfig && meta.decoderConfig.description;
      if (desc) {
        const seq = new Uint8Array(desc.buffer ? desc.buffer.slice(desc.byteOffset, desc.byteOffset + desc.byteLength) : desc).subarray(4);
        const merged = new Uint8Array(seq.length + out.length);
        merged.set(seq, 0);
        merged.set(out, seq.length);
        out = merged;
        this.stats.av1SeqHdrAdded++;
      }
    }
    if (out[0] !== 0x12) {
      this.stats.av1TdAdded++;
      const merged = new Uint8Array(2 + out.length);
      merged.set(AV1_TD, 0);
      merged.set(out, 2);
      out = merged;
    }
    return out;
  }

  async #pump() {
    const minGapUs = (1e6 / this.fps) * 0.75; // never encode meaningfully faster than requested
    try {
      for (;;) {
        const { value: frame, done } = await this.reader.read();
        if (done || this.stopped) {
          if (frame) frame.close();
          break;
        }
        this.stats.captured++;
        // Keep the newest frame open (released when the next one arrives) for #keepalive.
        if (this.lastFrame) this.lastFrame.close();
        this.lastFrame = frame;
        this.lastFrameAt = this.now();
        try {
          if (this.onCapturedFrame) this.onCapturedFrame(frame);
          const active = this.isActive();
          if (!active) {
            this.wasActive = false;
            this.stats.skippedIdle++;
            continue;
          }
          if (!this.wasActive) {
            this.wasActive = true;
            this.wantKeyframe = true; // resuming from idle: viewers need an IDR
          }
          if (frame.timestamp - this.lastEncodedTs < minGapUs) {
            this.stats.skippedRate++;
            continue;
          }
          const w = frame.displayWidth;
          const h = frame.displayHeight;
          if ((w !== this.width || h !== this.height) && w > 0 && h > 0) {
            this.stats.reconfigures++;
            await this.#configure(w, h);
          }
          const enc = this.encoder;
          if (!enc || enc.state !== "configured") continue;
          if (enc.encodeQueueSize > this.maxEncodeQueue) {
            // Encoder can't keep up: drop this INPUT frame. Nothing was encoded, so no reference chain is broken
            // (a keyframe is only needed when an already-encoded frame is lost, see RoomClient#sendMedia).
            this.stats.droppedBackpressure++;
            continue;
          }
          const t = this.now();
          const key = this.wantKeyframe || t - this.lastKeyframeAt >= this.gopMs;
          this.wantKeyframe = false;
          this.lastEncodedTs = frame.timestamp;
          enc.encode(frame, key ? { keyFrame: true } : undefined);
        } finally {
          // `frame` stays open as lastFrame; it is closed when superseded (or on stop)
        }
      }
    } catch (e) {
      if (!this.stopped) this.onError(e);
    }
  }

  setBitrate(kbps) {
    this.bitrateKbps = kbps;
    if (this.encoder && this.encoder.state === "configured") {
      this.encoder.configure({ ...this.cfg, bitrate: Math.round(kbps * 1000) });
      this.cfg = { ...this.cfg, bitrate: Math.round(kbps * 1000) };
      this.wantKeyframe = true;
    }
  }

  getStats() {
    const t = this.now();
    let fps = 0;
    if (this.encTimes.length > 1) fps = ((this.encTimes.length - 1) / (t - this.encTimes[0])) * 1000;
    let kbps = 0;
    if (this.byteTimes.length > 1) {
      let b = 0;
      for (const [, n] of this.byteTimes) b += n;
      kbps = (b * 8) / (t - this.byteTimes[0][0]);
    }
    return { ...this.stats, encodedFps: fps, bitrateKbps: kbps, width: this.width, height: this.height, encodeQueue: this.encoder ? this.encoder.encodeQueueSize : 0 };
  }

  stop() {
    this.stopped = true;
    clearInterval(this.keepaliveTimer);
    try {
      this.reader && this.reader.cancel();
    } catch {}
    try {
      if (this.encoder && this.encoder.state !== "closed") this.encoder.close();
    } catch {}
    if (this.lastFrame) {
      try {
        this.lastFrame.close();
      } catch {}
      this.lastFrame = null;
    }
  }
}
