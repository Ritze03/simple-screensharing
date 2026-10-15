// Outgoing streams. A Capture is one getDisplayMedia() pick; it goes live in one or more rooms ("destinations").
// The core shares one MediaStream into one room per startShare(), so fan-out is done here: every destination gets
// its own clones of the captured tracks and its own startShare() (own encoder, own key) on that room's RoomClient,
// which is what native does too (each extra room is a separate encoded + encrypted stream).
//
// Live edits: bitrate applies in place (ShareHandle.setBitrate). Framerate, codec and preset cannot be changed on a
// running WebCodecs share, so the destination is restarted (new share first, then the old one stops): viewers see
// the stream end and a new one start in its place.

import { S, invalidate, toast, persistSettings } from "./store.js";
import { t } from "./i18n.js";

let seq = 1;

/* ---------- own-tile live preview (task #78) ----------
 * The sharer already holds the captured MediaStream, so the own tile shows it in a muted <video>. `Why`
 * only while the page is visible AND focused: user request 2026-10-15 ("show a preview while tabbed into the
 * program, otherwise hide it"). Otherwise srcObject is detached so nothing renders or composites at all (the
 * track itself keeps feeding the encoder). `Why a persistent element per capture:` render() rebuilds the
 * whole tree on every state change; myCard() re-attaches the same element, so playback never restarts. */
export const previewActive = () => document.visibilityState === "visible" && (typeof document.hasFocus !== "function" || document.hasFocus());

function syncPreview(c) {
  const el = c.previewEl;
  if (!el) return;
  const on = previewActive() && c.video && c.video.readyState === "live";
  if (on) {
    if (!el.srcObject) el.srcObject = new MediaStream([c.video]); // video only: a preview never plays audio
    el.hidden = false;
    el.play().catch(() => {});
  } else {
    el.pause();
    el.srcObject = null;
    el.hidden = true;
  }
}

/** The capture's persistent preview element for myCard(), or null for a capture without video (audio-only). */
export function previewFor(c) {
  if (!c.video) return null;
  if (!c.previewEl) {
    const el = document.createElement("video");
    el.className = "live-preview";
    el.muted = true;
    el.autoplay = true;
    el.playsInline = true;
    el.disablePictureInPicture = true;
    el.setAttribute("aria-hidden", "true");
    c.previewEl = el;
  }
  syncPreview(c);
  return c.previewEl;
}

const syncAllPreviews = () => S.captures.forEach(syncPreview);
document.addEventListener("visibilitychange", syncAllPreviews);
window.addEventListener("focus", syncAllPreviews);
window.addEventListener("blur", syncAllPreviews);

export const capById = (id) => S.captures.find((c) => c.id === id) || null;
export const capInRoom = (roomId) => S.captures.find((c) => c.dests.has(roomId)) || null;
export const stagedFor = (roomId) => S.captures.find((c) => c.state !== "live" && c.stagedRoom === roomId) || null;
export const liveRoomIds = () => S.captures.flatMap((c) => [...c.dests.keys()]);
export const allDests = () => S.captures.flatMap((c) => [...c.dests.values()].map((d) => ({ cap: c, d })));

/** Bitrate a destination actually gets under the upload-bandwidth settings. */
export function allocated(d) {
  const bw = S.settings.bw;
  if (bw.mode === "per_stream") return d.kbps;
  const n = Math.max(1, allDests().length);
  return Math.max(bw.floor, Math.round(bw.total / n / 100) * 100);
}

/** Re-split the budget after anything that changes the number of streams or the budget itself. */
export function applyBandwidth() {
  for (const { d } of allDests()) {
    const k = allocated(d);
    if (d.share && d.appliedKbps !== k) {
      try {
        d.share.setBitrate(k);
        d.appliedKbps = k;
      } catch {}
    }
  }
}

function surfaceName(track) {
  const s = (track.getSettings && track.getSettings().displaySurface) || "";
  if (s === "monitor") return t("ui.capture.screen");
  if (s === "window") return t("ui.capture.window");
  if (s === "browser") return t("ui.capture.tab");
  return t("ui.capture.screen");
}

export function captureName(c) {
  return c.label;
}
export function captureRes(c) {
  const st = c.video && c.video.getSettings ? c.video.getSettings() : {};
  return st.width && st.height ? `${st.width}×${st.height}` : "";
}

/**
 * Pick a screen for `room` (must run inside a click handler: getDisplayMedia needs the user gesture).
 * Resolves the staged capture, or null when the user cancelled the picker.
 */
