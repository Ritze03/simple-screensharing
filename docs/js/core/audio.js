// Opus audio, matching the native sender (client/src/audio.rs): raw Opus packets (no Ogg), 48 kHz
// stereo, 20 ms frames (opusenc defaults), 64 kbit/s; wire PTS in µs advancing 20 ms per packet, stamped with the
// capture time on the sender's one clock (shared with its video, see avsync.js) so the receiver can delay audio to match the picture.
//
//   AudioReceiver  decrypted packets -> WebCodecs AudioDecoder -> loudness leveller (leveller.js, #119) -> WebAudio (gain: volume 0..5, mute)
//   AudioSender    MediaStreamTrack -> WebCodecs AudioEncoder -> packets

export const OPUS_SAMPLE_RATE = 48000;
export const OPUS_CHANNELS = 2;
export const OPUS_FRAME_US = 20000;
export const OPUS_BITRATE = 64000;

// 500 %. A GainNode (HTMLMediaElement.volume would cap at 1). The volume acts after the leveller; above 1.0 loud audio clips at the device, the same hard clip as the native client.
export const MAX_VOLUME = 5;

import { DelayController } from "./avsync.js";
import { Leveller } from "./leveller.js";

export function opusSupported() {
  return typeof AudioDecoder !== "undefined" && typeof AudioEncoder !== "undefined";
}

export class AudioReceiver {
  /**
   * @param {object} o
   * @param {AudioContext} o.context
   * @param {AudioNode} [o.destination]   defaults to context.destination
   * @param {number} [o.latencyMs=80]     playout delay used to absorb network jitter
   * @param {number} [o.volume=1]         0..5
   * @param {boolean} [o.muted=false]
   * @param {boolean} [o.level=true]      level the loudness of what is played (slow AGC + instant peak limiter, leveller.js); the volume acts after it
   * @param {import("./avsync.js").AvSync} [o.avSync]  this sender's A/V sync state: audio is delayed to line up with its video
   * @param {() => number} [o.now]  local clock in ms for the sync floors (tests inject one)
   */
  constructor({ context, destination = context.destination, latencyMs = 80, volume = 1, muted = false, level = true, onEvent = () => {}, avSync = null, now = () => performance.now() }) {
    this.avSync = avSync;
    this.now = now;
    this.delay = new DelayController();
    this.syncDelayS = 0; // the A/V-sync delay currently applied on top of `latency`, seconds
    this.ctx = context;
    this.latency = latencyMs / 1000;
    this.onEvent = onEvent;
    this.gain = context.createGain();
    this.gain.connect(destination);
    this.volume = volume;
    this.muted = muted;
    this.level_ = !!level; // "Level loudness" switch (the `level` field is the peak meter)
    this.leveller = null;
    this.#applyGain();
    this.decoder = null;
    this.anchorPtsUs = null;
    this.anchorCtxTime = 0;
    this.lastEnd = 0;
    this.level = 0;
    this.stats = { packets: 0, decodedFrames: 0, underruns: 0, resyncs: 0, syncSteps: 0, errors: 0 };
    this.closed = false;
    this.sources = new Map(); // scheduled source -> its start time on the context clock
  }

