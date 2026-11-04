// What the stage shows and how each view arranges it: the data side of the stage, shared by stage.js (drawing), bar.js (control
// bar) and menus.js. Native twin: `client/src/ui/stage/mod.rs` (`StageView`, `ItemId`, `collect`, `ViewGeom`).
//
// A VIEW is one room's stage or the combined "Watched streams" stage: `{ kind: "room", r }` or `{ kind: "watched" }`. Each view keeps
// its own order, grid, window rects and z-order (`vg`), UI-private and not persisted (the layout choice, Webcam in picture and the Layout switches are, see `store.js` `persistStage`). Streams are keyed by an item id built from the
// core's `stableKey` (room + sharer name + kind + label), never by the stream key whose ConnId changes with every reconnect, so a
// reconnect keeps the layout (the browser twin of native `ItemId`). Your own share is `<room>/me`.
//
// A PERSON is a connection, not a name: two members of one name are two people (`Why:` the user runs two clients under one name, "Both are
// technically me, but that should be properly handled!"). So an item carries `conn` (its owner's ConnId) and everything that asks "whose is
// this" compares that, room + conn across rooms (`peopleOf`, `sameOwner`). The name only labels. The item id above is the one thing that stays
// name-based, because it must outlive the ConnId: `numberIdentical` keeps two same-named owners' same-labelled streams apart.

import { isWebcamLabel, WEBCAM_LABEL } from "../core/index.js";
import { S, displayName, persistStage } from "./store.js";
import { roomById, allWatches } from "./rooms.js";
import { allocated, capInRoom, webcamInRoom, captureName, captureRes } from "./capture.js";
import { defaultPip } from "./stage-geom.js";
import { fmtKbps } from "./dom.js";
import { t, DICT } from "./i18n.js";

export const WATCHED = { kind: "watched" };
export const roomView = (r) => ({ kind: "room", r });
export const viewKey = (v) => (v.kind === "watched" ? "watched" : "room:" + v.r.id);
export function viewOf(key) {
  if (key === "watched") return WATCHED;
  const r = roomById(String(key).replace(/^room:/, ""));
  return r ? roomView(r) : null;
}

/* ---------- per-view layout state ---------- */
const VG = {}; // view key -> { order: [ids], z: [ids] back to front, float: { id: {x,y,w,h} fractions }, grid, pip: { webcam id: {w, spot, x, y} } }
export const vg = (key) => (VG[key] ??= { order: [], z: [], float: {}, grid: null, pip: {} });
/** The size and spot of the webcam `camId` in its parent's picture in view `key` (a new one starts bottom right, .24 wide). */
export const pipPos = (key, camId) => (vg(key).pip[camId] ??= defaultPip());
/** "Webcam in picture" is on for this view (per view, on until switched off: native `StageSettings::pip`). */
export const pipOn = (key) => !S.stage.pipOff.has(key);
export const layoutOf = (key) => S.stage.layouts[key] || "split";
export const LAYOUTS = [
  ["split", "grid_view", "ui.stage.layout_split", "ui.stage.layout_split_sub"],
  ["crop", "crop", "ui.stage.layout_crop", "ui.stage.layout_crop_sub"],
  ["float", "filter_none", "ui.stage.layout_floating", "ui.stage.layout_floating_sub"],
];
/** Floating starts every window where Split showed it: the layout menu captures nothing, `layoutArena` seeds a missing rect from the
 *  Split picture. */
export function setLayout(key, layout) {
  S.stage.layouts[key] = layout;
  persistStage();
}
/** "Reset sizes and positions": forget the view's grid, floating windows and their order. */
export function resetView(key) {
  const g = vg(key);
  g.grid = null;
  g.float = {};
  g.z = [];
  g.pip = {}; // also forgets the webcams' spots and sizes
}