export async function stage(room) {
  const def = S.settings.stream;
  const c = {
    id: seq++, state: "picking", stagedRoom: room.id, stream: null, video: null, audio: null, label: "", audioMuted: false,
    staged: { fps: def.fps, codec: def.codec, preset: def.preset, kbps: def.kbps }, dests: new Map(), meter: null,
  };
  S.captures.push(c);
  invalidate();
  let stream;
  try {
    stream = await navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: { ideal: Math.max(60, def.fps) } },
      // Ask for audio so the browser shows its "share audio" option; what it offers depends on browser, OS and source.
      audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false },
      systemAudio: "include",
      selfBrowserSurface: "exclude",
    });
  } catch (e) {
    discard(c);
    if (e && e.name !== "NotAllowedError" && e.name !== "AbortError") toast(t("ui.toast.pick_failed", { error: e.message }), "error", { err: true });
    return null;
  }
  if (!S.captures.includes(c)) {
    stream.getTracks().forEach((tr) => tr.stop());
    return null;
  }
  adopt(c, stream);
  c.state = "ready";
  invalidate();
  return c;
}

function adopt(c, stream) {
  c.stream = stream;
  c.video = stream.getVideoTracks()[0] || null;
  c.audio = stream.getAudioTracks()[0] || null;
  c.label = c.video ? surfaceName(c.video) : t("ui.capture.screen");
  if (c.video) {
    c.video.addEventListener("ended", () => {
      // The browser's own "Stop sharing" (or the source window closed) ends every clone too.
      if (!S.captures.includes(c)) return;
      endCapture(c);
      toast(t("ui.toast.ended_by_browser"), "stop_circle");
    });
  }
  startMeter(c);
}

export function discard(c) {
  endCapture(c, { silent: true });
}

function endCapture(c) {
  if (c.previewEl) {
    c.previewEl.pause();
    c.previewEl.srcObject = null;
    c.previewEl.remove();
    c.previewEl = null;
  }
  for (const roomId of [...c.dests.keys()]) stopDest(c, roomId, { keepCapture: true });
  if (c.stream) c.stream.getTracks().forEach((tr) => tr.stop());
  stopMeter(c);
  S.captures = S.captures.filter((x) => x !== c);
  if (S.modal && S.modal.kind === "stream" && S.modal.cap === c.id) S.modal = null;
  applyBandwidth();
  invalidate();
}

async function startDest(c, room, cfg, { state = "starting" } = {}) {
  if (!room.client || room.state !== "joined") throw new Error(t("ui.err.not_connected"));
  const clones = [c.video.clone()];
  if (c.audio) clones.push(c.audio.clone());
  try {
    await clones[0].applyConstraints({ frameRate: { ideal: cfg.fps, max: cfg.fps } });
  } catch {}
  const d = { roomId: room.id, share: null, fps: cfg.fps, codec: cfg.codec, preset: cfg.preset, kbps: cfg.kbps, state, clones, appliedKbps: 0, stats: null, stopping: false };
  const prev = c.dests.get(room.id);
  if (!prev) c.dests.set(room.id, d);
  invalidate();
  const kbps = allocated(d);
  try {
    d.share = await room.client.startShare({
      stream: new MediaStream(clones), audio: !!c.audio, codec: cfg.codec, fps: cfg.fps, bitrateKbps: kbps, preset: cfg.preset, label: c.label,
    });
    d.appliedKbps = kbps;
    if (c.audioMuted) d.share.muteAudio(true);
  } catch (e) {
    clones.forEach((tr) => tr.stop());
    if (!prev) c.dests.delete(room.id);
    invalidate();
    throw e;
  }
  d.state = "live";
  c.dests.set(room.id, d);
  if (prev && prev !== d) retire(prev);
  applyBandwidth();
  invalidate();
  return d;
}

function retire(d) {
  d.stopping = true;
  try {
    d.share && d.share.stop();
  } catch {}
  d.clones.forEach((tr) => tr.stop());
}

/** Go live with a staged capture in its room. */
export async function goLive(c, room) {
  const cfg = { ...c.staged };
  S.settings.stream = { ...S.settings.stream, ...cfg };
  persistSettings();
  c.state = "live";
  c.stagedRoom = null;
  try {
    await startDest(c, room, cfg);
    toast(t("ui.toast.live_in", { room: room.name }), "cast");
  } catch (e) {
    toast(t("ui.toast.share_failed", { error: e.message }), "error", { err: true });
    if (!c.dests.size) endCapture(c);
  }
}

/** Also send this capture to another room (same settings as the first destination). */
export async function addDest(c, room) {
  const src = [...c.dests.values()][0] || c.staged;
  try {
    await startDest(c, room, { fps: src.fps, codec: src.codec, preset: src.preset, kbps: src.kbps });
    toast(t("ui.toast.live_in", { room: room.name }), "cast");
  } catch (e) {
    toast(t("ui.toast.share_failed", { error: e.message }), "error", { err: true });
  }
}