  setVolume(v) {
    this.volume = Math.max(0, Math.min(MAX_VOLUME, Number(v) || 0));
    this.#applyGain();
  }
  /** Switch the loudness levelling on/off, live. Off passes the audio untouched and forgets the AGC state. */
  setLevelLoudness(on) {
    this.level_ = !!on;
    if (!this.level_ && this.leveller) this.leveller.reset();
  }
  setMuted(m) {
    this.muted = !!m;
    this.#applyGain();
  }
  #applyGain() {
    this.gain.gain.setTargetAtTime(this.muted ? 0 : this.volume, this.ctx.currentTime, 0.01);
  }

  #ensureDecoder() {
    if (this.decoder && this.decoder.state !== "closed") return this.decoder;
    const decoder = new AudioDecoder({
      output: (data) => this.#play(data),
      error: (e) => {
        this.stats.errors++;
        this.onEvent("decoder-error", { message: String(e && e.message ? e.message : e) });
        if (decoder === this.decoder) this.decoder = null;
      },
    });
    decoder.configure({ codec: "opus", sampleRate: OPUS_SAMPLE_RATE, numberOfChannels: OPUS_CHANNELS });
    this.decoder = decoder;
    return decoder;
  }

  /** Feed one decrypted Opus packet. */
  pushPacket({ ptsUs, payload }) {
    if (this.closed) return;
    this.stats.packets++;
    if (this.avSync) this.avSync.noteAudio(this.now(), ptsUs);
    try {
      this.#ensureDecoder().decode(new EncodedAudioChunk({ type: "key", timestamp: ptsUs, data: payload }));
    } catch (e) {
      this.stats.errors++;
      this.decoder = null;
    }
  }

  #play(data) {
    try {
      if (this.closed) return;
      const frames = data.numberOfFrames;
      const ch = Math.min(data.numberOfChannels, 2);
      const buf = this.ctx.createBuffer(ch, frames, data.sampleRate);
      let peak = 0;
      const planes = [];
      for (let c = 0; c < ch; c++) {
        const plane = new Float32Array(frames);
        data.copyTo(plane, { planeIndex: c, format: "f32-planar" });
        planes.push(plane);
      }
      if (this.level_) {
        if (!this.leveller || this.leveller.rate !== data.sampleRate) {
          this.leveller = new Leveller(data.sampleRate);
          this.leveller.rate = data.sampleRate;
        }
        this.leveller.process(planes);
      }
      for (let c = 0; c < ch; c++) {
        const plane = planes[c];
        buf.copyToChannel(plane, c);
        for (let i = 0; i < frames; i += 16) peak = Math.max(peak, Math.abs(plane[i]));
      }
      this.level = Math.max(peak, this.level * 0.97); // peak meter with a ~0.5 s hold (20 ms frames), not one frame's instant peak
      const now = this.ctx.currentTime;
      const ptsUs = data.timestamp;
      let when;
      // A/V sync: the delay that lines audio up with this sender's picture (0 for audio-only), stepped past a deadband.
      let wantDelayS = this.syncDelayS;
      if (this.avSync) wantDelayS = this.delay.update(this.avSync.targetDelayMs(this.latency * 1000), this.now()) / 1000;
      const delayChanged = this.anchorPtsUs != null && Math.abs(wantDelayS - this.syncDelayS) > 1e-6;
      this.syncDelayS = wantDelayS;
      const lead = this.latency + this.syncDelayS;
      if (this.anchorPtsUs == null) {
        this.anchorPtsUs = ptsUs;
        this.anchorCtxTime = now + lead;
      }
      when = this.anchorCtxTime + (ptsUs - this.anchorPtsUs) / 1e6;
      if (delayChanged) {
        // The video's latency moved: re-time audio once. Later = a gap of silence; earlier = drop what is already
        // queued past the new start, or it would play on top of the new audio.
        this.stats.syncSteps++;
        this.anchorPtsUs = ptsUs;
        this.anchorCtxTime = now + lead;
        when = this.anchorCtxTime;
        for (const [s, at] of this.sources) {
          if (at >= when - 0.001) {
            try {
              s.stop();
            } catch {}
            this.sources.delete(s);
          }
        }
      } else if (when < now) {
        // Underrun (or the context was suspended): re-anchor with a fresh latency budget.
        this.stats.underruns++;
        this.anchorPtsUs = ptsUs;
        this.anchorCtxTime = now + lead;
        when = this.anchorCtxTime;
      } else if (when > now + lead + this.latency * 3 + 0.2) {
        // Far in the future (burst, or sender clock ran ahead): pull back.
        this.stats.resyncs++;
        this.anchorPtsUs = ptsUs;
        this.anchorCtxTime = now + lead;
        when = this.anchorCtxTime;
      }
      const src = this.ctx.createBufferSource();
      src.buffer = buf;
      src.connect(this.gain);
      src.onended = () => this.sources.delete(src);
      this.sources.set(src, when);
      src.start(when);
      this.stats.decodedFrames++;
    } finally {
      data.close();
    }
  }

  close() {
    this.closed = true;
    try {
      if (this.decoder && this.decoder.state !== "closed") this.decoder.close();
    } catch {}
    for (const s of this.sources.keys()) {
      try {
        s.stop();
      } catch {}
    }
    this.sources.clear();
    try {
      this.gain.disconnect();
    } catch {}
  }
}

/**
 * Opus-encodes an audio MediaStreamTrack. Calls `onPacket({ ptsUs, payload })` per 20 ms packet.
 * `ptsUs` is relative to `originUs` (a media timestamp shared with the video encoder so the two stay in sync).
 */
export class AudioSender {
  constructor({ track, bitrate = OPUS_BITRATE, onPacket, onError = () => {}, clock }) {
    this.track = track;
    this.bitrate = bitrate;
    this.onPacket = onPacket;
    this.onError = onError;
    this.clock = clock; // { toPts(timestampUs) -> number }
    this.encoder = null;
    this.reader = null;
    this.stopped = false;
    this.packets = 0;
    this.lastPts = -1;
  }

  async start() {
    const cfg = {
      codec: "opus",
      sampleRate: OPUS_SAMPLE_RATE,
      numberOfChannels: OPUS_CHANNELS,
      bitrate: this.bitrate,
      opus: { frameDuration: OPUS_FRAME_US },
    };
    const sup = await AudioEncoder.isConfigSupported(cfg);
    if (!sup.supported) throw new Error("this browser cannot encode Opus");
    this.encoder = new AudioEncoder({
      output: (chunk) => {
        const payload = new Uint8Array(chunk.byteLength);
        chunk.copyTo(payload);
        let ptsUs = this.clock.toPts(chunk.timestamp);
        if (ptsUs <= this.lastPts) ptsUs = this.lastPts + 1;
        this.lastPts = ptsUs;
        this.packets++;
        this.onPacket({ ptsUs, payload });
      },
      error: (e) => this.onError(e),
    });
    this.encoder.configure(cfg);
    const processor = new MediaStreamTrackProcessor({ track: this.track });
    this.reader = processor.readable.getReader();
    this.#pump();
  }

  async #pump() {
    try {
      for (;;) {
        const { value: data, done } = await this.reader.read();
        if (done || this.stopped) {
          if (data) data.close();
          break;
        }
        try {
          if (this.encoder.encodeQueueSize < 50) this.encoder.encode(data);
        } finally {
          data.close();
        }
      }
    } catch (e) {
      if (!this.stopped) this.onError(e);
    }
  }

  stop() {
    this.stopped = true;
    try {
      this.reader && this.reader.cancel();
    } catch {}
    try {
      if (this.encoder && this.encoder.state !== "closed") this.encoder.close();
    } catch {}
  }
}
