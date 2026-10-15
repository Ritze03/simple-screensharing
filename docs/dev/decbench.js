// Dev-only: how many frames/s can this browser's VideoDecoder sustain for a given codec/size/config? Encodes N synthetic
// frames with the browser's own VideoEncoder (software), then decodes the chunks back-to-back as fast as the decoder allows
// (never more than 8 requests in flight, every VideoFrame closed at once) and reports frames/s per decoder config.
// Query: codec (h264|av1), w, h, n (frames), kbps, gop, variants (comma list of "latency:hw", e.g. "true:no-preference,false:no-preference")
// Added 2026-10-15 (task #67: Firefox stalls above 60 fps) to separate decoder capacity from transport/UI effects.
import { deriveCodecString } from "../js/core/codec.js";
import { kvGet, kvSet, log, params, postResult } from "./pagelog.js";

const q = (k, d) => (params.has(k) ? params.get(k) : d);
const codec = q("codec", "h264");
const W = Number(q("w", 1920));
const H = Number(q("h", 1080));
const N = Number(q("n", 360));
const kbps = Number(q("kbps", 12000));
const gop = Number(q("gop", 120));
const variants = q("variants", "true:no-preference,false:no-preference").split(",");
const out = document.getElementById("out");

async function encode() {
  const chunks = [];
  const cfg = codec === "av1"
    ? { codec: "av01.0.13M.08", width: W, height: H, bitrate: kbps * 1000, framerate: 120, latencyMode: "realtime" }
    : { codec: "avc1.640033", width: W, height: H, bitrate: kbps * 1000, framerate: 120, latencyMode: q("lat", "realtime"), avc: { format: "annexb" } };
  const enc = new VideoEncoder({
    output: (c) => {
      const data = new Uint8Array(c.byteLength);
      c.copyTo(data);
      chunks.push({ type: c.type, timestamp: c.timestamp, data });
    },
    error: (e) => log("encoder error", String(e)),
  });
  enc.configure(cfg);
  const cv = new OffscreenCanvas(W, H);
  const ctx = cv.getContext("2d");
  for (let n = 0; n < N; n++) {
    ctx.fillStyle = `hsl(${(n * 3) % 360} 40% 25%)`;
    ctx.fillRect(0, 0, W, H);
    for (let i = 0; i < 14; i++) {
      ctx.fillStyle = `rgb(${(Math.random() * 255) | 0} ${(Math.random() * 255) | 0} ${(Math.random() * 255) | 0})`;
      ctx.fillRect(Math.random() * (W - 400), Math.random() * (H - 250), 80 + Math.random() * 320, 60 + Math.random() * 190);
    }
    ctx.fillStyle = "#fff";
    ctx.font = "48px monospace";
    for (let l = 0; l < 6; l++) ctx.fillText(`line ${l} frame ${n} ${Math.random().toString(36)}`, 40, 80 + l * 70);
    const f = new VideoFrame(cv, { timestamp: Math.round((n * 1e6) / 120) });
    while (enc.encodeQueueSize > 4) await new Promise((r) => setTimeout(r, 2));
    enc.encode(f, { keyFrame: n % gop === 0 });
    f.close();
  }
  await enc.flush();
  enc.close();
  return chunks;
}

async function decodeRun(chunks, optimizeForLatency, hardwareAcceleration) {
  const string = deriveCodecString(chunks[0].data)?.string;
  let outFrames = 0;
  let err = null;
  let first = 0;
  const dec = new VideoDecoder({
    output: (f) => {
      if (!first) first = performance.now();
      outFrames++;
      f.close();
    },
    error: (e) => (err = String(e)),
  });
  dec.configure({ codec: string, optimizeForLatency, hardwareAcceleration });
  const t0 = performance.now();
  let maxQueue = 0;
  for (const c of chunks) {
    while (dec.decodeQueueSize >= 8 && !err) await new Promise((r) => (dec.ondequeue = () => r()));
    if (err) break;
    dec.decode(new EncodedVideoChunk({ type: c.type, timestamp: c.timestamp, data: c.data }));
    maxQueue = Math.max(maxQueue, dec.decodeQueueSize);
  }
  const submitted = performance.now() - t0;
  const flushed = await Promise.race([dec.flush().then(() => true, () => false), new Promise((r) => setTimeout(() => r("timeout"), 20000))]);
  const total = performance.now() - t0;
  try { dec.close(); } catch {}
  return {
    optimizeForLatency, hardwareAcceleration, codecString: string, frames: chunks.length, out: outFrames, flushed,
    fps: +((outFrames / total) * 1000).toFixed(1), submittedMs: Math.round(submitted), totalMs: Math.round(total), firstOutMs: first ? Math.round(first - t0) : null, maxQueue, err,
  };
}

