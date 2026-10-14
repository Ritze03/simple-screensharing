// RoomClient behaviour scenarios against a REAL relay (dev/scenarios.html, driven by dev/scenarios.mjs).
// Each scenario uses several RoomClients inside one page (cheap, low-rate streams); throughput / codec measurements live
// in live.html. Query: relay=host:port, hash=<relay cert hash>, only=name,name (optional).

import { CanvasRenderer, ConnectError, JoinRejectedError, RoomClient, capabilities, createRoom, parseJoinLink } from "../js/core/index.js";
import { kvGet, kvSet, log, params, postResult } from "./pagelog.js";
import { makePatternSource } from "./pattern.js";

const RELAY = params.get("relay");
const HASH = params.get("hash");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const b64u = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const hashBytes = () => Uint8Array.from(atob(HASH.replace(/-/g, "+").replace(/_/g, "/") + "="), (c) => c.charCodeAt(0));
const newRoom = () => createRoom(RELAY, { certHash: hashBytes() });

class Assert extends Error {}
const ok = (c, m) => {
  if (!c) throw new Assert(m);
};
async function waitFor(cond, timeoutMs, what) {
  const t0 = performance.now();
  for (;;) {
    const v = await cond();
    if (v) return v;
    if (performance.now() - t0 > timeoutMs) throw new Assert(`timeout (${timeoutMs} ms) waiting for ${what}`);
    await sleep(50);
  }
}

const cleanups = [];
async function client(link, name, opts = {}) {
  const c = new RoomClient({ displayName: name, ...opts });
  c.log = [];
  for (const t of ["state", "roster", "streams", "preview", "stats", "error", "media", "share", "subscribers", "watch-ended"]) {
    c.addEventListener(t, (e) => c.log.push({ t, at: performance.now(), d: e.detail }));
  }
  await c.connect(link, name);
  cleanups.push(() => c.disconnect().catch(() => {}));
  return c;
}
const events = (c, t) => c.log.filter((e) => e.t === t);
const streamOf = (c, ownerClient, kind = "video") => [...c.streams.values()].find((s) => s.owner === ownerClient.selfId && s.kind === kind);

const scenarios = [];
const scenario = (name, fn, { timeoutMs = 45_000 } = {}) => scenarios.push({ name, fn, timeoutMs });

// ---------------------------------------------------------------------------------------------------------------

scenario("capabilities() and link handling", async () => {
  const caps = capabilities();
  ok(caps.canWatch && caps.canShare, `headless Chromium should be fully capable: ${JSON.stringify(caps)}`);
  const room = newRoom();
  ok(parseJoinLink(room.link).certHash.length === 32, "createRoom keeps the cert hash");
  return { caps };
});

scenario("stale cert hash -> ConnectError with 'fresh invite link' hint", async () => {
  const room = createRoom(RELAY, { certHash: crypto.getRandomValues(new Uint8Array(32)) });
  const c = new RoomClient();
  const t0 = performance.now();
  let err;
  try {
    await c.connect(room.link, "x");
  } catch (e) {
    err = e;
  }
  ok(err instanceof ConnectError, `expected ConnectError, got ${err && err.name}: ${err && err.message}`);
  ok(err.mayBeExpiredLink === true && err.code === "handshake-failed", "flagged as possibly-expired link");
  ok(/fresh invite link/i.test(err.message), `message should tell the user to ask for a fresh link: ${err.message}`);
  ok(c.state === "closed", "client ends in state closed");
  // a link with no hash against a self-signed relay is also refused, with a different hint
  const noHash = createRoom(RELAY);
  const c2 = new RoomClient();
  let err2;
  try {
    await c2.connect(noHash.link, "x");
  } catch (e) {
    err2 = e;
  }
  ok(err2 instanceof ConnectError && !err2.mayBeExpiredLink && /certificate hash/.test(err2.message), `no-hash: ${err2 && err2.message}`);
  return { ms: Math.round(performance.now() - t0), message: err.message };
});

scenario("unreachable relay -> ConnectError (not a hang)", async () => {
  const room = createRoom("127.0.0.1:9", { certHash: hashBytes() });
  const c = new RoomClient();
  const t0 = performance.now();
  let err;
  try {
    await c.connect(room.link, "x");
  } catch (e) {
    err = e;
  }
  ok(err instanceof ConnectError, `got ${err && err.name}`);
  return { ms: Math.round(performance.now() - t0) };
});

