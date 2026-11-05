// Drop watch (task 146): a viewer that watches every video stream of `owner` for `duration` seconds and writes down every event that can end
// or restart a watch: relay session closes (state events with their reason), streams appearing/vanishing, watch-ended, "media" events
// (need-keyframe reasons, decoder errors) and a 5 s stats line. It re-watches by stableKey like the UI's resume list.
// Query: link, name, owner (name filter, default any), duration (s), nack (0|1, page URL is read by the core), hw.
import { CanvasRenderer, RoomClient } from "../js/core/index.js";
import { log, params, postResult } from "./pagelog.js";

const q = (k, d) => (params.has(k) ? params.get(k) : d);
const link = q("link", "");
const name = q("name", "dropwatch");
const ownerFilter = q("owner", "");
const duration = Number(q("duration", 60));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const t0 = performance.now();
const ev = [];
const note = (type, d = {}) => {
  const e = { t: +((performance.now() - t0) / 1000).toFixed(2), type, ...d };
  ev.push(e);
  log(JSON.stringify(e));
};

const client = new RoomClient({ displayName: name });
let known = new Set();
const watches = new Map(); // key -> { stableKey, label, renderer, shown, sinceMs }
const wanted = new Map(); // stableKey -> label (re-watch after a restart)
const counters = { sessions: 0, watchEnded: 0, streamGone: 0, streamNew: 0, needKeyframe: {}, shownTotal: 0, rewatch: 0 };

for (const t of ["state", "error"]) client.addEventListener(t, (e) => {
  const d = { ...e.detail }; delete d.error;
  if (t === "state") { if (d.state === "reconnecting" || (d.state === "connected" && d.reconnected)) counters.sessions++; }
  note(t, { ...d, errorName: e.detail.error && e.detail.error.name, errorMsg: e.detail.error && String(e.detail.error.message || e.detail.error) });
});
client.addEventListener("media", (e) => {
  const d = e.detail;
  if (d.type === "need-keyframe") counters.needKeyframe[d.reason] = (counters.needKeyframe[d.reason] || 0) + 1;
  note("media", { key: d.key, mtype: d.type, reason: d.reason, err: d.error && String(d.error.message || d.error) });
});
client.addEventListener("watch-ended", (e) => { counters.watchEnded++; watches.delete(e.detail.key); note("watch-ended", e.detail); });
client.addEventListener("streams", (e) => {
  const now = new Set(e.detail.streams.map((s) => s.key));
  for (const k of known) if (!now.has(k)) { counters.streamGone++; note("stream-gone", { key: k }); }
  for (const s of e.detail.streams) if (!known.has(s.key)) { counters.streamNew++; note("stream-new", { key: s.key, kind: s.kind, label: s.label, w: s.width, h: s.height, owner: s.ownerName }); }
  known = now;
  rewatch(e.detail.streams);
});

function startWatch(s) {
  const canvas = document.createElement("canvas");
  canvas.style.cssText = "width:320px;height:180px;background:#000";
  document.body.appendChild(canvas);
  const renderer = new CanvasRenderer(canvas);
  const w = { stableKey: s.stableKey, label: s.label, renderer, shown: 0, start: performance.now(), firstFrame: null };
  watches.set(s.key, w);
  wanted.set(s.stableKey, s.label);
  client.watch(s.key, { audio: false, onFrame: (f) => { if (w.firstFrame === null) { w.firstFrame = performance.now(); note("first-frame", { key: s.key, afterMs: Math.round(w.firstFrame - w.start) }); } w.shown++; counters.shownTotal++; renderer.present(f); } });
  note("watch", { key: s.key, label: s.label });
}
function rewatch(streams) {
  for (const s of streams) {
    if (s.self || s.kind !== "video" || watches.has(s.key)) continue;
    if (ownerFilter && !(s.ownerName || "").includes(ownerFilter)) continue;
    if (wanted.has(s.stableKey)) counters.rewatch++;
    startWatch(s);
  }
}

async function main() {
  const result = { params: Object.fromEntries(params), userAgent: navigator.userAgent, events: ev, counters };
  try {
    await client.connect(link, name);
    note("connected", { selfId: client.selfId, maxDatagramSize: client.session.maxDatagramSize });
    let last = performance.now();
    while (performance.now() - t0 < duration * 1000) {
      await sleep(5000);
      const dt = (performance.now() - last) / 1000; last = performance.now();
      const st = {};
      for (const [k, w] of watches) {
        const s = client.watches.get(k) && client.watches.get(k).stats();
        if (s) st[w.label || k] = { dec: +s.decodedFps.toFixed(1), rx: +s.receivedFps.toFixed(1), kbps: Math.round(s.bitrateKbps), buf: Math.round(s.bufferMs), loss: s.lossEvents, dropped: s.droppedFrames, decErr: s.decoderErrors, skipped: s.skippedDeltas, catchUp: s.catchUpDropped };
      }
      note("stats", { dt: +dt.toFixed(1), watches: st, nack: client.nackStats && client.nackStats() });
    }
  } catch (e) {
    result.error = String(e && e.stack ? e.stack : e);
    note("fatal", { err: result.error });
  }
  try { await client.disconnect(); } catch {}
  document.getElementById("out").textContent = JSON.stringify(counters, null, 1);
  await postResult(result);
}
main();