// mode=both (default): encode here, then decode. mode=encode: encode and leave the chunks in the runner's kv store ("chunks").
// mode=decode: fetch them. Needed for Firefox, whose VideoEncoder fails without the OpenH264 plugin ("EncodingError: The given
// encoding is not supported" on a fresh profile): xbench.mjs encodes in Chromium and decodes in the browser under test.
const mode = q("mode", "both");
const toB64 = (u8) => {
  let s = "";
  for (let i = 0; i < u8.length; i += 0x8000) s += String.fromCharCode.apply(null, u8.subarray(i, i + 0x8000));
  return btoa(s);
};
const fromB64 = (b) => Uint8Array.from(atob(b), (c) => c.charCodeAt(0));

const res = { userAgent: navigator.userAgent, codec, size: `${W}x${H}`, kbps, gop, mode, runs: [] };
try {
  let chunks;
  if (mode === "decode" || mode === "pipeline" || mode === "draw") {
    const { meta, data } = JSON.parse(await kvGet("chunks", { timeoutMs: 120_000 }));
    const all = fromB64(data);
    let o = 0;
    chunks = meta.map(([type, timestamp, len]) => ({ type, timestamp, data: all.subarray(o, (o += len)) }));
  } else {
    chunks = await encode();
  }
  res.chunks = chunks.length;
  res.avgChunkBytes = Math.round(chunks.reduce((a, c) => a + c.data.length, 0) / chunks.length);
  res.keyframes = chunks.filter((c) => c.type === "key").length;
  log(`${mode === "decode" ? "fetched" : "encoded"} ${chunks.length} chunks, avg ${res.avgChunkBytes} B`);
  if (mode === "encode") {
    const all = new Uint8Array(chunks.reduce((a, c) => a + c.data.length, 0));
    let o = 0;
    for (const c of chunks) {
      all.set(c.data, o);
      o += c.data.length;
    }
    await kvSet("chunks", JSON.stringify({ meta: chunks.map((c) => [c.type, c.timestamp, c.data.length]), data: toB64(all) }));
  } else if (mode === "draw") {
    // Cost of putting a decoded VideoFrame on screen, per method (main thread, ms per frame): 2D drawImage, WebGL
    // texImage2D + draw (+ finish, so the GPU work is included), createImageBitmap + bitmaprenderer.
    const string = deriveCodecString(chunks[0].data).string;
    const held = [];
    await new Promise((done) => {
      const d = new VideoDecoder({ output: (f) => (held.length < 8 ? held.push(f) : f.close()), error: (e) => log("err " + e) });
      d.configure({ codec: string, optimizeForLatency: false });
      for (const c of chunks.slice(0, 40)) d.decode(new EncodedVideoChunk({ type: c.type, timestamp: c.timestamp, data: c.data }));
      d.flush().then(done, done);
    });
    const vw = held[0].displayWidth, vh = held[0].displayHeight;
    const timeIt = async (name, fn, reps = 120) => {
      for (let i = 0; i < 6; i++) await fn(held[i % held.length]);
      const t = performance.now();
      for (let i = 0; i < reps; i++) await fn(held[i % held.length]);
      res[name] = +((performance.now() - t) / reps).toFixed(2);
    };
    const c2 = Object.assign(document.createElement("canvas"), { width: vw, height: vh });
    const ctx2 = c2.getContext("2d", { alpha: false });
    await timeIt("draw2d_ms", (f) => ctx2.drawImage(f, 0, 0, vw, vh));
    const c2d = Object.assign(document.createElement("canvas"), { width: vw, height: vh });
    const ctx2d = c2d.getContext("2d", { alpha: false, desynchronized: true });
    await timeIt("draw2d_desync_ms", (f) => ctx2d.drawImage(f, 0, 0, vw, vh));
    const cg = Object.assign(document.createElement("canvas"), { width: vw, height: vh });
    const gl = cg.getContext("webgl2", { alpha: false, antialias: false, desynchronized: true });
    if (gl) {
      const sh = (type, src) => { const s = gl.createShader(type); gl.shaderSource(s, src); gl.compileShader(s); return s; };
      const p = gl.createProgram();
      gl.attachShader(p, sh(gl.VERTEX_SHADER, "#version 300 es\nout vec2 uv;void main(){vec2 p=vec2((gl_VertexID<<1)&2,gl_VertexID&2);uv=vec2(p.x,1.0-p.y);gl_Position=vec4(p*2.0-1.0,0,1);}"));
      gl.attachShader(p, sh(gl.FRAGMENT_SHADER, "#version 300 es\nprecision mediump float;in vec2 uv;uniform sampler2D t;out vec4 o;void main(){o=texture(t,uv);}"));
      gl.linkProgram(p);
      gl.useProgram(p);
      const tex = gl.createTexture();
      gl.bindTexture(gl.TEXTURE_2D, tex);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
      gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
      await timeIt("drawWebGL_ms", (f) => {
        gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, f);
        gl.drawArrays(gl.TRIANGLES, 0, 3);
        gl.finish();
      });
      res.glRenderer = (() => { const e = gl.getExtension("WEBGL_debug_renderer_info"); return e ? gl.getParameter(e.UNMASKED_RENDERER_WEBGL) : "?"; })();
    }
    const cb = Object.assign(document.createElement("canvas"), { width: vw, height: vh });
    const bm = cb.getContext("bitmaprenderer");
    await timeIt("createImageBitmap_ms", async (f) => { const b = await createImageBitmap(f); bm.transferFromImageBitmap(b); });
    res.frame = `${vw}x${vh}`;
    held.forEach((f) => f.close());
  } else if (mode === "pipeline") {
    // The real receive path (VideoReceiver: jitter buffer, decode-queue backstop, CanvasRenderer) fed at a fixed rate from the
    // recorded chunks, looping (chunk 0 is a keyframe, so the wrap is a clean GOP boundary). No network, no relay.
    const fps = Number(q("fps", 120));
    const seconds = Number(q("seconds", 10));
    const { VideoReceiver, CanvasRenderer } = await import("../js/core/video-rx.js");
    const canvas = document.createElement("canvas");
    canvas.style.cssText = "width:480px;height:270px";
    document.body.appendChild(canvas);
    const renderer = new CanvasRenderer(canvas, { webgl: q("gl", "1") !== "0" }); // gl=0: the pre-#67 2D-only renderer
    const events = {};
    const opts = {};
    if (params.has("latency")) opts.lowLatency = q("latency") === "1";
    if (params.has("adaptive")) opts.adaptiveLatency = q("adaptive") === "1"; // adaptive=0: never leave optimizeForLatency
    const rx = new VideoReceiver({
      onFrame: q("render", "1") === "0" ? (f) => f.close() : (f) => renderer.present(f),
      onEvent: (t, d) => (events[t + (d && d.reason ? ":" + d.reason : "")] = (events[t + (d && d.reason ? ":" + d.reason : "")] || 0) + 1),
      ...opts,
    });
    const period = 1000 / fps;
    const t0 = performance.now();
    let sent = 0;
    const samples = [];
    let lastS = { t: 0, dec: 0, drawn: 0 };
    let maxLagMs = 0;
    await new Promise((done) => {
      const pump = () => {
        const now = performance.now();
        const due = Math.floor((now - t0) / period) + 1;
        maxLagMs = Math.max(maxLagMs, now - t0 - sent * period);
        while (sent < due) {
          const c = chunks[sent % chunks.length];
          rx.pushFrame({ ptsUs: Math.round(sent * period * 1000), keyframe: c.type === "key", payload: c.data });
          sent++;
        }
        if (now - t0 >= (samples.length + 1) * 1000) {
          const s = rx.getStats();
          samples.push({ t: samples.length + 1, decoded: s.decoded - lastS.dec, drawn: renderer.drawn - lastS.drawn, queued: s.queued, decodeQueue: s.decodeQueue, bufferMs: Math.round(s.bufferMs), catchUpDropped: s.catchUpDropped });
          lastS = { dec: s.decoded, drawn: renderer.drawn };
        }
        if (now - t0 >= seconds * 1000) return done();
        setTimeout(pump, 1);
      };
      pump();
    });
    const s = rx.getStats();
    res.pipeline = { backend: renderer.backend, calibration: CanvasRenderer.lastCalibration, drawMs: +(renderer.drawMsEwma || 0).toFixed(2), minIntervalMs: +(renderer.minIntervalMs || 0).toFixed(1), fps, seconds, lowLatency: rx.lowLatency, throughputMode: s.throughputMode, sent, decodedFps: +(s.decoded / seconds).toFixed(1), drawnFps: +(renderer.drawn / seconds).toFixed(1), skippedDeltas: s.skippedDeltas, catchUpDropped: s.catchUpDropped, decoderErrors: s.decoderErrors, events, maxFeedLagMs: Math.round(maxLagMs), samples };
    rx.close();
    renderer.stop();
  } else {
    for (const rep of [0, 1]) {
      for (const v of variants) {
        const [lat, hw] = v.split(":");
        const r = await decodeRun(chunks, lat === "true", hw);
        r.rep = rep;
        res.runs.push(r);
        log(JSON.stringify(r));
      }
    }
  }
} catch (e) {
  res.error = String(e && e.stack ? e.stack : e);
}
out.textContent = JSON.stringify(res, null, 2);
await postResult(res);
