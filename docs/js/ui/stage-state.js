// What the stage shows and how each view arranges it: the data side of the stage, shared by stage.js (drawing), bar.js (control
// bar) and menus.js. Native twin: `client/src/ui/stage/mod.rs` (`StageView`, `ItemId`, `collect`, `ViewGeom`).
//
// A VIEW is one room's stage or the combined "Watched streams" stage: `{ kind: "room", r }` or `{ kind: "watched" }`. Each view keeps
// its own order, grid, window rects and z-order (`vg`), UI-private and not persisted. Streams are keyed by an item id built from the
// core's `stableKey` (room + sharer name + kind + label), never by the stream key whose ConnId changes with every reconnect, so a
// reconnect keeps the layout (the browser twin of native `ItemId`). Your own share is `<room>/me`.

import { S, displayName } from "./store.js";
import { roomById, allWatches } from "./rooms.js";
import { allocated, capInRoom, captureName, captureRes } from "./capture.js";
import { fmtKbps } from "./dom.js";

export const WATCHED = { kind: "watched" };
export const roomView = (r) => ({ kind: "room", r });
export const viewKey = (v) => (v.kind === "watched" ? "watched" : "room:" + v.r.id);
export function viewOf(key) {
  if (key === "watched") return WATCHED;
  const r = roomById(String(key).replace(/^room:/, ""));
  return r ? roomView(r) : null;
}

/* ---------- per-view layout state ---------- */
const VG = {}; // view key -> { order: [ids], z: [ids] back to front, float: { id: {x,y,w,h} fractions }, grid }
export const vg = (key) => (VG[key] ??= { order: [], z: [], float: {}, grid: null });
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
}
/** "Reset sizes and positions": forget the view's grid, floating windows and their order. */
export function resetView(key) {
  const g = vg(key);
  g.grid = null;
  g.float = {};
  g.z = [];
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
 * What `view` shows, in the session's order: a room's video streams of other people (watched ones as video, the others as an idle
 * "X is sharing Y" tile), then your own live share; "Watched" has every watched stream of every room and nothing else. Audio-only
 * senders get no tile yet (task I9: audio-only sharing and Listen).
 * Item: `{ id, kind: "video"|"idle"|"mine", r, s, w, c, d, owner, label, mine, popped, ar }`.
 */
export function collect(view) {
  const out = [];
  const seen = new Map();
  const uniq = (base) => {
    const n = seen.get(base) || 0;
    seen.set(base, n + 1);
    return n ? `${base}#${n}` : base;
  };
  const stableId = (r, s) => uniq(`${r.id}/${s.stableKey || `${s.ownerName}/${s.label}`}`);
  if (view.kind === "room") {
    const r = view.r;
    for (const s of r.videoStreams()) {
      const w = r.watches.get(s.key) || null;
      out.push({ id: stableId(r, s), kind: w ? "video" : "idle", r, s, w, owner: s.ownerName || r.memberName(s.owner), label: s.label, mine: false });
    }
    const c = capInRoom(r.id);
    if (c && c.dests.get(r.id)) out.push({ id: `${r.id}/me`, kind: "mine", r, c, d: c.dests.get(r.id), owner: displayName(), label: captureName(c), mine: true });
  } else {
    for (const { r, w } of allWatches()) {
      const s = r.streams.find((x) => x.key === w.key) || { key: w.key, owner: w.owner, ownerName: w.ownerName, label: w.label, width: 0, height: 0 };
      out.push({ id: stableId(r, s), kind: "video", r, s, w, owner: w.ownerName, label: w.label, mine: false });
    }
  }
  for (const it of out) {
    it.popped = !!(it.w && it.w.popped);
    it.ar = itemAspect(it);
  }
  return out;
}

/** The item with `id` in `view` (null when it is gone). */
export const itemById = (view, id) => collect(view).find((i) => i.id === id) || null;
/** The view's items in the view's own order (new ones appended), minus popped-out ones and, with "Hide my own streams", your own. */
export function visibleItems(view, items) {
  const g = vg(viewKey(view));
  for (const it of items) if (!g.order.includes(it.id)) g.order.push(it.id);
  return g.order.map((id) => items.find((i) => i.id === id)).filter((i) => i && !i.popped && !(S.stage.hideOwn && i.mine));
}

/** Whether the person called `name` shares audible audio in `items` (a watched stream whose sender shares audio and is not muted, or
 *  my own share with audio that is not muted): the strip's green ring. */
export function audibleFor(items, name, self) {
  return items.some((it) => {
    if (it.mine) return self && !!it.c.audio && !it.c.audioMuted;
    return it.owner === name && !!it.w && it.r.hasAudio(it.s.owner) && !it.w.muted;
  });
}

/** `res · fps · codec · rate` of an item (what the hover card prints); for an unwatched stream only the announced size. */
export function itemNumbers(it) {
  if (it.mine) return [captureRes(it.c), `${it.d.fps} fps`, codecName(it.d.codec), fmtKbps(allocated(it.d))].filter(Boolean).join(" · ");
  if (!it.w) return it.s && it.s.width ? `${it.s.width}×${it.s.height}` : "";
  const st = it.w.stats;
  const fps = watchFps(it.w);
  return [watchRes(it.w, it.s), fps ? `${fps} fps` : "", codecName((st && st.codec) || it.w.codec), watchKbps(it.w)].filter(Boolean).join(" · ");
}