/* ---------- formatters (the sidebar uses them too) ---------- */
export const watchFps = (w) => (w && w.stats && Number.isFinite(w.stats.decodedFps) ? String(Math.round(w.stats.decodedFps)) : "");
export const watchKbps = (w) => (w && w.stats && Number.isFinite(w.stats.bitrateKbps) ? fmtKbps(w.stats.bitrateKbps) : "");
export const watchRes = (w, info) => {
  const st = w && w.stats;
  if (st && st.width && st.height) return `${st.width}×${st.height}`;
  return info && info.width ? `${info.width}×${info.height}` : "";
};
const CODEC_NAMES = { h264: "H.264", h265: "H.265", av1: "AV1" };
export const codecName = (c) => CODEC_NAMES[String(c || "").toLowerCase().replace(/[.\-\s]/g, "")] || String(c || "").toUpperCase();

/* ---------- items ---------- */

/** Aspect ratio (w/h) of a picture: the decoded frame once there is one, else what the sharer announced, else 16:9 (native `aspect`). */
function aspect(w, h) {
  return w > 0 && h > 0 ? w / h : 16 / 9;
}
export function itemAspect(it) {
  if (it.kind === "video") {
    const cv = it.w.canvas;
    if (it.w.firstFrame && cv.width > 0 && cv.height > 0) return aspect(cv.width, cv.height);
  }
  if (it.kind === "mine") {
    const st = it.c.video && it.c.video.getSettings ? it.c.video.getSettings() : {};
    return aspect(st.width, st.height);
  }
  return aspect(it.s && it.s.width, it.s && it.s.height);
}

/**
 * What `view` shows, in the session's order: a room's streams of other people (watched ones as video, the others as an idle "X is sharing Y"
 * tile, an audio-only sender as a Listen tile), then your own live screen and webcam; "Watched" has every watched video stream of every room
 * and nothing else.
 * Item: `{ id, kind: "video"|"idle"|"audio"|"mine", r, s, w, c, d, conn, owner, label, mine, cam, popped, ar, inPip, pipCam }`. `conn`: the owner's ConnId, whose it
 * is (`owner` is their display name, only for showing). `cam`: a webcam (its label
 * starts with "Webcam"). `kind: "audio"`: `w` is the Listen watch (null while not listening). `inPip`: a webcam drawn inside its owner's screen
 * share (no tile of its own); `pipCam`: on that screen share, the webcam item inside it.
 */
export function collect(view) {
  const out = [];
  const stableId = (r, s) => `${r.id}/${s.stableKey || `${s.ownerName}/${s.label}`}`;
  if (view.kind === "room") {
    const r = view.r;
    for (const s of r.streams) {
      if (s.self) continue;
      if (s.kind === "video") {
        const w = r.watches.get(s.key) || null;
        out.push({ id: stableId(r, s), kind: w ? "video" : "idle", r, s, w, conn: s.owner, owner: s.ownerName || r.memberName(s.owner), label: s.label, mine: false });
      } else if (s.kind === "audio" && !r.ownerHasVideo(s.owner)) {
        // an audio-only sender: a Listen tile (with a screen the audio rides along with the screen's watch and has no tile)
        out.push({ id: stableId(r, s), kind: "audio", r, s, w: r.watches.get(s.key) || null, conn: s.owner, owner: s.ownerName || r.memberName(s.owner), label: s.label, mine: false });
      }
    }
    const c = capInRoom(r.id);
    if (c && c.dests.get(r.id)) out.push({ id: `${r.id}/me`, kind: "mine", r, c, d: c.dests.get(r.id), conn: r.selfId, owner: displayName(), label: captureName(c), mine: true });
    // your webcam is a share of its own: a tile like your screen's (native `Kind::Mine` with the label "Webcam")
    const cam = webcamInRoom(r.id);
    if (cam && cam.dests.get(r.id)) out.push({ id: `${r.id}/me/cam`, kind: "mine", r, c: cam, d: cam.dests.get(r.id), conn: r.selfId, owner: displayName(), label: WEBCAM_LABEL, mine: true });
  } else {
    for (const { r, w } of allWatches()) {
      const s = r.streams.find((x) => x.key === w.key) || { key: w.key, owner: w.owner, ownerName: w.ownerName, label: w.label, width: 0, height: 0 };
      out.push({ id: stableId(r, s), kind: "video", r, s, w, conn: s.owner, owner: w.ownerName, label: w.label, mine: false });
    }
  }
  numberIdentical(out);
  for (const it of out) {
    it.cam = isWebcamLabel(it.label);
    it.label = shareLabel(it.label);
    it.popped = !!(it.w && it.w.popped);
    it.ar = itemAspect(it);
  }
  markPip(view, out);
  return out;
}

