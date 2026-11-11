// Outgoing streams. A Capture is one getDisplayMedia() pick (`kind: "screen"`), one getUserMedia() camera (`kind: "webcam"`, task I10) or
// an audio-only pick (`kind: "audio"`, I10: getDisplayMedia with audio, the video track stopped); it goes live in one or more rooms ("destinations").
// The core shares one MediaStream into one room per startShare(), so fan-out is done here: every destination gets
// its own clones of the captured tracks and its own startShare() (own encoder, own key) on that room's RoomClient,
// which is what native does too (each extra room is a separate encoded + encrypted stream).
//
// Live edits: bitrate applies in place (ShareHandle.setBitrate). Framerate, codec and preset cannot be changed on a
// running WebCodecs share, so the destination is restarted (new share first, then the old one stops): viewers see
// the stream end and a new one start in its place.

import { WEBCAM_LABEL } from "../core/index.js";
import { previewAllowed } from "./stage-state.js";
import { S, invalidate, toast, persistSettings, encoderOf } from "./store.js";
import { t } from "./i18n.js";

let seq = 1;

/* ---------- own-tile live preview (task #78) ----------
 * The sharer already holds the captured MediaStream, so the own tile shows it in a muted <video>. `Why`
 * only while the page is visible AND focused: user request 2026-10-15 ("show a preview while tabbed into the
 * program, otherwise hide it"). Otherwise srcObject is detached so nothing renders or composites at all (the
 * track itself keeps feeding the encoder). Row 174 (user request 2026-11-11, native row 173): the Layout menu's "Hide when
 * unfocused" (on by default) can be off, and the own webcam is never hidden by focus loss; a hidden tab always detaches.  `Why a persistent element per capture:` render() rebuilds the
 * whole tree on every state change; myCard() re-attaches the same element, so playback never restarts. */
export const previewActive = (c) => previewAllowed({ visible: document.visibilityState === "visible", focused: typeof document.hasFocus !== "function" || document.hasFocus(), hideUnfocused: S.stage.hideUnfocused, webcam: c.kind === "webcam" });

function syncPreview(c) {
  const el = c.previewEl;
  if (!el) return;
  const on = previewActive(c) && c.video && c.video.readyState === "live";
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
/** The kinds of capture: a screen (the original, `kind` unset on none), a webcam, an audio-only share. */
export const isScreen = (c) => c.kind === "screen";
export const screens = () => S.captures.filter(isScreen);
/** Your screen share live in `roomId` (the Share button's), the webcam / the audio-only share live there. */
export const capInRoom = (roomId) => screens().find((c) => c.dests.has(roomId)) || null;
export const webcamInRoom = (roomId) => S.captures.find((c) => c.kind === "webcam" && c.dests.has(roomId)) || null;
export const audioInRoom = (roomId) => S.captures.find((c) => c.kind === "audio" && c.dests.has(roomId)) || null;
/** The webcam capture whatever room it is in (one webcam at a time, like native: it moves between rooms) or null; `starting` ones count. */
export const webcamCap = () => S.captures.find((c) => c.kind === "webcam") || null;
/** Every capture with a destination in `roomId`, all kinds (what leaving the room stops). */
export const capsInRoom = (roomId) => S.captures.filter((c) => c.dests.has(roomId));
export const stagedFor = (roomId) => screens().find((c) => c.state !== "live" && c.stagedRoom === roomId) || null;
/** Rooms your SCREEN is live in ("Live in N rooms": native `MyPhase` is about screens too). */
export const liveRoomIds = () => screens().flatMap((c) => [...c.dests.keys()]);
/** Every video destination (screen and webcam): what the upload budget is split between. An audio share has no bitrate to split. */
export const allDests = () => S.captures.filter((c) => c.kind !== "audio").flatMap((c) => [...c.dests.values()].map((d) => ({ cap: c, d })));
/** Every destination of every kind (the Outgoing menu lists them all). */
export const allSends = () => S.captures.flatMap((c) => [...c.dests.values()].map((d) => ({ cap: c, d })));

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
/** The label an audio-only share carries on the wire: where its audio comes from, as the browser picked it. */
function audioName(track, videoSurface) {
  return videoSurface === "browser" ? t("ui.capture.audio_tab") : t("ui.capture.audio_system");
}
export function captureRes(c) {
  const st = c.video && c.video.getSettings ? c.video.getSettings() : {};
  return st.width && st.height ? `${st.width}×${st.height}` : "";
}

const pickDisplay = (fps) =>
  navigator.mediaDevices.getDisplayMedia({
    video: { frameRate: { ideal: Math.max(60, fps) } },
    // Ask for audio so the browser shows its "share audio" option; what it offers depends on browser, OS and source.
    audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false },
    systemAudio: "include",
    selfBrowserSurface: "exclude",
  });

