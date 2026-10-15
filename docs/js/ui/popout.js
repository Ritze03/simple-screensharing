// Pop-out viewer. A browser page cannot open a free OS window like native does, so:
//   1. Document Picture-in-Picture (`documentPictureInPicture.requestWindow`, Chromium 116+): a real always-on-top
//      window. The tile's own <canvas> is moved into it, so the CanvasRenderer keeps drawing (no second decode).
//      Only one such window per page exists: popping out another stream brings the previous one back first.
//   2. Otherwise Fullscreen: the canvas moves into a fullscreen stage; leaving fullscreen brings it back.
// Both need the click's user activation, so call popOut() straight from a click handler.

import { h, ic } from "./dom.js";
import { t } from "./i18n.js";

let current = null; // { w, win?, stage, kind, onBack }

export const pipSupported = () => typeof window.documentPictureInPicture !== "undefined" && !!window.documentPictureInPicture.requestWindow;
export const popoutMode = () => (pipSupported() ? "pip" : document.fullscreenEnabled ? "fullscreen" : null);

function stageFor(w, doc, onClose) {
  return h("div", { class: "pip-stage" },
    w.canvas,
    h("div", { class: "ovl hb g2", style: "align-items:flex-start" },
      h("div", { class: "fl" }, h("div", { class: "hb g2 statbar" }, h("span", { class: "dot" }), h("span", { class: "txt nm", text: w.ownerName }))),
      h("button", { class: "round-btn", type: "button", title: t("ui.room.bring_back"), "aria-label": t("ui.room.bring_back"), onclick: onClose }, ic("move_down"))));
}

/**
 * @param w       the watch ({ canvas, ownerName, ... })
 * @param onBack  called once the stream is back in the main window (window closed / fullscreen left)
 * @param want    "fullscreen" forces the fullscreen stage (double-click on a tile); omitted = PiP when supported
 * @returns "pip" | "fullscreen"
 */
export async function popOut(w, onBack, want) {
  if (current) closePopout(current.w);
  const mode = want === "fullscreen" ? (document.fullscreenEnabled ? "fullscreen" : null) : popoutMode();
  if (!mode) throw new Error(t("ui.err.no_popout"));
  if (mode === "pip") {
    const win = await window.documentPictureInPicture.requestWindow({ width: 960, height: 540 });
    for (const link of document.querySelectorAll('link[rel="stylesheet"]')) {
      win.document.head.appendChild(h("link", { rel: "stylesheet", href: link.href }));
    }
    win.document.title = `${w.ownerName} — ${t("app.title")}`;
    win.document.documentElement.style.background = "#000";
    win.document.body.className = "pip-body";
    const stage = stageFor(w, win.document, () => win.close());
    win.document.body.appendChild(stage);
    current = { w, win, stage, kind: "pip", onBack };
    win.addEventListener("pagehide", () => back(w));
    w.popped = "pip";
    return "pip";
  }
  const stage = stageFor(w, document, () => document.exitFullscreen().catch(() => {}));
  stage.style.zIndex = "200";
  stage.addEventListener("dblclick", () => closePopout(w)); // double-click again = back (Esc does the same natively)
  document.body.appendChild(stage);
  current = { w, stage, kind: "fullscreen", onBack };
  const onFs = () => {
    if (document.fullscreenElement !== stage) {
      document.removeEventListener("fullscreenchange", onFs);
      back(w);
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

function back(w, { silent = false } = {}) {
  if (!current || current.w !== w) return;
  const c = current;
  current = null;
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