scenario("malformed link -> JoinLinkError before any network", async () => {
  const c = new RoomClient();
  let err;
  try {
    await c.connect("not-a-link", "x");
  } catch (e) {
    err = e;
  }
  ok(err && err.name === "JoinLinkError", `got ${err && err.name}`);
  return { message: err.message };
});

scenario("display names are clamped to 64 bytes like the relay does", async () => {
  const room = newRoom();
  const a = await client(room.link, "é".repeat(60), {});
  const b = await client(room.link, "b");
  await waitFor(() => b.members.size === 2, 5000, "roster of 2");
  const aName = b.members.get(a.selfId).name;
  ok(new TextEncoder().encode(aName).length <= 64, `name not clamped: ${aName.length} chars`);
  return { clampedChars: aName.length };
});

scenario("room is full at 8 members (JoinRejectedError RoomFull), a seat frees up when someone leaves", async () => {
  const room = newRoom();
  const cs = [];
  for (let i = 0; i < 8; i++) cs.push(await client(room.link, `m${i}`));
  await waitFor(() => cs[0].members.size === 8, 5000, "8 members");
  const ninth = new RoomClient();
  let err;
  try {
    await ninth.connect(room.link, "m8");
  } catch (e) {
    err = e;
  }
  // The relay closes the connection right after writing JoinRejected and the frame can be lost with it
  // (relay race, reported in README "Known relay issues"); the client then reports "join-no-reply".
  const viaRejection = err instanceof JoinRejectedError && err.reason === "RoomFull";
  const viaClose = err instanceof ConnectError && err.code === "join-no-reply";
  ok(viaRejection || viaClose, `got ${err && err.name} ${err && (err.reason || err.code)}`);
  ok(/full/i.test(err.message), "human readable message");
  await cs[7].disconnect();
  const again = await client(room.link, "m8-retry");
  ok(again.state === "connected", "joins once a seat is free");
  return { rejectedVia: viaRejection ? "JoinRejected{RoomFull}" : "connection closed before the frame arrived (relay race)", closeInfo: err.closeInfo || null, cause: err.cause && String(err.cause.message || err.cause) };
});

scenario("roster + streams events as peers join, share, stop, leave", async () => {
  const room = newRoom();
  const a = await client(room.link, "alice");
  const b = await client(room.link, "bob");
  await waitFor(() => events(a, "roster").some((e) => e.d.members.length === 2), 5000, "alice sees bob");
  const src = makePatternSource(320, 180, 15);
  const share = await a.startShare({ stream: src.stream, fps: 15, bitrateKbps: 500, label: "Test screen" });
  const st = await waitFor(() => streamOf(b, a), 5000, "bob sees alice's stream");
  ok(st.kind === "video" && st.label === "Test screen" && st.width === 320 && st.height === 180, `stream info: ${JSON.stringify({ ...st, ownerId: undefined })}`);
  const ev = events(b, "streams").at(-1).d.streams.find((s) => s.key === st.key);
  ok(ev && ev.ownerName === "alice", "streams event carries ownerName");
  ok(b.members.get(a.selfId).streamCount >= 1, "member.streamCount reflects sharing");
  share.stop();
  await waitFor(() => !streamOf(b, a), 5000, "stream disappears after stopShare");
  await b.disconnect();
  await waitFor(() => a.members.size === 1, 5000, "bob left the roster");
  src.stop();
});