/** Items with one id (two same-named owners sharing one label, or one owner's two "Screen"s) get `#1`, `#2`... in stream key order, so a tile never shares its layout
 *  with another (native `number_identical`). `Why` stream key order and not the list's: the same stream keeps its number from frame to frame. A ConnId is new after a
 *  reconnect, so two same-named owners may swap numbers then, and so places: nothing is lost, and no identity that survives a reconnect can tell them apart. */
export function numberIdentical(items) {
  const seen = new Map();
  const key = (i) => (i.s ? i.s.key : "");
  for (const it of [...items].sort((a, b) => (key(a) < key(b) ? -1 : key(a) > key(b) ? 1 : 0))) {
    const n = seen.get(it.id) || 0;
    seen.set(it.id, n + 1);
    if (n) it.id = `${it.id}#${n}`;
  }
}

/** Who the strip shows: a room's members (you first), or the owners of the watched streams (Watched). A person is a connection, so `key` is room + ConnId
 *  and a person's `items` are the ones with their `conn`: never compare names here. `{ key, conns, name, you, emoji, items, r }` (`r`: the first connection's room). */
export function peopleOf(view, items) {
  const personKey = (r, conn) => `${r.id}/${conn}`;
  if (view.kind === "room") {
    const r = view.r;
    const members = [...r.members].sort((a, b) => (b.self ? 1 : 0) - (a.self ? 1 : 0));
    return members.map((m) => ({ key: personKey(r, m.id), name: m.name || "?", you: !!m.self, emoji: r.emojiOf(m.id), items: items.filter((i) => (m.self ? i.mine : !i.mine && i.conn === m.id)), r }));
  }
  // Watched: group by connection (room + ConnId), then merge connections of one NAME across DIFFERENT rooms into one person; yours (`mine`) are all one
  // "You". `Why:` the name is the only identity across rooms, but two connections of one name in the SAME room are provably two (the user's report above),
  // so a person never takes a second connection of a room it already has; with several candidates the first person wins. The merged emoji is the first
  // connection's (the others may differ; nothing better to pick). `conns`: the person's `room/ConnId` keys; `key` is the first.
  const people = [];
  for (const it of items) {
    const at = personKey(it.r, it.conn);
    let p = people.find((x) => x.conns.includes(at));
    if (!p) p = people.find((x) => (it.mine ? x.you : !x.you && x.name === it.owner) && x.conns.every((c) => !c.startsWith(`${it.r.id}/`)));
    if (p) { if (!p.conns.includes(at)) p.conns.push(at); }
    else people.push((p = { key: at, conns: [at], name: it.mine ? "" : it.owner, you: !!it.mine, emoji: it.r.emojiOf(it.conn), items: [], r: it.r }));
    p.items.push(it);
  }
  return people;
}

/** A label that is a known default in any language shows in the current one (native `share_label`): the webcam's wire label is `Webcam` everywhere, a Desktop
 *  audio share announces the English "Desktop audio" (task I7); anything else (a browser's "Tab audio", an app's name) as is. */
export function shareLabel(raw) {
  if (isWebcamLabel(raw)) return t("ui.stream.webcam");
  if (Object.values(DICT).some((d) => d["share.desktop_audio_label"] === raw)) return t("share.desktop_audio_label");
  return raw;
}

