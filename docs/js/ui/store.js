// Persistence (localStorage) for settings and saved rooms, plus the shared UI state object and its re-render hook.
// Every localStorage access is wrapped: a private window or blocked storage must not break the page.

import { DEFAULT_MAX_JITTER_BUFFER_MS, DEFAULT_CATCH_UP_SPEED_PERCENT, clampMaxJitterBufferMs, clampCatchUpSpeedPercent, ENCODER_PRESETS } from "../core/index.js";

const K_SETTINGS = "sss.settings.v1";
const K_ROOMS = "sss.rooms.v1";
const K_AUTONAME = "sss.autoname.v1";

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

/**
 * The automatic default display name, `user-` + 4 random characters of [a-z0-9]. Generated once and persisted under its
 * own key so it is stable across reloads, and kept apart from `settings.name`, which stays "" until the user chooses a
 * name themselves (native does the same: `display_name` unset, name derived from a fallback). Without working storage it
 * is stable for the page's lifetime only.
 */
let autoNameCache = null;
export function autoName() {
  if (autoNameCache) return autoNameCache;
  const stored = read(K_AUTONAME);
  if (typeof stored === "string" && /^user-[a-z0-9]{4}$/.test(stored)) return (autoNameCache = stored);
  const alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
  let id = "";
  const buf = new Uint8Array(16);
  while (id.length < 4) {
    crypto.getRandomValues(buf);
    for (const b of buf) if (b < 252 && id.length < 4) id += alphabet[b % 36]; // 252 = 7 * 36: no modulo bias
  }
  autoNameCache = "user-" + id;
  write(K_AUTONAME, autoNameCache);
  return autoNameCache;
}

/** The name sent to rooms and shown as "you": the chosen name, else the automatic default. */
export const displayName = () => (S.settings.name || "").trim() || autoName();

const browserLang = () => (String(navigator.language || "en").toLowerCase().startsWith("de") ? "de" : "en");

const PRESET_NAMES = ["text", "quality", "gaming"];
/** The keyframe intervals the stream settings offer, ms (the presets use 250 and 1000, the webcam 2000); a stored other one is listed as it is. */
export const GOP_OPTIONS = [250, 500, 1000, 2000];
/** The preset whose FPS, encoding mode and keyframe interval are exactly these (native `EncoderPreset::detect`), or ""
 *  when none: a hand-made combination, no preset lit. */
export const presetOf = (fps, latencyMode, gopMs) => PRESET_NAMES.find((p) => ENCODER_PRESETS[p].fps === fps && ENCODER_PRESETS[p].latencyMode === latencyMode && ENCODER_PRESETS[p].gopMs === gopMs) || "";
/** What a stream config encodes with: its own `latencyMode` ("quality" | "realtime") and `gopMs`, else its preset's (an older stored config has only a preset). */
export function encoderOf(d) {
  const p = ENCODER_PRESETS[d.preset] || ENCODER_PRESETS.quality;
  return { latencyMode: d.latencyMode === "quality" || d.latencyMode === "realtime" ? d.latencyMode : p.latencyMode, gopMs: Number.isFinite(d.gopMs) && d.gopMs > 0 ? d.gopMs : p.gopMs };
}
function normalizeStream(st) {
  if (!PRESET_NAMES.includes(st.preset)) st.preset = "quality";
  const { latencyMode, gopMs } = encoderOf(st);
  Object.assign(st, { latencyMode, gopMs, preset: presetOf(st.fps, latencyMode, gopMs) });
  return st;
}

const WEBCAM_CODECS = ["h264", "av1", "h265"];
function normalizeWebcam(w) {
  w = w && typeof w === "object" ? w : {};
  return {
    cameraId: typeof w.cameraId === "string" ? w.cameraId : "",
    mode: typeof w.mode === "string" && /^\d+x\d+@\d+$/.test(w.mode) ? w.mode : "",
    codec: WEBCAM_CODECS.includes(w.codec) ? w.codec : "h264",
    kbps: Number.isFinite(w.kbps) ? Math.min(12000, Math.max(200, Math.round(w.kbps))) : 2500,
  };
}

const STAGE_LAYOUTS = ["split", "crop", "float"];
/** The Layout menu's saved choices (native `rooms::StagePrefs`): three global switches, and per view key ("room:<id>" | "watched") its layout
 *  and "Webcam in picture" (on by default). `views` holds only views that differ from the defaults (Split, picture on). */
function normalizeStage(st) {
  st = st && typeof st === "object" ? st : {};
  const views = {};
  for (const [k, v] of Object.entries(st.views && typeof st.views === "object" ? st.views : {})) {
    const layout = v && STAGE_LAYOUTS.includes(v.layout) ? v.layout : "split";
    const pip = !(v && v.pip === false);
    if (layout !== "split" || !pip) views[k] = { layout, pip };
  }
  return { hideOwn: st.hideOwn === true, alwaysNames: st.alwaysNames === true, showStats: st.showStats === true, hideUnfocused: st.hideUnfocused !== false, views };
}