/** Move: start in `to` first, then stop in `from` (viewers in `to` never wait on the old one). */
export async function moveDest(c, from, to) {
  const d = c.dests.get(from.id);
  if (!d) return;
  d.state = "moving";
  invalidate();
  try {
    await startDest(c, to, { fps: d.fps, codec: d.codec, preset: d.preset, kbps: d.kbps });
    stopDest(c, from.id, { keepCapture: true });
    if (S.modal && S.modal.kind === "stream" && S.modal.room === from.id) S.modal.room = to.id;
    toast(t("ui.toast.moved", { room: to.name }), "swap_horiz");
  } catch (e) {
    d.state = "live";
    toast(t("ui.toast.share_failed", { error: e.message }), "error", { err: true });
  }
  invalidate();
}

export function stopDest(c, roomId, { keepCapture = false } = {}) {
  const d = c.dests.get(roomId);
  if (!d) return;
  c.dests.delete(roomId);
  retire(d);
  if (!keepCapture && !c.dests.size) endCapture(c);
  applyBandwidth();
  invalidate();
}

export function stopAll() {
  for (const c of [...S.captures]) endCapture(c);
}

/** Change fps / codec / preset of a live destination: restart it with the new settings. */
export async function editDest(c, room, patch) {
  const d = c.dests.get(room.id);
  if (!d) return;
  const cfg = { fps: d.fps, codec: d.codec, preset: d.preset, kbps: d.kbps, ...patch };
  if ("kbps" in patch && Object.keys(patch).length === 1) {
    d.kbps = patch.kbps;
    applyBandwidth();
    invalidate();
    return;
  }
  d.state = "restarting";
  d.applying = true;
  invalidate();
  try {
    const nd = await startDest(c, room, cfg, { state: "restarting" });
    nd.applying = false;
  } catch (e) {
    d.state = "live";
    d.applying = false;
    toast(t("ui.toast.share_failed", { error: e.message }), "error", { err: true });
  }
  invalidate();
}

export function setAudioMuted(c, muted) {
  c.audioMuted = muted;
  for (const d of c.dests.values()) if (d.share) d.share.muteAudio(muted);
  invalidate();
}

/** The room is going away (Leave / Forget): stop our streams there. */
export function stopDestsInRoom(roomId) {
  for (const c of [...S.captures]) {
    if (c.dests.has(roomId)) stopDest(c, roomId);
    if (c.stagedRoom === roomId && c.state !== "live") discard(c);
  }
}

/** Core `share` events of one room. */
export function onShareEvent(room, detail) {
  const share = detail.share;
  if (detail.state === "no-audio") {
    toast(t("ui.toast.no_audio"), "volume_off");
    return;
  }
  if (detail.state === "no-frames") {
    toast(t("ui.toast.no_frames"), "hourglass_empty");
    return;
  }
  if (detail.state === "stopped" && share) {
    // A share the core stopped by itself (encoder error, browser stop, disconnect) - drop its destination.
    for (const c of [...S.captures]) {
      const d = c.dests.get(room.id);
      if (d && d.share === share && !d.stopping) {
        c.dests.delete(room.id);
        d.clones.forEach((tr) => tr.stop());
        if (!c.dests.size && c.state === "live") endCapture(c);
      }
    }
    applyBandwidth();
    invalidate();
  }
}

export function onShareStats(room, sharing) {
  for (const c of S.captures) {
    const d = c.dests.get(room.id);
    if (!d || !d.share) continue;
    const st = sharing.find((x) => x.videoStreamId === d.share.videoStreamId);
    if (st) d.stats = st;
  }
}

/* ---- level meter of the captured audio (a WebAudio analyser on a clone of the audio track) ---- */
let actx = null;
function startMeter(c) {
  if (!c.audio) return;
  try {
    actx = actx || new AudioContext();
    const src = actx.createMediaStreamSource(new MediaStream([c.audio.clone()]));
    const an = actx.createAnalyser();
    an.fftSize = 512;
    src.connect(an);
    const buf = new Float32Array(an.fftSize);
    c.meter = {
      src, an,
      level() {
        an.getFloatTimeDomainData(buf);
        let sum = 0;
        for (let i = 0; i < buf.length; i++) sum += buf[i] * buf[i];
        return Math.min(1, Math.sqrt(sum / buf.length) * 4);
      },
    };
  } catch {
    c.meter = null;
  }
}
function stopMeter(c) {
  if (!c.meter) return;
  try {
    c.meter.src.mediaStream.getTracks().forEach((tr) => tr.stop());
    c.meter.src.disconnect();
  } catch {}
  c.meter = null;
}
export function captureLevel(c) {
  if (!c || !c.meter || c.audioMuted) return 0;
  if (actx && actx.state === "suspended") actx.resume().catch(() => {});
  return c.meter.level();
}
