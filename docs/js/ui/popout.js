// Pop-outs. A browser page cannot open a free OS window like native does, so:
//   1. Document Picture-in-Picture (`documentPictureInPicture.requestWindow`, Chromium 116+): a real always-on-top window.
//      A STREAM's own <canvas> is moved into it (no second decode); the whole STAGE can be moved into one too (`popStage`).
//      Only one such window per page exists: popping out another stream (or the stage) brings the previous one back first.
//   2. Otherwise Fullscreen: the canvas moves into a fullscreen stage; leaving fullscreen brings it back. (The stage has no such
//      fallback: its fullscreen button is the whole-stage fullscreen, so its pop-out button is hidden without Document PiP.)
// Both need the click's user activation, so call popOut()/popStage() straight from a click handler.
//
// A stream's pop-out survives a reconnect or a relay restart (D1, native `popout_survives_resume`): the watch ends and the same
// stream comes back as a NEW watch with a new canvas, so `rebind()` moves the new canvas into the window that is still open (a
// window cannot be re-opened without a user gesture, so it is simply kept open while the 60 s resume window runs).

import { h, ic } from "./dom.js";
import { t } from "./i18n.js";

let current = null; // { w, win?, stage, kind, onBack }
let stageWin = null; // { win, root, key, onBack }

export const pipSupported = () => typeof window.documentPictureInPicture !== "undefined" && !!window.documentPictureInPicture.requestWindow;
export const popoutMode = () => (pipSupported() ? "pip" : document.fullscreenEnabled ? "fullscreen" : null);

function stageFor(w, doc, onClose) {
  return h("div", { class: "pip-stage" },
    w.canvas,
    h("div", { class: "ovl hb g2", style: "align-items:flex-start" },
      h("div", { class: "fl" }, h("div", { class: "hb g2 statbar" }, h("span", { class: "dot" }), h("span", { class: "txt nm", text: w.ownerName }))),
      h("button", { class: "round-btn", type: "button", title: t("ui.room.bring_back"), "aria-label": t("ui.room.bring_back"), onclick: onClose }, ic("move_down"))));
}

const IDLE_MS = 3000; // fullscreen overlay auto-hide, same as native `popout.rs` OVERLAY_IDLE (#124)

/** Fullscreen stage: after IDLE_MS without mouse move / click / wheel / key, hide the overlay and the cursor
 *  (`.pip-stage.idle`); the overlay stays while the pointer rests on a control. */
function autoHide(stage) {
  let timer = 0;
  const arm = () => {
    stage.classList.remove("idle");
    clearTimeout(timer);
    timer = setTimeout(() => {
      if (!stage.isConnected) return document.removeEventListener("keydown", arm);
      if (stage.querySelector(".statbar:hover, .round-btn:hover")) return arm();
      stage.classList.add("idle");
    }, IDLE_MS);
  };
  for (const ev of ["mousemove", "mousedown", "wheel"]) stage.addEventListener(ev, arm, { passive: true });
  document.addEventListener("keydown", arm);
  arm();
}

const copyStyles = (win) => {
  for (const link of document.querySelectorAll('link[rel="stylesheet"]')) win.document.head.appendChild(h("link", { rel: "stylesheet", href: link.href }));
};

/**
 * @param w       the watch ({ canvas, ownerName, ... })
 * @param onBack  called once the stream is back in the main window (window closed / fullscreen left)
 * @param want    "fullscreen" forces the fullscreen stage (double-click on a tile); omitted = PiP when supported
 * @param decorate optional `(stageEl) => dispose`: called once the stage exists, for what lies over the picture there (the stream's webcam in picture, task I10);
 *                `dispose` runs when the stream is back
 * @returns "pip" | "fullscreen"
 */
