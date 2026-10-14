// Dev-only helpers to drive the REAL site UI (docs/index.html) through dev/cdp.mjs: add/create rooms, share a (synthetic or
// real) capture, watch a stream, read the numbers. Used by the I20 end-to-end runs (see dev/README.md "End to end").

import { launchBrowser } from "./cdp.mjs";
import { startServer } from "./headless.mjs";

export const BASE = "/simple-screensharing/";
export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** Serve docs/ at http://127.0.0.1:<port>/simple-screensharing/ (like GitHub Pages). */
export async function serveSite(port = 0) {
  const s = await startServer({ port });
  return { ...s, url: `http://127.0.0.1:${s.port}${BASE}`, close: () => { s.server.close(); s.server.closeAllConnections?.(); } };
}

/** Open the site in a fresh browser profile. `settings` seeds localStorage (name, lang, stream defaults). */
export async function openApp(siteUrl, { name = "Browser", lang = "en", settings = {}, headed = false, flags = [], viewport } = {}) {
  const b = await launchBrowser({ url: "about:blank", headed, flags, viewport });
  const seed = JSON.stringify({ name, lang, ...settings });
  await b.page.addInitScript(`try { if (!localStorage.getItem("sss.settings.v1")) localStorage.setItem("sss.settings.v1", ${JSON.stringify(seed)}); } catch {}`);
  await b.page.goto(siteUrl);
  await b.page.waitFor("window.__sss && document.querySelector('#app').children.length > 0", { what: "app boot" });
  return b;
}

const roomState = (i = 0) => `(__sss.S.rooms[${i}] && __sss.S.rooms[${i}].state)`;

export async function addRoomByLink(page, link, name = "") {
  await page.click("#btn-create-join");
  await page.waitFor("document.querySelector('#add-link')", { what: "join modal" });
  await page.type("#add-link", link);
  if (name) await page.type("#add-name", name);
  await page.click("#btn-join");
}

/** Create tab: mint a room for `server` (+ optional hash), return its link and open it. */
export async function createRoom(page, { name, server, hash }) {
  await page.click("#btn-create-join");
  await page.waitFor("document.querySelector('.seg')", { what: "add modal" });
  await page.click({ js: "document.querySelectorAll('.seg button')[1]" }); // second tab = create (language independent)
  await page.waitFor("document.querySelector('#add-cname')", { what: "create tab" });
  await page.type("#add-cname", name);
  await page.type("#add-server", server);
  if (hash) await page.type("#add-hash", hash);
  await page.click("#btn-create");
  const link = await page.waitFor("__sss.S.modal && __sss.S.modal.minted && __sss.S.modal.minted.link", { what: "minted link" });
  await page.click("#btn-open-room");
  return link;
}

export async function waitJoined(page, i = 0, timeout = 20000) {
  await page.waitFor(`${roomState(i)} === "joined"`, { timeout, what: `room ${i} joined` });
}

/** The invite link the UI would copy (the link the room was joined with, including the cert hash). */
export const inviteLink = (page, i = 0) => page.eval(`__sss.S.rooms[${i}].link`);

/**
 * Replace getDisplayMedia with the harness pattern source (noisy rectangles + frame/clock strip + optional 440 Hz tone),
 * so the real UI's share flow runs unattended. The pattern module is the one dev/ pages use.
 */
export async function stubCapture(page, { width = 1920, height = 1080, fps = 60, audio = true } = {}) {
  await page.eval(`(async () => {
    const { makePatternSource } = await import("${BASE}dev/pattern.js");
    const src = makePatternSource(${width}, ${height}, ${fps}, { audio: ${audio} });
    document.body.lastElementChild.style.cssText = "position:fixed;left:-9999px;width:8px;height:8px";
    window.__fakeSrc = src;
    navigator.mediaDevices.getDisplayMedia = async () => src.stream;
    return true;
  })()`);
}