export function loadSettings() {
  const s = read(K_SETTINGS) || {};
  const out = {
    name: typeof s.name === "string" ? s.name : "",
    // Set once the user has chosen a name (the "Choose your name" modal or the settings field): never ask again,
    // even if they later empty the settings field.
    nameSet: s.nameSet === true || (typeof s.name === "string" && s.name.trim() !== ""),
    lang: s.lang === "de" || s.lang === "en" ? s.lang : browserLang(),
    jitter: clampMaxJitterBufferMs(Number.isFinite(s.jitter) ? s.jitter : DEFAULT_MAX_JITTER_BUFFER_MS),
    catchup: clampCatchUpSpeedPercent(Number.isFinite(s.catchup) ? s.catchup : DEFAULT_CATCH_UP_SPEED_PERCENT),
    // Viewer-side loudness levelling of watched audio (#119); on unless the user switched it off.
    levelLoudness: s.levelLoudness !== false,
    // Settings > Playback > "Automatically watch new streams" (row 176): on unless the user switched it off (a missing value is on).
    autoWatch: s.autoWatch !== false,
    // Notification sounds volume, percent 0-100 (0 = silent), separate from every stream's volume (I9; `sounds.js`). Default 50.
    soundVol: Number.isFinite(s.soundVol) ? Math.min(100, Math.max(0, Math.round(s.soundVol))) : 50,
    // Upload bandwidth, same two modes as native: per-stream maximum, or one absolute total split across every stream.
    bw: Object.assign({ mode: "absolute", total: 12000, floor: 3000 }, s.bw || {}),
    // Defaults for the next stream (the staged stream-settings modal starts from these).
    // The presets are Text, Quality and Gaming (I2); a stored "balanced" or "low_latency" (and anything unknown) is Quality, like the
    // native settings file (`rooms::StartStreamDefaults`).
    stream: normalizeStream(Object.assign({ fps: 60, codec: "h264", preset: "quality", kbps: 6000, audio: true }, s.stream || {})),
    // The webcam (task I10; native `WebcamSettings`): the camera's device id ("" = the first one), its mode `WxH@fps` ("" = the default, 1280x720 at 30),
    // the codec and the bitrate (200-12000 kbps, default 2500). The encoder is fixed: Performance, keyframe 2000 ms.
    webcam: normalizeWebcam(s.webcam),
    // Settings > Profile: my emoji avatar ("" = none), announced to every room's relay (core `setAvatar`).
    emoji: typeof s.emoji === "string" ? s.emoji.trim() : "",
    // The Layout menu (#131): see `normalizeStage`.
    stage: normalizeStage(s.stage),
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
const initialSettings = loadSettings();
const stageLayouts = () => Object.fromEntries(Object.entries(initialSettings.stage.views).filter(([, v]) => v.layout !== "split").map(([k, v]) => [k, v.layout]));
const stagePipOff = () => new Set(Object.entries(initialSettings.stage.views).filter(([, v]) => !v.pip).map(([k]) => k));
/** A phone-sized screen: narrow (portrait) or short and touch (landscape). The SAME query is in css/app.css `@media`; change both together. ponytail: one breakpoint, no tablet tier. */
export const PHONE_Q = "(max-width: 760px), (max-height: 500px) and (pointer: coarse)";
export const PHONE = window.matchMedia(PHONE_Q);
export const S = {
  drawer: false, // phone: the rooms list slides over the stage (css `.drawer-open`)
  settings: initialSettings,
  rooms: [], // RoomEntry (rooms.js)
  captures: [], // Capture (capture.js)
  view: "room", // "room" | "watching"
  selected: null, // room id
  modal: null,
  popup: null,
  toasts: [],
  dropLog: [], // Settings > Connection: what ended or restarted a watch or the room connection, newest last (`dropLog()` below)
  flash: null,
  menu: null, // the open dropdown / context menu (menus.js): { kind, mk, ... }
  // The stage (stage.js). Layout geometry per view (order, sizes, spots) lives there, UI-private and per session like native `StageState`. These are
  // the choices the Layout menu and the bar set; they are saved (`persistStage`, `settings.stage`, keyed by saved room: #131) and loaded here.
  stage: {
    layouts: stageLayouts(), // view key ("room:<id>" | "watched") -> "split" | "crop" | "float"
    hideOwn: initialSettings.stage.hideOwn,
    pipOff: stagePipOff(), // view keys with "Webcam in picture" switched off (on until then: native `StageSettings::pip`)
    alwaysNames: initialSettings.stage.alwaysNames,
    hideUnfocused: initialSettings.stage.hideUnfocused, // Layout menu "Hide when unfocused" (on by default): the own screen preview stops while the page is unfocused; a webcam never does
    stripHidden: false, // the people button hid the strip
    sidebarHidden: false,
    showStats: initialSettings.stage.showStats, // Layout menu "Show stats": the numbers on every watched stream's name tag (native `StageSettings::show_stats`)
    popped: new Set(), // view keys whose whole stage is in its own (Document PiP) window
    fullscreen: false,
  },
  card: null, // the open hover card in the people strip: { name, view }
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

/** One line of the connection log (Settings > Connection): `kind` is "lost" | "back" | "stream" | "recover". The user has no console on a phone, so every
 *  drop of a watch writes its reason here. Kept in memory for the page's life, newest last, at most 80 lines. */
export function dropLog(kind, text) {
  const last = S.dropLog[S.dropLog.length - 1];
  if (last && last.kind === kind && last.text === text) { last.n++; last.at = Date.now(); return; } // the same line again: counted, not repeated
  S.dropLog.push({ at: Date.now(), kind, text, n: 1 });
  if (S.dropLog.length > 200) S.dropLog.splice(0, S.dropLog.length - 200);
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
/** Save the Layout menu's choices (`S.stage`) into `settings.stage`. */
export function persistStage() {
  const st = S.stage;
  const keys = new Set([...Object.keys(st.layouts), ...st.pipOff]);
  S.settings.stage = normalizeStage({ hideOwn: st.hideOwn, alwaysNames: st.alwaysNames, showStats: st.showStats, hideUnfocused: st.hideUnfocused, views: Object.fromEntries([...keys].map((k) => [k, { layout: st.layouts[k] || "split", pip: !st.pipOff.has(k) }])) });
  persistSettings();
}
export const persistRooms = () => saveRoomList(S.rooms);