scenario("encoder idles with no viewers; subscribe resumes it with a keyframe; unsubscribe idles again", async () => {
  const room = newRoom();
  const a = await client(room.link, "sharer");
  const b = await client(room.link, "viewer1");
  const src = makePatternSource(640, 360, 30);
  const share = await a.startShare({ stream: src.stream, fps: 30, bitrateKbps: 1000 });
  await sleep(1500);
  let s = share.stats();
  ok(s.encoded === 0 && s.skippedIdle > 10, `no viewer: encoder must idle (encoded=${s.encoded}, skippedIdle=${s.skippedIdle})`);
  ok(s.subscribers === 0, "subscribers=0");
  const st = await waitFor(() => streamOf(b, a), 5000, "stream visible");
  const canvas = document.body.appendChild(document.createElement("canvas"));
  const w1 = b.watch(st.key, { canvas });
  await waitFor(() => share.subscribers === 1, 5000, "sender told SubscriberCount=1");
  await waitFor(() => (b.watchStats(st.key) || {}).decoded > 5, 8000, "viewer decodes frames");
  s = share.stats();
  ok(s.keyframes >= 1 && s.encoded > 5, `resumed: ${JSON.stringify({ k: s.keyframes, e: s.encoded })}`);
  const firstStats = b.watchStats(st.key);
  ok(firstStats.codec === "h264" && firstStats.width === 640, `viewer sniffed ${firstStats.codec} ${firstStats.width}x${firstStats.height}`);
  // a second viewer joining mid-GOP must trigger a fresh keyframe (KeyframeRequest honoured)
  const c = await client(room.link, "viewer2");
  const st2 = await waitFor(() => streamOf(c, a), 5000, "viewer2 sees stream");
  const keysBefore = share.stats().keyframes;
  c.watch(st2.key);
  await waitFor(() => share.subscribers === 2, 5000, "SubscriberCount=2");
  await waitFor(() => share.stats().keyframes > keysBefore, 3000, "KeyframeRequest produced an extra keyframe");
  await waitFor(() => (c.watchStats(st2.key) || {}).decoded > 3, 5000, "viewer2 decodes from the forced keyframe");
  // everyone leaves -> idle again
  w1.stop();
  c.unwatch(st2.key);
  await waitFor(() => share.subscribers === 0, 5000, "SubscriberCount back to 0");
  await sleep(600);
  const e0 = share.stats().encoded;
  await sleep(800);
  ok(share.stats().encoded === e0, "encoder idle again");
  src.stop();
  return { keyframes: share.stats().keyframes };
});