/** Click through the real share flow: Share screen -> stream settings (codec, fps) -> Go live. */
export async function goLive(page, { codec = "h264", fps = 60, kbps = null } = {}) {
  await page.click("#btn-share");
  await page.waitFor("document.querySelector('#st-fps')", { what: "stream modal" });
  await page.eval(`(() => { const s = document.querySelector("#st-fps"); s.value = "${fps}"; s.dispatchEvent(new Event("change", { bubbles: true })); })()`);
  await sleep(150);
  const label = { h264: "H.264", h265: "H.265", av1: "AV1" }[codec];
  await page.click({ text: label, sel: ".seg button" });
  if (kbps) {
    await page.eval(`(() => { __sss.S.settings.bw.mode = "per_stream"; })()`);
  }
  await sleep(150);
  await page.waitFor("!document.querySelector('#btn-modal-go-live').disabled", { what: "go-live enabled" });
  await page.click("#btn-modal-go-live");
  await page.waitFor("__sss.S.captures.some((c) => c.state === 'live' && [...c.dests.values()].some((d) => d.state === 'live'))", { timeout: 20000, what: "live" });
}

/** Click Watch (the play_arrow icon button, any language) on the first video stream card in the selected room. */
export async function watchFirst(page) {
  await page.waitFor("[...document.querySelectorAll('button')].some((b) => /^play_arrow/.test(b.innerText.trim()))", { timeout: 30000, what: "a Watch button" });
  await page.click({ js: "[...document.querySelectorAll('button')].find((b) => /^play_arrow/.test(b.innerText.trim()))" });
  await page.waitFor("__sss.S.rooms.some((r) => r.watches.size)", { what: "watch started" });
}

/** Numbers of every current watch: core stats merged with UI state. */
export const watchStats = (page) =>
  page.eval(`(() => { const out = []; for (const r of __sss.S.rooms) for (const w of r.watches.values()) {
    const st = r.client ? r.client.watchStats(w.key) : null; out.push({ room: r.name, key: w.key, owner: w.ownerName, firstFrame: w.firstFrame, error: w.error, codec: w.codec, st }); } return out; })()`);

/** Average decoded fps / audio over `ms` from the cumulative counters of the first watch. */
export async function measureWatch(page, ms = 8000) {
  const read = () => page.eval(`(() => { const r = __sss.S.rooms.find((x) => x.watches.size); const w = [...r.watches.values()][0]; const s = r.client.watchStats(w.key) || {};
    const a = w.handle && w.handle.audioKey ? r.client.watchStats(w.handle.audioKey) : null; return { t: performance.now(), s, a: a || null }; })()`);
  const a = await read();
  let maxLevel = 0;
  const t0 = Date.now();
  while (Date.now() - t0 < ms) {
    await sleep(100);
    const x = await read();
    if (x.a && x.a.level > maxLevel) maxLevel = x.a.level;
  }
  const b = await read();
  const dt = (b.t - a.t) / 1000;
  const d = (k) => ((b.s[k] ?? 0) - (a.s[k] ?? 0)) / dt;
  return {
    seconds: +dt.toFixed(2), decodedFps: +d("decoded").toFixed(1), receivedFps: b.s.receivedFps, reportedDecodedFps: b.s.decodedFps, bitrateKbps: b.s.bitrateKbps,
    width: b.s.width, height: b.s.height, codec: b.s.codec, codecString: b.s.codecString, lossEvents: b.s.lossEvents, decoderErrors: b.s.decoderErrors, decryptFailures: b.s.decryptFailures,
    audio: b.a ? { packets: b.a.packets, decodedFrames: b.a.decodedFrames, level: b.a.level, maxLevel: +maxLevel.toFixed(4), underruns: b.a.underruns, packetsPerSec: +(((b.a.packets ?? 0) - ((a.a || {}).packets ?? 0)) / dt).toFixed(1) } : null,
  };
}

export const roomText = (page) => page.eval("document.body.innerText");
