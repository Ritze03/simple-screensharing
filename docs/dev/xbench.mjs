#!/usr/bin/env node
// Dev-only: decoder throughput of one browser for chunks encoded by Chromium (decbench.html, mode=encode then mode=decode).
//
//   node docs/dev/xbench.mjs [--browser firefox|chromium] [--codec h264|av1] [--size 1920x1080] [--n 240] [--kbps 12000]
//        [--variants true:no-preference,false:no-preference] [--flag pref=value]...
//
// Chromium encodes (Firefox has no usable software H.264 encoder without the OpenH264 plugin), the chosen browser decodes.
import { resolve } from "node:path";
import { launchChromium, startServer } from "./headless.mjs";

const o = { browser: "firefox", codec: "h264", size: "1920x1080", n: "240", kbps: "12000", variants: "true:no-preference,false:no-preference", flags: [] };
const a = process.argv.slice(2);
for (let i = 0; i < a.length; i++) {
  if (!a[i].startsWith("--")) continue;
  const k = a[i].slice(2);
  if (k === "flag") o.flags.push(a[++i]);
  else o[k] = a[++i];
}
const [w, h] = o.size.split("x");
// --docs <dir>: serve another copy of docs/ (e.g. the tree before a change, for before/after numbers)
const { server, port, results, waitersResult } = await startServer({ onLog: (id, m) => process.env.VERBOSE && console.log(`[${id}] ${m}`), ...(o.docs ? { docs: resolve(o.docs) } : {}) });
// --mode pipeline [--fps 120 --seconds 10 --latency 0|1]: feed the recorded chunks through the real VideoReceiver at a fixed rate
const decMode = o.mode || "decode";
const url = (id, mode) => `http://127.0.0.1:${port}/simple-screensharing/dev/decbench.html?id=${id}&mode=${mode === "decode" ? decMode : mode}&codec=${o.codec}&w=${w}&h=${h}&n=${o.n}&kbps=${o.kbps}&variants=${o.variants}${o.fps ? `&fps=${o.fps}` : ""}${o.seconds ? `&seconds=${o.seconds}` : ""}${o.latency ? `&latency=${o.latency}` : ""}${o.render ? `&render=${o.render}` : ""}${o.gl ? `&gl=${o.gl}` : ""}${o.adaptive ? `&adaptive=${o.adaptive}` : ""}`;
const procs = [];
const waitFor = (id, ms = 150_000) =>
  new Promise((ok, fail) => {
    const t = setTimeout(() => fail(new Error(`timeout waiting for page ${id}`)), ms);
    const check = () => {
      if (results.has(id)) {
        clearTimeout(t);
        ok(results.get(id));
      }
    };
    waitersResult.push(check);
    check();
  });
try {
  procs.push(launchChromium(url("0", "encode"), { browser: "chromium" }));
  const enc = await waitFor("0");
  if (enc.error) throw new Error("encode: " + enc.error);
  procs.push(launchChromium(url("1", "decode"), { browser: o.browser, flags: o.flags }));
  const dec = await waitFor("1");
  console.log(`${dec.userAgent.split(" ").slice(-1)[0]} ${dec.codec} ${dec.size} avgChunk ${dec.avgChunkBytes} B, ${dec.keyframes} keyframes${dec.error ? " ERROR " + dec.error : ""}`);
  if (dec.draw2d_ms !== undefined) console.log("  draw cost, ms/frame on the main thread: " + Object.entries(dec).filter(([k]) => /_ms$|glRenderer|^frame$/.test(k)).map(([k, v]) => `${k}=${v}`).join("  "));
  if (dec.pipeline) {
    const p = dec.pipeline;
    console.log(`  pipeline ${p.fps} fps for ${p.seconds}s, lowLatency=${p.lowLatency}, renderer ${p.backend} ${JSON.stringify(p.calibration || {})} (draw ${p.drawMs} ms, spacing ${p.minIntervalMs} ms):decoded ${p.decodedFps} fps, drawn ${p.drawnFps} fps, catchUpDropped ${p.catchUpDropped}, skippedDeltas ${p.skippedDeltas}, decoderErrors ${p.decoderErrors}, max feed lag ${p.maxFeedLagMs} ms, events ${JSON.stringify(p.events)}`);
    console.log(`  per second decoded: ${p.samples.map((s) => s.decoded).join(" ")}\n  per second queued/decodeQueue/buffer ms: ${p.samples.map((s) => `${s.queued}/${s.decodeQueue}/${s.bufferMs}`).join(" ")}`);
  }
  for (const r of dec.runs) console.log(`  optimizeForLatency=${r.optimizeForLatency} hw=${r.hardwareAcceleration}: ${r.fps} fps (${r.out}/${r.frames} out, first out ${r.firstOutMs} ms, ${r.totalMs} ms)${r.err ? " ERR " + r.err : ""}`);
} finally {
  procs.forEach((p) => p.kill());
  server.close();
  server.closeAllConnections?.();
  process.exit(0);
}