/* ---------- webcam in picture: who sits inside whom ---------- */

/** Whether a webcam item and its parent are the same person: the same room and the same connection (a name could be someone else's). */
const sameOwner = (a, b) => a.r === b.r && a.conn === b.conn;
/** An item that is playing on the stage: a watched stream or your own share, not in a pop-out window. A stream in the FULLSCREEN stage still counts: the
 *  PIP goes with it into the fullscreen (native `fullscreen_pip`), only a pop-out window shows its stream alone. */
const playing = (i) => (i.kind === "video" || i.kind === "mine") && (!i.popped || (i.w && i.w.popped === "fullscreen"));

/**
 * Webcam in picture (native `pip_pairs`, the replica's `pipPairs`): parent item id -> the webcam item that sits inside it. The parent is the webcam
 * owner's first screen share, in the view's order, that is on the stage and playing (yours, or one you watch, and not popped out), in the same room; the
 * webcam must be playing too. With none, the webcam stays a tile (a webcam you cannot see playing in a picture would have no Watch button anywhere).
 * One parent holds one webcam: the first one, a second keeps its tile. Pure: `items` carry `{ id, kind, cam, conn, r, popped, w }`.
 */
export function pipPairs(items, order) {
  const list = order.map((id) => items.find((i) => i.id === id)).filter((i) => i && playing(i));
  const pairs = new Map();
  for (const c of list.filter((i) => i.cam)) {
    const p = list.find((i) => !i.cam && sameOwner(c, i));
    if (p && !pairs.has(p.id)) pairs.set(p.id, c);
  }
  return pairs;
}

/** Make sure every item is in the view's order, then (when the view has Webcam in picture on) mark the webcams that sit inside a share. */
function markPip(view, items) {
  const key = viewKey(view);
  const g = vg(key);
  for (const it of items) if (!g.order.includes(it.id)) g.order.push(it.id);
  if (!pipOn(key)) return;
  for (const [pid, cam] of pipPairs(items, g.order)) {
    cam.inPip = true;
    items.find((i) => i.id === pid).pipCam = cam;
  }
}

/** The item with `id` in `view` (null when it is gone). */
export const itemById = (view, id) => collect(view).find((i) => i.id === id) || null;
/** The view's items in the view's own order (new ones appended), minus popped-out ones, the webcams inside a picture and, with "Hide my own streams", your own
 *  (a webcam inside your own screen goes with it). */
export function visibleItems(view, items) {
  const g = vg(viewKey(view));
  for (const it of items) if (!g.order.includes(it.id)) g.order.push(it.id);
  return g.order.map((id) => items.find((i) => i.id === id)).filter((i) => i && !i.popped && !i.inPip && !(S.stage.hideOwn && i.mine));
}

/** Whether a person's `items` carry audible audio (a watched stream whose sender shares audio and is not muted, or my own share with audio that is not muted): the
 *  strip's green ring. */
export function audibleFor(items) {
  return items.some((it) => {
    if (it.mine) return !!it.c.audio && !it.c.audioMuted;
    return !!it.w && !it.cam && it.r.hasAudio(it.s.owner) && !it.w.muted; // a webcam brings no audio
  });
}

/** `res · fps · codec · rate` of an item (what the hover card prints); for an unwatched stream only the announced size. */
export function itemNumbers(it) {
  if (it.kind === "audio") return t("ui.room.audio_only");
  if (it.mine) return [captureRes(it.c), `${it.d.fps} fps`, codecName(it.d.codec), fmtKbps(allocated(it.d))].filter(Boolean).join(" · ");
  if (!it.w) return it.s && it.s.width ? `${it.s.width}×${it.s.height}` : "";
  const st = it.w.stats;
  const fps = watchFps(it.w);
  return [watchRes(it.w, it.s), fps ? `${fps} fps` : "", codecName((st && st.codec) || it.w.codec), watchKbps(it.w)].filter(Boolean).join(" · ");
}
