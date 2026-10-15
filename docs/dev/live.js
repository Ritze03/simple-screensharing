// Live harness: one page = one peer joining a real relay (dev/live.html). Roles:
//
//   role=share  encode a synthetic test pattern (canvas.captureStream) and share it; measure the encode side
//   role=view   watch the first video stream whose owner name matches `owner` (default: any); measure the decode side
//
// Query: link (full join link), codec (h264|h265|av1), fps, w, h, bitrate (kbps), duration (s of measurement),
//        name, owner, pattern (counter|none), preset, hw (no-preference|prefer-hardware|prefer-software), audio (0|1)
// The result JSON is POSTed to the runner (dev/headless.mjs) and shown on the page.

import { CanvasRenderer, RoomClient, capabilities, probeEncoders } from "../js/core/index.js";
import { log, params, postResult } from "./pagelog.js";
import { makePatternSource, readPattern } from "./pattern.js";

const q = (k, d) => (params.has(k) ? params.get(k) : d);
const role = q("role", "view");
const link = q("link", "");
const codec = q("codec", "h264");
const fps = Number(q("fps", 60));
const W = Number(q("w", 1920));
const H = Number(q("h", 1080));
const bitrate = Number(q("bitrate", 8000));
const duration = Number(q("duration", 10));
const name = q("name", role);
const ownerFilter = q("owner", "");
const pattern = q("pattern", "counter");
const wantAudio = q("audio", "0") === "1";
const hw = q("hw", "no-preference");
const preset = q("preset", "balanced");
const source = q("source", "pattern"); // pattern = share the synthetic MediaStream; tab = real getDisplayMedia of this tab
const out = document.getElementById("out");
const show = (o) => (out.textContent = JSON.stringify(o, (_, v) => (typeof v === "bigint" ? `${v}n` : v), 2));

const pct = (arr, p) => (arr.length ? arr.slice().sort((a, b) => a - b)[Math.min(arr.length - 1, Math.floor((arr.length * p) / 100))] : null);
const avg = (arr) => (arr.length ? arr.reduce((a, b) => a + b, 0) / arr.length : null);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function main() {
  const result = { role, name, params: Object.fromEntries(params), caps: capabilities(), userAgent: navigator.userAgent };
  const client = new RoomClient({ displayName: name, hardwareAcceleration: hw });
  const events = [];
  for (const t of ["state", "error", "media", "share"]) {
    client.addEventListener(t, (e) => {
      const d = { ...e.detail };
      delete d.share;
      delete d.error;
      events.push({ t, at: Math.round(performance.now()), ...d });
      log(`event ${t}`, d);
    });
  }
  result.events = events;
  try {
    await client.connect(link, name);
    result.selfId = client.selfId;
    result.maxDatagramSize = client.session.maxDatagramSize;
    log("connected as", client.selfId, "maxDatagramSize", client.session.maxDatagramSize);

    if (role === "share") await runShare(client, result);
    else await runView(client, result);
  } catch (e) {
    result.error = String(e && e.stack ? e.stack : e);
    result.errorName = e && e.name;
    result.errorMessage = e && e.message;
    result.mayBeExpiredLink = !!(e && e.mayBeExpiredLink);
    log("ERROR", result.error);
  }
  try {
    await client.disconnect();
  } catch {}
  show(result);
  await postResult(result);
}

async function runShare(client, result) {
  const src = makePatternSource(W, H, fps, { audio: wantAudio, speaker: source === "tab" });
  const probes = await probeEncoders({ width: W, height: H, fps, bitrateKbps: bitrate });
  result.encoderProbe = probes;
  log("encoder probe", probes);
  let share;
  try {
    const real = source === "tab" ? { displayMediaOptions: { preferCurrentTab: true, selfBrowserSurface: "include", systemAudio: "include" } } : { stream: src.stream };
    share = await client.startShare({ ...real, codec, fps, bitrateKbps: bitrate, audio: wantAudio, preset, hardwareAcceleration: hw, label: `Test ${codec} ${fps}fps` });
    result.shareTracks = {
      video: share.videoTrack.getSettings(),
      audio: share.audioTrack ? share.audioTrack.getSettings() : null,
      hasAudioStream: share.audioStreamId !== null,
    };
  } catch (e) {
    src.stop();
    throw e;
  }
  result.codecString = share.codecString;
  log("sharing", share.codecString, "video stream", share.videoStreamId);
  // Wait for the first viewer (encoding is paused until then), then measure for `duration`.
  const t0 = performance.now();
  while (share.subscribers === 0 && performance.now() - t0 < 40_000) await sleep(100);
  result.waitedForViewerMs = Math.round(performance.now() - t0);
  if (share.subscribers === 0) throw new Error("no viewer subscribed within 40 s");
  const samples = [];
  const base = share.stats();
  const srcBase = { ...src.stats };
  const start = performance.now();
  while (performance.now() - start < (duration + 6) * 1000) {
    await sleep(1000);
    const s = share.stats();
    samples.push({ t: Math.round(performance.now() - start), encodedFps: +s.encodedFps.toFixed(1), kbps: Math.round(s.bitrateKbps), queue: s.encodeQueue, txDropped: s.txDropped });
  }
  const end = share.stats();
  const elapsed = (performance.now() - start) / 1000;
  result.source = { generatedFps: +((src.stats.generated - srcBase.generated) / elapsed).toFixed(1) };
  result.sender = {
    ...end,
    capturedFps: +((end.captured - base.captured) / elapsed).toFixed(1),
    // averaged over the seconds in which a viewer was subscribed (the encoder idles before/after)
    avgEncodedFps: +(avg(samples.filter((x) => x.encodedFps > 1).map((x) => x.encodedFps)) ?? 0).toFixed(1),
    avgKbps: +(avg(samples.filter((x) => x.kbps > 1).map((x) => x.kbps)) ?? 0).toFixed(0),
    elapsedS: +elapsed.toFixed(1),
    samples,
  };
  src.stop();
  share.stop();
  await sleep(500);
}