/** The settings one destination is started with (fps, codec, bitrate and the encoder: preset name, mode and keyframe interval, see `encoderOf`). */
const encCfg = (d) => ({ fps: d.fps, codec: d.codec, preset: d.preset, ...encoderOf(d), kbps: d.kbps });

/**
 * Pick a screen for `room` (must run inside a click handler: getDisplayMedia needs the user gesture).
 * Resolves the staged capture, or null when the user cancelled the picker.
 */
export async function stage(room) {
  const def = S.settings.stream;
  const c = {
    id: seq++, kind: "screen", state: "picking", stagedRoom: room.id, stream: null, video: null, audio: null, label: "", audioMuted: false,
    staged: { ...encCfg(def) }, dests: new Map(), meter: null,
  };
  S.captures.push(c);
  invalidate();
  let stream;
  try {
    stream = await pickDisplay(def.fps);
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
  c.label = c.kind === "webcam" ? WEBCAM_LABEL : c.kind === "audio" ? audioName(c.audio, c.surface) : c.video ? surfaceName(c.video) : t("ui.capture.screen");
  const track = c.video || c.audio;
  if (track) {
    track.addEventListener("ended", () => {
      // The browser's own "Stop sharing" (or the source window closed, the camera pulled out) ends every clone too. Not a track that
      // "Change source" replaced.
      if (!S.captures.includes(c) || (c.video || c.audio) !== track) return;
      endCapture(c);
      if (c.kind === "webcam") toast(t("errors.webcam_lost", { reason: t("ui.webcam.lost_reason") }), "videocam_off", { err: true });
      else toast(t("ui.toast.ended_by_browser"), "stop_circle");
    });
  }
  startMeter(c);
}

/**
 * "Change source" (the Source card's Change button, staged and live; needs the click's user gesture): pick another screen, window or
 * tab and carry on with it. A staged capture just swaps its tracks; a live one restarts every destination on the new tracks (the new
 * share starts first, then the old one retires, like a live FPS change), so viewers see the stream end and start again. Cancelling the
 * picker changes nothing. Resolves whether the source changed.
 */
export async function changeSource(c) {
  let stream;
  try {
    stream = await pickDisplay([...c.dests.values()][0]?.fps ?? c.staged.fps);
  } catch (e) {
    if (e && e.name !== "NotAllowedError" && e.name !== "AbortError") toast(t("ui.toast.pick_failed", { error: e.message }), "error", { err: true });
    return false;
  }
  if (!S.captures.includes(c)) {
    stream.getTracks().forEach((tr) => tr.stop());
    return false;
  }
  await swapStream(c, stream);
  return true;
}

/** Carry on with `stream` as the capture's source: a staged capture just swaps its tracks; a live one restarts every destination on the new
 *  tracks (the new share starts first, then the old one retires), `cfg` (optional) overrides what the restarted destinations send. */
async function swapStream(c, stream, cfg = {}) {
  const old = c.stream;
  stopMeter(c);
  adopt(c, stream);
  if (c.previewEl) {
    c.previewEl.srcObject = null;
    syncPreview(c);
  }
  invalidate();
  for (const [roomId, d] of [...c.dests]) {
    const room = S.rooms.find((r) => r.id === roomId);
    if (!room) continue;
    d.state = "restarting";
    invalidate();
    try {
      await startDest(c, room, { ...encCfg(d), ...cfg }, { state: "restarting" });
    } catch (e) {
      d.state = "live";
      toast(t("ui.toast.share_failed", { error: e.message }), "error", { err: true });
    }
  }
  if (old) old.getTracks().forEach((tr) => tr.stop());
  invalidate();
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
  const audioOnly = c.kind === "audio";
  const clones = audioOnly ? [] : [c.video.clone()];
  if (c.audio) clones.push(c.audio.clone());
  if (!audioOnly && c.kind === "screen") {
    try {
      await clones[0].applyConstraints({ frameRate: { ideal: cfg.fps, max: cfg.fps } });
    } catch {}
  }
  const d = { roomId: room.id, share: null, ...encCfg(cfg), state, clones, appliedKbps: 0, stats: null, stopping: false };
  const prev = c.dests.get(room.id);
  if (!prev) c.dests.set(room.id, d);
  invalidate();
  const kbps = allocated(d);
  try {
    d.share = await room.client.startShare({
      stream: new MediaStream(clones), audio: !!c.audio, codec: cfg.codec, fps: cfg.fps, bitrateKbps: kbps, preset: cfg.preset, latencyMode: cfg.latencyMode, gopMs: cfg.gopMs, label: c.label,
      // a webcam's encoder is fixed (plan D8): Performance, a keyframe every 2000 ms; an audio share has no video encoder at all
      ...(c.kind === "webcam" ? { gopMs: WEBCAM_GOP_MS, latencyMode: "realtime" } : {}),
      ...(audioOnly ? { audioOnly: true } : {}),
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
    await startDest(c, room, encCfg(src));
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
    await startDest(c, to, encCfg(d));
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

/** Change fps / codec / preset / encoding mode / keyframe interval of a live destination: restart it with the new settings. */
export async function editDest(c, room, patch) {
  const d = c.dests.get(room.id);
  if (!d) return;
  const cfg = { ...encCfg(d), ...patch };
  // What the user last chose is what the next share starts with, also on a live one (2026-11-05). `cfg` is the user's edit, so a
  // codec the browser substituted on its own is not stored.
  if (c.kind === "screen") {
    S.settings.stream = { ...S.settings.stream, ...encCfg(cfg) };
    persistSettings();
  }
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
    const st = sharing.find((x) => x.videoStreamId === d.share.videoStreamId && x.audioStreamId === d.share.audioStreamId);
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

/* ============================== webcam (task I10) ==============================
 * A webcam is an ordinary video share whose label is "Webcam" (plan D7), from `getUserMedia` (D10), no audio with it (`audio: false`: a sender with
 * a webcam and shared audio has that audio as an audio-only share), one at a time (it moves between rooms like native), always encoded in Performance
 * mode with a keyframe every 2000 ms (D8). Its settings (`S.settings.webcam`: camera, mode, codec, bitrate) are the native `WebcamSettings`. */

export const WEBCAM_GOP_MS = 2000;
export const WEBCAM_KBPS_MIN = 200;
export const WEBCAM_KBPS_MAX = 12000;
export const WEBCAM_KBPS_DEFAULT = 2500;
export const camSupported = () => !!(navigator.mediaDevices && navigator.mediaDevices.getUserMedia && navigator.mediaDevices.enumerateDevices);

// What `enumerateDevices` found (`list`), what each camera can do at most (`caps`, from a track that was open: `getCapabilities()` needs one).
const CAM = { list: [], listing: false, asked: false, caps: new Map() };
export const cameras = () => CAM.list;
export const camsRequested = () => CAM.asked;
export const camsListing = () => CAM.listing;

/** (Re)list the cameras. Without the camera permission the browser gives no names ("Camera 1" ...), so the list is refreshed once a camera was opened. */
export async function refreshCameras() {
  if (!camSupported() || CAM.listing) return;
  CAM.listing = true;
  CAM.asked = true;
  invalidate();
  try {
    const devs = await navigator.mediaDevices.enumerateDevices();
    CAM.list = devs.filter((d) => d.kind === "videoinput").map((d, i) => ({ id: d.deviceId, name: d.label || t("ui.webcam.camera_n", { n: i + 1 }), named: !!d.label }));
  } catch {
    CAM.list = [];
  }
  CAM.listing = false;
  invalidate();
}
try {
  navigator.mediaDevices && navigator.mediaDevices.addEventListener && navigator.mediaDevices.addEventListener("devicechange", () => CAM.asked && refreshCameras());
} catch {}

/** The camera the settings name (its device id), else the first one; `null` when none was found. */
export function camChoice() {
  return CAM.list.find((c) => c.id && c.id === S.settings.webcam.cameraId) || CAM.list[0] || null;
}

/** The sizes the mode list offers next to a camera's own maximum: a browser has no list of what a camera delivers. */
const COMMON_SIZES = [[1920, 1080], [1280, 720], [960, 540], [640, 480], [320, 240]];
export const modeKey = (m) => `${m.w}x${m.h}@${m.fps}`;
/** The modes offered for `cam`: its maximum when known (`getCapabilities()` of an opened track), then the common sizes that fit, at 30 fps (60 where the camera does). */
export function camModes(cam) {
  const caps = cam && CAM.caps.get(cam.id);
  const fits = ([w, h]) => !caps || (w <= caps.w && h <= caps.h);
  const out = [];
  const add = (w, h, fps, max = false) => {
    const m = { w, h, fps, max };
    if (!out.some((x) => modeKey(x) === modeKey(m))) out.push(m);
  };
  const fps30 = caps && caps.fps ? Math.min(30, Math.floor(caps.fps)) : 30;
  if (caps && !COMMON_SIZES.some(([w, h]) => w === caps.w && h === caps.h)) add(caps.w, caps.h, fps30, true);
  for (const [w, h] of COMMON_SIZES.filter(fits)) {
    const isMax = !!caps && w === caps.w && h === caps.h;
    add(w, h, fps30, isMax);
    if (caps && caps.fps >= 60 && w >= 1280) add(w, h, 60, isMax);
  }
  return out;
}
/** The mode in use: the settings' key if this camera offers it, else 1280x720 at 30 (what most webcams do), else the first one. */
export function camMode(cam) {
  const modes = camModes(cam);
  return modes.find((m) => modeKey(m) === S.settings.webcam.mode) || modes.find((m) => m.w === 1280 && m.h === 720 && m.fps === 30) || modes[0] || { w: 1280, h: 720, fps: 30 };
}

/** Open the chosen camera in `mode` (the sizes are `ideal`: the browser gives what the camera can). Refuses to leave without a stream. */
async function openCamera(cam, mode) {
  const video = { width: { ideal: mode.w }, height: { ideal: mode.h }, frameRate: { ideal: mode.fps } };
  let stream;
  try {
    stream = await navigator.mediaDevices.getUserMedia({ video: cam && cam.id ? { ...video, deviceId: { exact: cam.id } } : video, audio: false });
  } catch (e) {
    if (e && e.name === "OverconstrainedError" && cam && cam.id) stream = await navigator.mediaDevices.getUserMedia({ video, audio: false }); // that camera is gone: the default one
    else throw e;
  }
  const track = stream.getVideoTracks()[0];
  try {
    const caps = track.getCapabilities ? track.getCapabilities() : null;
    const id = (track.getSettings && track.getSettings().deviceId) || (cam && cam.id) || "";
    if (caps && caps.width && caps.height) CAM.caps.set(id, { w: caps.width.max, h: caps.height.max, fps: caps.frameRate ? caps.frameRate.max : 0 });
  } catch {}
  refreshCameras(); // the camera permission was just granted: names are available now
  return stream;
}

const webcamCfg = (cam) => {
  const m = camMode(cam);
  const wc = S.settings.webcam;
  return { fps: m.fps, codec: wc.codec, preset: "", latencyMode: "realtime", gopMs: WEBCAM_GOP_MS, kbps: wc.kbps }; // fixed encoder (plan D8)
};

/** Start the webcam in `room` (the saved camera in its saved mode); live in another room it MOVES here. Resolves the capture, or null (refused, failed). */
export async function startWebcam(room) {
  const live = webcamCap();
  if (live && live.dests.has(room.id)) return live;
  if (live && live.state === "live") {
    const from = [...live.dests.keys()].map((id) => S.rooms.find((r) => r.id === id)).filter(Boolean);
    for (const f of from) await moveDest(live, f, room);
    return live;
  }
  if (live) return live; // still starting
  const cam = camChoice();
  const c = {
    id: seq++, kind: "webcam", state: "starting", stagedRoom: room.id, stream: null, video: null, audio: null, label: WEBCAM_LABEL, audioMuted: false,
    staged: webcamCfg(cam), dests: new Map(), meter: null,
  };
  S.captures.push(c);
  invalidate();
  let stream;
  try {
    stream = await openCamera(cam, camMode(cam));
  } catch (e) {
    endCapture(c);
    toast(t("ui.toast.webcam_failed", { error: e.message || String(e) }), "error", { err: true });
    return null;
  }
  if (!S.captures.includes(c)) {
    stream.getTracks().forEach((tr) => tr.stop());
    return null;
  }
  adopt(c, stream);
  c.state = "live";
  c.stagedRoom = null;
  try {
    await startDest(c, room, c.staged);
    toast(t("ui.toast.live_in", { room: room.name }), "videocam");
  } catch (e) {
    toast(t("ui.toast.webcam_failed", { error: e.message }), "error", { err: true });
    if (!c.dests.size) endCapture(c);
    return null;
  }
  return c;
}

/** Stop the webcam wherever it is. */
export function stopWebcam() {
  const c = webcamCap();
  if (c) endCapture(c);
}

/** The camera or mode changed (the webcam menu / modal): a running webcam restarts on it, new share first, like a live source change. */
export async function restartWebcam() {
  const c = webcamCap();
  if (!c || c.state !== "live" || !c.dests.size) return;
  const cam = camChoice();
  let stream;
  try {
    stream = await openCamera(cam, camMode(cam));
  } catch (e) {
    toast(t("ui.toast.webcam_failed", { error: e.message || String(e) }), "error", { err: true });
    return;
  }
  if (!S.captures.includes(c)) {
    stream.getTracks().forEach((tr) => tr.stop());
    return;
  }
  const cfg = webcamCfg(cam);
  await swapStream(c, stream, { fps: cfg.fps });
}

/** The webcam's codec changed (restarts the destination) or its bitrate (in place): apply the saved settings to a running webcam. */
export async function applyWebcamSettings() {
  const c = webcamCap();
  if (!c || c.state !== "live") return;
  const wc = S.settings.webcam;
  for (const [roomId, d] of [...c.dests]) {
    const room = S.rooms.find((r) => r.id === roomId);
    if (!room) continue;
    if (d.codec !== wc.codec) await editDest(c, room, { codec: wc.codec });
    else if (d.kbps !== wc.kbps) await editDest(c, room, { kbps: wc.kbps });
  }
}

/* ============================== audio-only share (task I10) ==============================
 * D9: Desktop audio, source groups and the exclusion list are native Linux only, a browser can't list apps. A browser's audio-only share is a
 * getDisplayMedia pick with audio (a tab's, or the system's where the browser offers it): the audio track is kept, the video track stopped. */

/** Pick a tab or system audio for `room` (needs the click's user gesture) and share just the audio. Resolves the capture, or null (cancelled, no audio, failed). */
export async function startAudio(room) {
  if (audioInRoom(room.id)) return null;
  const c = {
    id: seq++, kind: "audio", state: "picking", stagedRoom: room.id, stream: null, video: null, audio: null, label: "", audioMuted: false, surface: "",
    staged: { fps: 0, codec: "", preset: "", kbps: 0 }, dests: new Map(), meter: null,
  };
  S.captures.push(c);
  invalidate();
  let stream;
  try {
    stream = await pickDisplay(60);
  } catch (e) {
    endCapture(c);
    if (e && e.name !== "NotAllowedError" && e.name !== "AbortError") toast(t("ui.toast.pick_failed", { error: e.message }), "error", { err: true });
    return null;
  }
  const v = stream.getVideoTracks()[0];
  c.surface = (v && v.getSettings && v.getSettings().displaySurface) || "";
  stream.getVideoTracks().forEach((tr) => {
    tr.stop();
    stream.removeTrack(tr);
  });
  if (!stream.getAudioTracks().length) {
    stream.getTracks().forEach((tr) => tr.stop());
    endCapture(c);
    toast(t("ui.toast.no_audio_only"), "volume_off", { err: true });
    return null;
  }
  if (!S.captures.includes(c)) {
    stream.getTracks().forEach((tr) => tr.stop());
    return null;
  }
  adopt(c, stream);
  c.state = "live";
  c.stagedRoom = null;
  try {
    await startDest(c, room, c.staged);
    toast(t("ui.toast.audio_live_in", { room: room.name }), "music_note");
  } catch (e) {
    toast(t("ui.toast.audio_failed", { error: e.message }), "error", { err: true });
    if (!c.dests.size) endCapture(c);
    return null;
  }
  return c;
}

/* ---- the webcam menu / modal act at once (native `UiAction::SetWebcam`): save the choice, restart or adjust a running webcam ---- */

/** A new camera starts at its own default mode (and a running webcam restarts on it). */
export function useCamera(cam) {
  S.settings.webcam.cameraId = cam.id;
  S.settings.webcam.mode = "";
  persistSettings();
  invalidate();
  return restartWebcam();
}
export function useMode(key) {
  S.settings.webcam.mode = key;
  persistSettings();
  invalidate();
  return restartWebcam();
}
export function useCodec(codec) {
  S.settings.webcam.codec = codec;
  persistSettings();
  invalidate();
  return applyWebcamSettings();
}
export function useKbps(kbps) {
  S.settings.webcam.kbps = Math.max(WEBCAM_KBPS_MIN, Math.min(WEBCAM_KBPS_MAX, Math.round(kbps)));
  persistSettings();
  invalidate();
  return applyWebcamSettings();
}

/** Open `cam` for a moment to learn its maximum (`getCapabilities()` needs an open track), so the mode list can offer it. Only for a camera the page may
 *  already use (its name is known, i.e. the permission was granted: never a permission prompt out of nowhere) and not while the webcam runs (that track is the source). */
export async function probeCamera(cam) {
  if (!cam || !cam.id || !cam.named || CAM.caps.has(cam.id) || CAM.probing === cam.id || webcamCap()) return;
  CAM.probing = cam.id;
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ video: { deviceId: { exact: cam.id } }, audio: false });
    const track = stream.getVideoTracks()[0];
    const caps = track.getCapabilities ? track.getCapabilities() : null;
    if (caps && caps.width && caps.height) CAM.caps.set(cam.id, { w: caps.width.max, h: caps.height.max, fps: caps.frameRate ? caps.frameRate.max : 0 });
    stream.getTracks().forEach((tr) => tr.stop());
  } catch {}
  CAM.probing = null;
  invalidate();
}