scenario("static screen (no new frames at all): a new viewer still gets a keyframe within ~1 s", async () => {
  const room = newRoom();
  const a = await client(room.link, "sharer");
  const b = await client(room.link, "viewer");
  const src = makePatternSource(640, 360, 30);
  const share = await a.startShare({ stream: src.stream, fps: 30, bitrateKbps: 1000 });
  await sleep(500);
  src.pause(); // the capture source goes quiet, exactly what a damage-driven screen capture does when nothing changes
  await sleep(800);
  const st = await waitFor(() => streamOf(b, a), 5000, "stream");
  const t0 = performance.now();
  b.watch(st.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await waitFor(() => (b.watchStats(st.key) || {}).decoded >= 1, 4000, "first frame from a source that produces no frames");
  const ttfMs = Math.round(performance.now() - t0);
  ok(share.stats().keepaliveKeyframes >= 1, "keyframe came from re-encoding the last frame");
  // and a viewer who joins even later still gets one
  const c = await client(room.link, "viewer2");
  const st2 = await waitFor(() => streamOf(c, a), 5000, "stream for viewer2");
  c.watch(st2.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await waitFor(() => (c.watchStats(st2.key) || {}).decoded >= 1, 4000, "viewer2 first frame");
  src.stop();
  return { timeToFirstFrameMs: ttfMs, keepaliveKeyframes: share.stats().keepaliveKeyframes };
});

scenario("audio follows video; volume 0-200 % and mute", async () => {
  const room = newRoom();
  const a = await client(room.link, "sharer");
  const b = await client(room.link, "viewer");
  const src = makePatternSource(320, 180, 15, { audio: true });
  const share = await a.startShare({ stream: src.stream, fps: 15, bitrateKbps: 500, audio: true });
  ok(share.audioStreamId !== null, "audio stream announced");
  const v = await waitFor(() => streamOf(b, a), 5000, "video stream");
  const au = await waitFor(() => streamOf(b, a, "audio"), 5000, "audio stream");
  const h = b.watch(v.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  ok(h.audioKey === au.key, `watching video auto-subscribes sibling audio (${h.audioKey})`);
  await waitFor(() => (b.watchStats(au.key) || {}).decodedFrames > 20, 8000, "audio decoded");
  const as = b.watchStats(au.key);
  ok(as.errors === 0 && as.decryptFailures === 0, "no audio errors");
  ok(as.level > 0.05 || (await waitFor(() => (b.watchStats(au.key) || {}).level > 0.05, 3000, "tone level")), "audible signal");
  ok(share.audioSubscribers === 1, "sender sees audio subscriber");
  b.setVolume(v.key, 0.5);
  ok(b.watchStats(au.key).volume === 0.5, "volume applies via the video key");
  b.setVolume(au.key, 3);
  ok(b.watchStats(au.key).volume === 2, "clamped to 200 %");
  b.setMuted(v.key, true);
  ok(b.watchStats(au.key).muted === true, "mute");
  b.setVolume(au.key, 1.5);
  const g = b.subs.get(au.key).impl.gain.gain;
  await sleep(300);
  ok(g.value === 0, `gain node is silent while muted (${g.value})`);
  b.setMuted(v.key, false);
  await sleep(300);
  ok(Math.abs(g.value - 1.5) < 0.01, `gain node back at 150 % (${g.value})`);
  // sender-side mute keeps the stream alive (like native: volume=0), packets keep flowing
  share.muteAudio(true);
  const p0 = b.watchStats(au.key).packets;
  await sleep(600);
  ok(b.watchStats(au.key).packets > p0, "muted share still sends (silent) packets");
  share.muteAudio(false);
  b.unwatch(v.key);
  ok(!b.subs.has(au.key) && !b.subs.has(v.key), "unwatching video drops its audio subscription too");
  await waitFor(() => share.audioSubscribers === 0 && share.subscribers === 0, 5000, "sender sees both unsubscribes");
  src.stop();
});

scenario("previews: sender produces ~immediately, relay replays the cached one to late joiners", async () => {
  const room = newRoom();
  const a = await client(room.link, "sharer");
  const b = await client(room.link, "early");
  const src = makePatternSource(1280, 720, 10);
  await a.startShare({ stream: src.stream, fps: 10, bitrateKbps: 500 });
  const e = await waitFor(() => events(b, "preview")[0], 8000, "early viewer gets a preview");
  const bmp = await createImageBitmap(e.d.blob);
  ok(e.d.blob.type === "image/jpeg" && bmp.width === 320 && bmp.height === 180, `preview ${bmp.width}x${bmp.height} ${e.d.blob.type}`);
  const t0 = performance.now();
  const late = await client(room.link, "late");
  const l = await waitFor(() => events(late, "preview")[0], 5000, "late joiner gets the cached preview");
  ok(l.at - t0 < 3000, "replayed promptly on join");
  ok(late.previews.size === 1, "client.previews map holds it");
  const own = events(a, "preview").length;
  ok(own === 0, "sender does not get its own previews back as events");
  src.stop();
  return { bytes: e.d.blob.size, replayedAfterMs: Math.round(l.at - t0) };
});

scenario("peer with the wrong room key: nothing decodes, nothing crashes", async () => {
  const room = newRoom();
  const a = await client(room.link, "sharer");
  const wrong = parseJoinLink(room.link);
  wrong.roomKey = crypto.getRandomValues(new Uint8Array(32));
  const mallory = await client(wrong, "mallory");
  const src = makePatternSource(320, 180, 15);
  await a.startShare({ stream: src.stream, fps: 15, bitrateKbps: 500 });
  const st = await waitFor(() => streamOf(mallory, a), 5000, "mallory sees the (cleartext) stream announcement");
  mallory.watch(st.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await sleep(4000);
  const s = mallory.watchStats(st.key);
  ok(s.decoded === 0 && s.rxFrames === 0, `no frame may decrypt (decoded=${s.decoded})`);
  ok(s.decryptFailures > 10, `decrypt failures counted (${s.decryptFailures})`);
  ok(events(mallory, "preview").length === 0, "previews with the wrong key are dropped");
  src.stop();
  return { decryptFailures: s.decryptFailures };
});

scenario("packet loss: viewer waits for a keyframe, asks for one (resubscribe), and recovers", async () => {
  const room = newRoom();
  const a = await client(room.link, "sharer");
  const b = await client(room.link, "viewer");
  const src = makePatternSource(640, 360, 30);
  const share = await a.startShare({ stream: src.stream, fps: 30, bitrateKbps: 2000, gopMs: 30_000 });
  const st = await waitFor(() => streamOf(b, a), 5000, "stream");
  b.watch(st.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await waitFor(() => (b.watchStats(st.key) || {}).decoded > 20, 8000, "decoding");
  // drop 25 % of incoming datagrams for 2 s
  const sess = b.session;
  const orig = sess.onDatagram;
  sess.onDatagram = (d) => {
    if (Math.random() > 0.25) orig(d);
  };
  await sleep(2000);
  sess.onDatagram = orig;
  const lossy = b.watchStats(st.key);
  ok(lossy.lossEvents > 0, `loss events recorded (${lossy.lossEvents})`);
  ok(events(b, "media").some((e) => e.d.type === "need-keyframe"), "need-keyframe raised");
  // With a 30 s GOP, only the resubscribe-triggered keyframe can heal the picture within a few seconds.
  const keysBefore = share.stats().keyframes;
  const decodedBefore = b.watchStats(st.key).decoded;
  await waitFor(() => !b.watchStats(st.key).needKeyframe && b.watchStats(st.key).decoded > decodedBefore + 15, 6000, "decoder resumed after a fresh keyframe");
  ok(share.stats().keyframes > keysBefore, "sender produced an extra keyframe for the resubscribe");
  ok(b.watchStats(st.key).decoderErrors === 0, "no fatal decoder errors");
  src.stop();
  return { lossEvents: lossy.lossEvents, skippedDeltas: b.watchStats(st.key).skippedDeltas };
});

scenario("stream stopping under a viewer cleans up (watch-ended, no leaked subscriptions)", async () => {
  const room = newRoom();
  const a = await client(room.link, "sharer");
  const b = await client(room.link, "viewer");
  const src = makePatternSource(320, 180, 15, { audio: true });
  const share = await a.startShare({ stream: src.stream, fps: 15, bitrateKbps: 500, audio: true });
  const st = await waitFor(() => streamOf(b, a), 5000, "stream");
  await waitFor(() => streamOf(b, a, "audio"), 5000, "audio stream");
  b.watch(st.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await waitFor(() => (b.watchStats(st.key) || {}).decoded > 3, 8000, "decoding");
  share.stop();
  await waitFor(() => events(b, "watch-ended").length === 1, 5000, "watch-ended event");
  ok(b.subs.size === 0 && b.watches.size === 0 && b.subRefs.size === 0, `no leaked state: subs=${b.subs.size} watches=${b.watches.size} refs=${b.subRefs.size}`);
  src.stop();
});

scenario("two rooms at once from one page are isolated", async () => {
  const r1 = newRoom();
  const r2 = newRoom();
  const a1 = await client(r1.link, "a1");
  const b1 = await client(r1.link, "b1");
  const x2 = await client(r2.link, "x2");
  const src = makePatternSource(320, 180, 15);
  await a1.startShare({ stream: src.stream, fps: 15, bitrateKbps: 500 });
  const st = await waitFor(() => streamOf(b1, a1), 5000, "b1 sees a1's stream");
  b1.watch(st.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await waitFor(() => (b1.watchStats(st.key) || {}).decoded > 3, 8000, "b1 decodes");
  ok(x2.streams.size === 0 && x2.members.size === 1, "room 2 sees nothing of room 1");
  ok(a1.selfId !== x2.selfId, "separate connections have separate ConnIds");
  src.stop();
});

scenario("nonce counter exhaustion: connection restarts with a fresh ConnId and the share continues", async () => {
  const room = newRoom();
  const a = await client(room.link, "sharer");
  const b = await client(room.link, "viewer");
  const src = makePatternSource(320, 180, 20);
  const share = await a.startShare({ stream: src.stream, fps: 20, bitrateKbps: 500 });
  const st = await waitFor(() => streamOf(b, a), 5000, "stream");
  b.watch(st.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await waitFor(() => (b.watchStats(st.key) || {}).decoded > 5, 8000, "decoding");
  const oldId = a.selfId;
  // jump the shared counter to 12 values before the end of the u32 space
  a.session.nonce.next = 0xfffffff3;
  await waitFor(() => events(a, "error").some((e) => e.d.code === "nonce-exhausted"), 5000, "nonce-exhausted error");
  await waitFor(() => events(a, "state").some((e) => e.d.state === "reconnecting"), 5000, "reconnecting");
  await waitFor(() => a.state === "connected" && a.selfId !== oldId, 15000, "reconnected with a new ConnId");
  ok(share.ended === false && a.shares.length === 1, "share survived");
  const st2 = await waitFor(() => streamOf(b, a), 5000, "new announcement visible");
  ok(st2.key !== st.key, "new owner id");
  b.watch(st2.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await waitFor(() => (b.watchStats(st2.key) || {}).decoded > 5, 10000, "viewer decodes the continued share");
  src.stop();
  return { oldId, newId: a.selfId };
});

scenario("relay restart: clients reconnect on their own, shares are re-announced, viewers can re-watch", async () => {
  const room = newRoom();
  const a = await client(room.link, "sharer");
  const b = await client(room.link, "viewer");
  const src = makePatternSource(320, 180, 15);
  await a.startShare({ stream: src.stream, fps: 15, bitrateKbps: 500 });
  const st = await waitFor(() => streamOf(b, a), 5000, "stream");
  b.watch(st.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await waitFor(() => (b.watchStats(st.key) || {}).decoded > 5, 8000, "decoding");
  const oldA = a.selfId;
  await kvSet("restart-relay", String(Date.now()));
  await kvGet("relay-restarted", { timeoutMs: 40_000 });
  // The relay process is gone without any CONNECTION_CLOSE reaching the browser, so detection takes as long as the QUIC
  // idle timeout (measured below), not milliseconds.
  const t0 = performance.now();
  await waitFor(() => events(a, "state").some((e) => e.d.state === "reconnecting") && events(b, "state").some((e) => e.d.state === "reconnecting"), 90_000, "both clients noticed the outage");
  const detectedAfterMs = Math.round(performance.now() - t0);
  await waitFor(() => a.state === "connected" && b.state === "connected", 40_000, "both reconnected");
  ok(a.selfId !== oldA, "fresh ConnId after reconnect");
  ok(events(b, "watch-ended").length >= 1, "the old stream key ended for the viewer");
  const st2 = await waitFor(() => streamOf(b, a), 10_000, "sharer re-announced");
  b.watch(st2.key, { canvas: document.body.appendChild(document.createElement("canvas")) });
  await waitFor(() => (b.watchStats(st2.key) || {}).decoded > 5, 10_000, "viewer decodes after the restart");
  src.stop();
  return { outageDetectedAfterMs: detectedAfterMs, reconnectEvents: events(a, "state").map((e) => e.d.state) };
}, { timeoutMs: 170_000 });

// ---------------------------------------------------------------------------------------------------------------

async function main() {
  const only = params.get("only") ? params.get("only").split(",") : null;
  const results = [];
  for (const s of scenarios) {
    if (only && !only.some((o) => s.name.includes(o))) continue;
    const t0 = performance.now();
    let r;
    try {
      const detail = await Promise.race([s.fn(), new Promise((_, rej) => setTimeout(() => rej(new Assert(`scenario timeout ${s.timeoutMs} ms`)), s.timeoutMs))]);
      r = { name: s.name, ok: true, ms: Math.round(performance.now() - t0), detail };
    } catch (e) {
      r = { name: s.name, ok: false, ms: Math.round(performance.now() - t0), error: String(e && e.message ? e.message : e), stack: e && e.stack };
    }
    while (cleanups.length) await cleanups.pop()();
    results.push(r);
    log(`${r.ok ? "PASS" : "FAIL"} ${r.name} (${r.ms} ms)${r.ok ? "" : " :: " + r.error}${r.detail ? " " + JSON.stringify(r.detail) : ""}`);
    document.getElementById("out").textContent = results.map((x) => `${x.ok ? "PASS" : "FAIL"} ${x.name}${x.ok ? "" : "\n     " + x.error}`).join("\n");
    await sleep(300);
  }
  const failed = results.filter((r) => !r.ok);
  await postResult({ ok: failed.length === 0, passed: results.length - failed.length, total: results.length, results });
}

main();