async function runView(client, result) {
  // wait for the sharer's video stream
  const t0 = performance.now();
  let info = null;
  while (!info && performance.now() - t0 < 50_000) {
    info = [...client.streams.values()].find((s) => !s.self && s.kind === "video" && (!ownerFilter || (client.members.get(s.owner)?.name ?? "").includes(ownerFilter)));
    if (!info) await sleep(100);
  }
  if (!info) throw new Error(`no video stream appeared (owner filter "${ownerFilter}")`);
  result.stream = { ...info, ownerName: client.members.get(info.owner)?.name, ownerId: undefined };
  log("watching", info.key, info.label, `${info.width}x${info.height}`);

  const canvas = document.createElement("canvas");
  canvas.style.cssText = "width:480px;height:270px;background:#000";
  document.body.appendChild(canvas);
  const lat = [];
  const counters = [];
  let sampleBusy = false;
  let n = 0;
  const renderer = new CanvasRenderer(canvas, { preserveDrawingBuffer: true }); // toDataURL snapshot below
  // tee: sample the pattern from a clone, then render
  const watch = client.watch(info.key, {
    audio: wantAudio,
    onFrame: (frame) => {
      n++;
      if (pattern === "counter" && !sampleBusy && n % 4 === 0) {
        sampleBusy = true;
        const c = frame.clone();
        const arrivedAt = Date.now() & 0xffffff;
        readPattern(c)
          .then(
            (p) => {
              const l = (arrivedAt - p.ts + 0x1000000) & 0xffffff;
              if (l < 5000) lat.push(l);
              counters.push(p.counter);
            },
            () => {},
          )
          .finally(() => {
            c.close();
            sampleBusy = false;
          });
      }
      renderer.present(frame);
    },
  });

  // let it settle (first keyframe, jitter buffer), then measure
  await sleep(2500);
  const statsBase = watch.stats();
  const drawnBase = renderer.drawn;
  lat.length = 0;
  counters.length = 0;
  const samples = [];
  const start = performance.now();
  while (performance.now() - start < duration * 1000) {
    await sleep(1000);
    const s = watch.stats();
    if (!s) throw new Error("stream ended while measuring");
    samples.push({ t: Math.round(performance.now() - start), decodedFps: +s.decodedFps.toFixed(1), receivedFps: +s.receivedFps.toFixed(1), kbps: Math.round(s.bitrateKbps), bufferMs: Math.round(s.bufferMs), jitterMs: +s.jitterMs.toFixed(1) });
  }
  const end = watch.stats();
  const elapsed = (performance.now() - start) / 1000;
  // sequence continuity from sampled counters (every 4th decoded frame): gaps = frames the sender produced that never showed up
  let regress = 0;
  for (let i = 1; i < counters.length; i++) if (((counters[i] - counters[i - 1] + 0x10000) & 0xffff) > 0x8000) regress++;
  result.receiver = {
    codec: end.codec,
    codecString: end.codecString,
    resolution: `${end.width}x${end.height}`,
    avgDecodedFps: +((end.decoded - statsBase.decoded) / elapsed).toFixed(1),
    avgReceivedFps: +((end.rxFrames - statsBase.rxFrames) / elapsed).toFixed(1),
    drawnFps: +((renderer.drawn - drawnBase) / elapsed).toFixed(1),
    avgKbps: +(((end.rxBytes - statsBase.rxBytes) * 8) / elapsed / 1000).toFixed(0),
    lossEvents: end.lossEvents - statsBase.lossEvents,
    droppedFramesReassembly: end.droppedFrames,
    decoderErrors: end.decoderErrors,
    decryptFailures: end.decryptFailures,
    skippedDeltas: end.skippedDeltas,
    catchUpDropped: end.catchUpDropped,
    bufferMs: Math.round(end.bufferMs),
    latencyMs: lat.length ? { n: lat.length, avg: +avg(lat).toFixed(1), p50: pct(lat, 50), p95: pct(lat, 95), max: Math.max(...lat) } : null,
    counterSamples: counters.length,
    counterRegressions: regress,
    elapsedS: +elapsed.toFixed(1),
    samples,
  };
  try {
    result.snapshotJpegDataUrl = canvas.toDataURL("image/jpeg", 0.6);
  } catch {}
  // previews (30 s cadence; the relay replays the cached one on join) and audio, if any
  result.previews = [];
  for (const [key, p] of client.previews) {
    let dims = null;
    try {
      const bmp = await createImageBitmap(p.blob);
      dims = `${bmp.width}x${bmp.height}`;
      bmp.close();
    } catch {}
    result.previews.push({ key, bytes: p.blob.size, type: p.blob.type, dims, ageMs: Date.now() - p.receivedAt });
  }
  if (watch.audioKey) {
    const a = client.watchStats(watch.audioKey);
    result.audio = a && { key: watch.audioKey, packets: a.packets, decodedFrames: a.decodedFrames, underruns: a.underruns, resyncs: a.resyncs, errors: a.errors, decryptFailures: a.decryptFailures, peakLevel: a.level };
  }
  watch.stop();
  await sleep(300);
}

main();
