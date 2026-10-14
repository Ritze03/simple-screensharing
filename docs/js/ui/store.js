// Persistence (localStorage) for settings and saved rooms, plus the shared UI state object and its re-render hook.
// Every localStorage access is wrapped: a private window or blocked storage must not break the page.

import { DEFAULT_MAX_JITTER_BUFFER_MS, DEFAULT_CATCH_UP_SPEED_PERCENT, clampMaxJitterBufferMs, clampCatchUpSpeedPercent } from "../core/index.js";

const K_SETTINGS = "sss.settings.v1";
const K_ROOMS = "sss.rooms.v1";

function read(key) {
  try {
    const v = localStorage.getItem(key);
    return v ? JSON.parse(v) : null;
  } catch {
    return null;
  }
}
function write(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
    return true;
  } catch {
    return false;
  }
}

const browserLang = () => (String(navigator.language || "en").toLowerCase().startsWith("de") ? "de" : "en");

export function loadSettings() {
  const s = read(K_SETTINGS) || {};
  const out = {
    name: typeof s.name === "string" ? s.name : "",
    lang: s.lang === "de" || s.lang === "en" ? s.lang : browserLang(),
    jitter: clampMaxJitterBufferMs(Number.isFinite(s.jitter) ? s.jitter : DEFAULT_MAX_JITTER_BUFFER_MS),
    catchup: clampCatchUpSpeedPercent(Number.isFinite(s.catchup) ? s.catchup : DEFAULT_CATCH_UP_SPEED_PERCENT),
    // Upload bandwidth, same two modes as native: per-stream maximum, or one absolute total split across every stream.
    bw: Object.assign({ mode: "absolute", total: 12000, floor: 3000 }, s.bw || {}),
    // Defaults for the next stream (the staged stream-settings modal starts from these).
    stream: Object.assign({ fps: 60, codec: "h264", preset: "balanced", kbps: 6000, audio: true }, s.stream || {}),
    lastServer: typeof s.lastServer === "string" ? s.lastServer : "",
    lastCertHash: typeof s.lastCertHash === "string" ? s.lastCertHash : "",
  };
  return out;
}
export const saveSettings = (s) => write(K_SETTINGS, s);

/** Saved rooms: the join link is the only thing that matters; name and auto-connect are local. */
export function loadRooms() {
  const r = read(K_ROOMS);
  return Array.isArray(r) ? r.filter((x) => x && typeof x.link === "string") : [];
}
export const saveRoomList = (rooms) => write(K_ROOMS, rooms.map((r) => ({ id: r.id, name: r.name, link: r.link, auto: !!r.auto })));

/* ---- shared UI state ---- */
export const S = {
  settings: loadSettings(),
  rooms: [], // RoomEntry (rooms.js)
  captures: [], // Capture (capture.js)
  view: "room", // "room" | "watching"
  selected: null, // room id
  modal: null,
  popup: null,
  toasts: [],
  flash: null,
  advOpen: false,
};

let renderFn = null;
let pending = false;
export function onInvalidate(fn) {
  renderFn = fn;
}
/** Coalesced full re-render (next animation frame, or a timer while the tab is hidden). */
export function invalidate() {
  if (pending) return;
  pending = true;
  const run = () => {
    if (!pending) return;
    pending = false;
    renderFn && renderFn();
  };
  requestAnimationFrame(run);
  setTimeout(run, 100);
}

export function toast(text, icon = "check_circle", { err = false } = {}) {
  const t = { id: Math.random(), text, icon, err };
  S.toasts.push(t);
  invalidate();
  setTimeout(() => {
    S.toasts = S.toasts.filter((x) => x !== t);
    invalidate();
  }, err ? 5000 : 2600);
}

export const persistSettings = () => saveSettings(S.settings);
export const persistRooms = () => saveRoomList(S.rooms);
