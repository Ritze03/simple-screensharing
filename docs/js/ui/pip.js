// Webcam in picture (task I10): one webcam drawn inside its owner's screen share instead of as a tile of its own. The maths is stage-geom.js (a line-for-line port of
// native `stage/geom.rs`, tested); this file builds one PIP element, places it and reads its pointer input. Native twin: `client/src/ui/stage/pip.rs`.
//
// - Drag moves it (kept inside the visible picture, in Crop too), release snaps it to the nearest of eight spots (corners and side middles) and glides there
//   (.16 s, none under `prefers-reduced-motion`; native cannot animate); the handle on the corner facing the middle of the picture resizes it with its aspect
//   locked and the opposite corner fixed. A click changes nothing. Right click opens the webcam's own stream menu, a double click is its fullscreen.
// - It never moves what is under it: a press on the PIP stops there, so the tile under it is never dragged or resized.
// - The webcam's picture is the watch's persistent <canvas> (or, for your own webcam, the capture's preview <video>): a new render builds a new PIP element and
//   re-adopts it, so video never restarts when a webcam moves between its own tile and its parent.
// - The position lives in the VIEW (`vg(key).pip[camId]`, keyed by the webcam's item id = room + stableKey): the spot survives layouts, tile resizes and fullscreen.

import { h, ic } from "./dom.js";
import { t } from "./i18n.js";
import { A } from "./actions.js";
import * as G from "./stage-geom.js";
import { pipPos, itemAspect } from "./stage-state.js";
import { previewFor } from "./capture.js";
import { openTileMenu } from "./menus.js";

/** A pointer drag of a PIP in progress: `{ el, kind: "move" | "resize", corner, from, sx, sy, moved }` (`from` = the PIP as it was at the press). */
let drag = null;
export const pipDragging = () => !!drag;

/**
 * The element of the webcam `cam` (an item of stage-state.js `collect`). `where` = `{ key, geom, onPress? }`: `key` = the view the position is stored in,
 * `geom()` = `{ tw, th, ar, crop }` of the box the PIP sits in (its parent tile, or the fullscreen stage) right now, `onPress` runs on a press (a floating
 * parent comes to the front).
 */
export function pipEl(cam, where) {
  const mine = cam.mine;
  const tip = mine ? t("ui.stage.pip_tip_mine") : t("ui.stage.pip_tip", { who: cam.owner });
  const el = h("div", { class: "pip", "data-id": cam.id, "data-ar": cam.ar.toFixed(4), title: tip });
  if (cam.kind === "video") {
    el.appendChild(cam.w.canvas);
  } else {
    el.appendChild(h("div", { class: "mine-pic" }, ic("videocam")));
    const pv = previewFor(cam.c);
    if (pv) el.appendChild(pv);
  }
  el.appendChild(h("span", { class: "prz hover-ui", title: t("ui.stage.resize_tip") }));
  el._cam = cam;
  el._where = where;
  el.addEventListener("pointerdown", (e) => {
    if (e.button !== 0) return;
    e.stopPropagation(); // the tile under it never sees the press: no tile drag, no window move
    e.preventDefault();
    if (where.onPress) where.onPress();
    if (!el._geo) return;
    drag = { el, kind: e.target.closest(".prz") ? "resize" : "move", corner: el._geo.corner, from: el._geo, sx: e.clientX, sy: e.clientY, moved: false };
    el.classList.remove("snap");
    el.classList.add("held");
  });
  el.addEventListener("dblclick", (e) => {
    e.stopPropagation();
    if (cam.kind === "video" && cam.w) A.doFullscreen(cam.r, cam.w);
  });
  el.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    e.stopPropagation();
    openTileMenu(where.key, cam.id, el, { x: e.clientX, y: e.clientY });
  });
  return el;
}

/** Put a PIP where its stored position says in its box (call after the box has its size). */
export function placePip(el) {
  const cam = el._cam;
  if (!cam || !el.isConnected) return null;
  const { tw, th, ar, crop } = el._where.geom();
  if (!(tw > 0 && th > 0)) return null;
  const a = parseFloat(el.dataset.ar) || 16 / 9;
  const geo = G.layoutPip(tw, th, ar, a, crop, pipPos(el._where.key, cam.id));
  el._geo = geo;
  const s = el.style;
  s.left = geo.rect.x + "px";
  s.top = geo.rect.y + "px";
  s.width = geo.rect.w + "px";
  s.height = geo.rect.h + "px";
  el.dataset.c = (geo.corner.right ? "r" : "l") + (geo.corner.bottom ? "b" : "t");
  return geo;
}

/** The pointer moved: a PIP drag follows it (the position is stored first, then placed, so nothing lags). Returns whether a PIP drag is in progress. */
export function pipPointerMove(e) {
  if (!drag) return false;
  const d = drag;
  if (!d.el.isConnected) {
    drag = null;
    return false;
  }
  const dx = e.clientX - d.sx;
  const dy = e.clientY - d.sy;
  if (!d.moved && Math.hypot(dx, dy) < 4) return true; // a click is not a drag
  d.moved = true;
  const pos = pipPos(d.el._where.key, d.el._cam.id);
  if (d.kind === "move") G.movePip(d.from, dx, dy, pos);
  else G.resizePip(d.from, d.corner, dx, dy, pos);
  placePip(d.el);
  return true;
}

/** The pointer was released: a moved PIP goes to the nearest spot (from where it was drawn), then the held-back render runs. */
export function pipPointerUp() {
  if (!drag) return false;
  const d = drag;
  drag = null;
  const el = d.el;
  el.classList.remove("held");
  if (el.isConnected && d.moved && d.kind === "move") {
    const geo = placePip(el);
    G.snapPip(geo, pipPos(el._where.key, el._cam.id));
    el.classList.add("snap"); // glide to the spot
    placePip(el);
    setTimeout(() => el.classList.remove("snap"), 260);
  } else if (el.isConnected) placePip(el);
  A.flush();
  return true;
}

/** The webcam in the FULLSCREEN stage of its parent (popout.js): `stage` is the `.pip-stage` holding the parent's canvas, `parent`/`cam` stage-state items.
 *  Same position, same spot as on the stage (native `fullscreen_pip`). Returns what undoes it. */
export function fullscreenPip(stage, parent, cam, key) {
  const el = pipEl(cam, { key, geom: () => ({ tw: stage.clientWidth, th: stage.clientHeight, ar: itemAspect(parent), crop: false }) });
  stage.appendChild(el);
  const relayout = () => {
    el.dataset.ar = itemAspect(cam).toFixed(4);
    if (!drag) placePip(el);
  };
  relayout();
  const timer = setInterval(relayout, 400); // a stream can change shape (and the stage resizes with the window)
  window.addEventListener("resize", relayout);
  return () => {
    clearInterval(timer);
    window.removeEventListener("resize", relayout);
    if (drag && drag.el === el) drag = null;
    el.remove();
  };
}