export async function popOut(w, onBack, want, decorate) {
  if (current) closePopout(current.w);
  if (stageWin) closeStage();
  const mode = want === "fullscreen" ? (document.fullscreenEnabled ? "fullscreen" : null) : popoutMode();
  if (!mode) throw new Error(t("ui.err.no_popout"));
  if (mode === "pip") {
    const win = await window.documentPictureInPicture.requestWindow({ width: 960, height: 540 });
    copyStyles(win);
    win.document.title = `${w.ownerName} — ${t("app.title")}`;
    win.document.documentElement.style.background = "#000";
    win.document.body.className = "pip-body";
    const stage = stageFor(w, win.document, () => win.close());
    win.document.body.appendChild(stage);
    current = { w, win, stage, kind: "pip", onBack, dispose: decorate ? decorate(stage) : null };
    win.addEventListener("pagehide", () => current && current.win === win && back(current.w)); // current.w: a rebind swaps the watch
    w.popped = "pip";
    return "pip";
  }
  const stage = stageFor(w, document, () => document.exitFullscreen().catch(() => {}));
  stage.style.zIndex = "200";
  autoHide(stage);
  stage.addEventListener("dblclick", () => current && closePopout(current.w)); // double-click again = back (Esc does the same natively)
  document.body.appendChild(stage);
  current = { w, stage, kind: "fullscreen", onBack, dispose: decorate ? decorate(stage) : null };
  const onFs = () => {
    if (document.fullscreenElement !== stage) {
      document.removeEventListener("fullscreenchange", onFs);
      if (current && current.stage === stage) back(current.w);
    }
  };
  document.addEventListener("fullscreenchange", onFs);
  w.popped = "fullscreen";
  try {
    await stage.requestFullscreen();
  } catch (e) {
    document.removeEventListener("fullscreenchange", onFs);
    back(w, { silent: true });
    throw e;
  }
  return "fullscreen";
}

/** The watch `oldW` ended (a reconnect) and `newW` took its place: move the new canvas into the still-open window. */
export function rebind(oldW, newW) {
  if (!current || current.w !== oldW) return false;
  const old = oldW.canvas;
  if (old && old.parentNode === current.stage) old.replaceWith(newW.canvas);
  else current.stage.prepend(newW.canvas);
  const nm = current.stage.querySelector(".nm");
  if (nm) nm.textContent = newW.ownerName;
  newW.popped = oldW.popped;
  oldW.popped = null;
  current.w = newW;
  return true;
}
/** Whether `w` has a pop-out window (or fullscreen stage) open. */
export const isPopped = (w) => !!current && current.w === w;

function back(w, { silent = false } = {}) {
  if (!current || current.w !== w) return;
  const c = current;
  current = null;
  if (c.dispose) c.dispose();
  c.stage.remove();
  w.popped = null;
  if (!silent && c.onBack) c.onBack();
}

/** Bring a popped stream back (closes the PiP window / leaves fullscreen). */
export function closePopout(w, { silent = false } = {}) {
  if (!current || current.w !== w) return;
  const c = current;
  if (c.kind === "pip") {
    back(w, { silent });
    try {
      c.win.close();
    } catch {}
  } else {
    back(w, { silent });
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  }
}

/* ---------------- the whole stage in its own window ---------------- */

/** Open the stage window (Document PiP only). Resolves `{ win, root }`: render the stage into `root`; `onBack(key)` runs once the window is gone. */
export async function popStage(key, onBack) {
  if (!pipSupported()) throw new Error(t("ui.err.no_popout"));
  if (current) closePopout(current.w);
  if (stageWin) closeStage();
  const win = await window.documentPictureInPicture.requestWindow({ width: 1280, height: 760 });
  copyStyles(win);
  win.document.title = t("app.title");
  win.document.documentElement.style.background = "#060708";
  win.document.body.className = "pip-body stage-body";
  const root = win.document.createElement("div");
  root.id = "app";
  root.className = "stage-win";
  win.document.body.appendChild(root);
  const sw = (stageWin = { win, root, key, onBack });
  win.addEventListener("pagehide", () => {
    if (stageWin !== sw) return;
    stageWin = null;
    onBack(key);
  });
  return sw;
}
/** Bring the stage back to the main window. */
export function closeStage() {
  const sw = stageWin;
  if (!sw) return;
  stageWin = null;
  try {
    sw.win.close();
  } catch {}
  sw.onBack(sw.key);
}
export const stageWindow = () => stageWin;
