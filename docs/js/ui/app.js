// Browser client UI: the room-centric layout of the approved mockup (and of the native egui app), driven by the
// browser core in ../core. State lives in store.js (S); rooms.js and capture.js turn core events into that state;
// render() rebuilds the DOM from S (cheap: a few hundred nodes) and keeps long-lived nodes (video canvases, preview
// images) by moving them into the new tree. Numbers that change every second (fps, kbps, meters) are patched in
// place instead of re-rendering, so sliders and hovers are never interrupted.

import { capabilities, createRoom, parseJoinLink, parseServerAddr, base64urlDecode, probeEncoders, ENCODER_PRESETS } from "../core/index.js";
import { S, onInvalidate, invalidate, toast, persistSettings, persistRooms, loadRooms, autoName, displayName } from "./store.js";
import { t, setLang, getLang } from "./i18n.js";
import {
  h, ic, T, V, H, SL, FILL, pct, ctrlRow, valBox, sliderRow, seg, cbx, card, roomRow, sharerRow, destRow, btnRow, emptyState,
  banner, meter, modalShell, avatarEl, LABEL_W,
} from "./dom.js";
import {
  RoomEntry, roomById, connectRoom, disconnectRoom, retryNow, watchStream, allWatches, addRoom, forgetRoom, applyJitterSettings,
  applyLevelLoudness,
} from "./rooms.js";
import {
  capById, capInRoom, liveRoomIds, allDests, allocated, applyBandwidth, captureName, captureRes, stage, discard, goLive,
  stopDest, editDest, captureLevel, changeSource, setAudioMuted,
} from "./capture.js";
import { popOut, closePopout, popStage as popoutStage, closeStage, stageWindow } from "./popout.js";
import { A } from "./actions.js";
import { WATCHED, roomView, viewKey, viewOf, vg, watchFps } from "./stage-state.js";
import { renderStage, renderPoppedNote, afterRender, isDragging, bindWindow, overlays, toggleStageFullscreen } from "./stage.js";
import { opener, selectField, FPS_LIST, PRESET_ORDER, presetLabel, presetCaption } from "./menus.js";

const caps = capabilities();
const CODECS = [["h264", "H.264"], ["av1", "AV1"], ["h265", "H.265"]];
let encoders = null; // Set of codecs this browser can encode (probeEncoders), null until probed

const root = document.getElementById("app");
const SCROLLERS = [".p-center", ".p-side-scroll", ".modal-body"];
let renderQueued = false; // a render asked for while a stage drag was in progress (drags update styles directly and hold renders back)
const HOST_MAIN = { id: "main", win: window, doc: document };
let live = []; // [{ el, fn }] text patched every stats tick
let meters = []; // [{ el, fn }] level meters, patched ~7x/s

const me = displayName;
const sel = () => roomById(S.selected);

/* ============================== render ============================== */

let namePromptShown = false;
/**
 * Offer the "Choose your name" modal once per page load while the user never chose a name, as soon as no other modal is
 * open (native `prompt_for_name`). Later / Escape / x / backdrop only close it; Save (or the settings field) ends it for good.
 */
function promptForName() {
  if (namePromptShown || S.modal || S.settings.nameSet) return;
  namePromptShown = true;
  S.modal = { kind: "choose-name", value: displayName(), focus: true };
}

/** The view the central area shows as a stage: Watched streams, or the selected room while it is joined (else the scrolling body). */
function stageViewNow() {
  if (S.view === "watching") return WATCHED;
  const r = sel();
  return r && r.state === "joined" ? roomView(r) : null;
}

// A slider being dragged must not be replaced under the pointer: renders wait until the pointer is up (like a stage drag).
let sliderHeld = false;
document.addEventListener("pointerdown", (e) => { if (e.target.matches && e.target.matches('input[type="range"]')) sliderHeld = true; }, true);
window.addEventListener("pointerup", () => {
  if (!sliderHeld) return;
  sliderHeld = false;
  A.flush();
}, true);

function render() {
  if (isDragging() || sliderHeld) {
    renderQueued = true;
    return;
  }
  renderQueued = false;
  promptForName();
  const keep = SCROLLERS.map((s) => {
    const n = root.querySelector(s);
    return n ? n.scrollTop : null;
  });
  const act = document.activeElement;
  const focusedId = act && root.contains(act) ? act.id : null;
  const caret = act && focusedId && typeof act.selectionStart === "number" ? [act.selectionStart, act.selectionEnd] : null;
  live = [];
  meters = [];
  document.documentElement.lang = getLang();
  document.title = S.view === "room" && sel() ? `${sel().name} — ${t("app.title")}` : t("app.title");

  const sv = stageViewNow();
  if (S.stage.fullscreen && !sv) {
    S.stage.fullscreen = false; // the room went away under a fullscreen stage
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
  }
  // Fullscreen: only the stage, its strip and bar lying over the streams (they hide by themselves, chrome.js). Modals, menus and toasts stay.
  const fsStage = S.stage.fullscreen && sv && !S.stage.popped.has(viewKey(sv));
  const app = fsStage
    ? V({ cls: "p-app fs-root", g: 0 }, renderStage(sv, HOST_MAIN, { fullscreen: true }))
    : H({ cls: "p-app st", g: 0 }, S.stage.sidebarHidden && sv ? null : renderSide(), renderMain(sv));
  if (S.modal) app.appendChild(renderModal());
  app.appendChild(h("div", { class: "layer" }, overlays("main")));
  app.appendChild(V({ cls: "toasts", g: 1, attrs: { role: "status", "aria-live": "polite" } },
    S.toasts.map((x) => H({ cls: "toast" + (x.err ? " err" : ""), g: 2 }, ic(x.icon), T(x.text)))));
  app.classList.toggle("has-menu", !!(S.menu || S.modal));
  root.replaceChildren(app);
  root.classList.toggle("stage-fs", !!fsStage);

  SCROLLERS.forEach((s, i) => {
    const n = root.querySelector(s);
    if (n && keep[i] != null) n.scrollTop = keep[i];
  });
  if (focusedId) {
    const f = document.getElementById(focusedId);
    if (f) {
      f.focus();
      if (caret && typeof f.setSelectionRange === "function") try { f.setSelectionRange(caret[0], caret[1]); } catch {}
    }
  }
  if (S.modal && S.modal.focus) {
    // A modal that wants its field focused with the text selected (once, when it opens).
    S.modal.focus = false;
    const f = root.querySelector(".modal input.field");
    if (f) {
      f.focus();
      f.select();
    }
  }
  root.querySelectorAll('input[type="range"]').forEach(pct);
  renderPop();
  patchLive();
  const sw = stageWindow();
  afterRender(sw ? [window, sw.win] : [window]);
}
/** The whole stage in its own (Document PiP) window: the same stage tree rendered into that window's document. */
function renderPop() {
  const sw = stageWindow();
  if (!sw) return;
  const view = viewOf(sw.key);
  if (!view || (view.kind === "room" && view.r.state === "disconnected")) {
    setTimeout(closeStage, 0); // the room is gone (left, forgotten): the stage comes home
    return;
  }
  const host = { id: "pop", win: sw.win, doc: sw.win.document };
  const away = view.kind === "room" && view.r.state !== "joined"; // suspended: the window stays, the stage is back with the room
  const body = away
    ? V({ cls: "stg eq minh0", g: 0 }, h("div", { class: "arena" }, h("div", { class: "arena-note" }, ic("sync", "big"), h("b", { text: t("ui.sidebar.status_reconnecting") }))))
    : renderStage(view, host);
  sw.root.replaceChildren(body, h("div", { class: "layer" }, overlays("pop")));
}
/** Run a render that was held back by a drag. */
A.flush = () => {
  if (renderQueued) render();
};
onInvalidate(render);
window.addEventListener("resize", () => invalidate());

function liveText(el, fn) {
  live.push({ el, fn });
  return el;
}
function patchLive() {
  for (const { el, fn } of live) {
    const v = fn();
    if (el.textContent !== v) el.textContent = v;
  }
}
window.addEventListener("sss-stats", patchLive);
setInterval(patchLive, 1000);
setInterval(() => {
  for (const { el, fn } of meters) {
    const level = fn();
    el.querySelectorAll("i").forEach((b, i) => {
      const lit = level > i / 5 + 0.02;
      b.classList.toggle("lit", lit);
      b.classList.toggle("hot", lit && i === 4);
    });
  }
}, 140);
function liveMeter(fn) {
  const el = meter("live");
  meters.push({ el, fn });
  return el;
}

function roomStatusTitle(r) {
  return { joined: t("ui.sidebar.status_connected"), reconnecting: t("ui.sidebar.status_reconnecting"), connecting: t("ui.room.connecting"), disconnected: t("ui.sidebar.status_disconnected") }[r.state];
}

/* ============================== sidebar ============================== */

function renderSide() {
  const liveIds = liveRoomIds();
  const watches = allWatches();
  const rows = [];
  for (const r of S.rooms) {
    const joined = r.state === "joined";
    const mine = capInRoom(r.id);
    const vids = r.videoStreams();
    const nShares = joined ? vids.length + (mine ? 1 : 0) : null;
    rows.push(roomRow({
      lead: h("span", { class: "sdot " + (r.state === "disconnected" && r.error ? "failed" : r.state) }), name: r.name,
      me: liveIds.includes(r.id) ? ic("cast", "ico live") : null,
      num: nShares || null, unit: nShares ? ic("screen_share", "unit-ico") : null,
      sel: S.view === "room" && S.selected === r.id, title: roomStatusTitle(r),
      onclick: () => select(r),
    }));
    if (!joined) continue;
    if (mine) {
      const d = mine.dests.get(r.id);
      rows.push(sharerRow({ icon: ic("cast", "ico live"), name: t("ui.sidebar.you"), num: d.fps, unit: "fps", title: t("ui.sidebar.you_tip"), onclick: () => { select(r); openStreamModal(mine.id, r.id); } }));
    }
    for (const s of vids) {
      const w = r.watches.get(s.key);
      rows.push(sharerRow({
        icon: ic(w ? "visibility" : "screen_share", "ico" + (w ? " watch" : "")), name: s.ownerName || r.memberName(s.owner), num: null, unit: w ? "fps" : "",
        title: s.label || null, numAttrs: null,
        onclick: () => { select(r, false); flash(s.key); },
      }));
      if (w) liveText(rows[rows.length - 1].querySelector(".num"), () => watchFps(w));
    }
  }
  const nLive = liveIds.length;
  return V({ tag: "aside", cls: "p-side", g: 0 },
    H({ cls: "band p-brand", g: 2 }, h("span", { class: "mark", "aria-hidden": "true" }), T(t("app.title"))),
    V({ cls: "p-side-scroll", g: 2 },
      V({ cls: "well", g: 0 },
        roomRow({ lead: ic("grid_view", "ico"), name: t("ui.sidebar.watched_streams"), me: false, num: watches.length, unit: ic("visibility", "unit-ico"), sel: S.view === "watching",
          title: t("ui.sidebar.watched_streams_tip"), onclick: () => { S.view = "watching"; render(); } })),
      H({ g: 2, style: "margin-top:var(--s1)" }, h("h2", { class: "section-label" }, t("ui.sidebar.rooms"), h("span", { class: "count", text: String(S.rooms.length) })), h("span", { class: "rule" })),
      S.rooms.length ? V({ cls: "well", g: 0, attrs: { role: "list" } }, rows) : h("div", { class: "side-empty", text: t("ui.sidebar.no_rooms") }),
      H({ g: 0 }, FILL(h("button", { class: "btn btn-ghost wide", type: "button", id: "btn-create-join", onclick: () => openAdd() }, ic("add"), t("ui.sidebar.create_or_join"))))),
    H({ cls: "band bottom p-user", g: 2 },
      SL(32, avatarEl(me(), S.settings.emoji, { pres: S.rooms.some((r) => r.state === "joined") })),
      FILL(V({ cls: "who", g: 0, attrs: { role: "button", tabindex: "0", title: t("ui.sidebar.outgoing_tip"), ...opener("outgoing") } },
        T(me(), "b"), h("span", { class: "txt" + (nLive ? " live" : ""), text: nLive ? (nLive === 1 ? t("ui.sidebar.live_in_one_room") : t("ui.sidebar.live_in_rooms", { count: nLive })) : t("ui.sidebar.not_sharing") }))),
      SL(32, h("button", { class: "icon-btn", type: "button", id: "btn-settings", title: t("ui.sidebar.settings"), "aria-label": t("ui.sidebar.settings"), onclick: () => { S.modal = { kind: "settings" }; render(); } }, ic("settings")))));
}

function select(r, rerender = true) {
  S.view = "room";
  S.selected = r.id;
  S.menu = null;
  if (rerender) render();
}
function flash(key) {
  S.flash = key;
  render();
  setTimeout(() => {
    if (S.flash === key) {
      S.flash = null;
      render();
    }
  }, 1200);
}

/* ============================== main ============================== */

function capsBanner() {
  if (caps.canWatch) return null;
  return banner("block", t("ui.err.browser_unsupported"), null, { error: true });
}

/** The stage region: the stage itself, or a note with "Bring it back" while it is open in its own window. */
function stageRegion(view) {
  return S.stage.popped.has(viewKey(view)) ? renderPoppedNote(view) : renderStage(view, HOST_MAIN);
}

function renderMain(sv) {
  const cb = capsBanner();
  const cbRow = cb ? H({ cls: "p-banner", g: 0 }, FILL(cb)) : null;
  if (S.view === "watching") {
    return V({ tag: "main", cls: "eq", g: 0 },
      H({ tag: "header", cls: "band top p-head", g: 2 }, h("h2", { text: t("ui.sidebar.watched_streams") }), T(t("ui.sidebar.watched_streams_tip"), "meta")), cbRow, stageRegion(sv));
  }
  const r = sel();
  if (!r) {
    return V({ tag: "main", cls: "eq", g: 0 },
      H({ tag: "header", cls: "band top p-head", g: 2 }, h("h2", { text: t("app.title") })),
      H({ cls: "eq st minh0", g: 0 },
        V({ cls: "p-center eq", g: 4 }, cb,
          emptyState("meeting_room", t("ui.home.title"), t("ui.home.text"),
            h("button", { class: "btn btn-primary btn-lg wide", type: "button", id: "btn-home-add", onclick: () => openAdd() }, ic("add"), t("ui.sidebar.create_or_join"))))));
  }
  if (sv) return V({ tag: "main", cls: "eq", g: 0 }, renderHead(r), cbRow, stageRegion(sv));
  // a room with no stage (not connected, connecting, reconnecting): the scrolling body
  const center = V({ cls: "p-center eq", g: 4 });
  if (cb) center.appendChild(cb);
  if (r.state === "disconnected") {
    center.appendChild(emptyState("cloud_off", t("ui.room.not_connected_title", { room: r.name }), r.error ? null : t("ui.room.not_connected_text"),
      btnRow(h("button", { class: "btn btn-primary btn-lg", type: "button", id: "btn-connect", onclick: () => connectRoom(r) }, ic("login"), t("ui.room.connect")),
        h("button", { class: "btn btn-danger btn-lg", type: "button", onclick: () => { S.modal = { kind: "forget", room: r.id }; render(); } }, ic("delete"), t("ui.room.forget"))),
      { err: !!r.error, errText: r.error }));
  } else if (r.state === "connecting") {
    center.appendChild(emptyState("sync", t("ui.room.connecting_title", { room: r.name }), t("ui.room.connecting_text")));
  } else {
    // The browser cannot tell a stale certificate hash from a relay that is down, so the expiry hint only shows
    // once a few attempts have failed (a relay restart is back within one or two).
    const issue = r.issue; // "unreachable" | "full" | null (null: a connection was just lost)
    const attempt = Math.max(1, r.attempt);
    center.appendChild(banner(issue ? "link_off" : "sync_problem",
      issue === "unreachable" ? t("ui.room.reconnect_unreachable", { attempt }) : issue === "full" ? t("ui.room.reconnect_full", { attempt }) : t("ui.room.reconnecting_banner", { attempt }),
      h("button", { class: "btn btn-secondary", type: "button", onclick: () => retryNow(r) }, t("ui.room.retry_now")), { error: !!issue }));
    center.appendChild(emptyState("tv_off", t("ui.room.no_shares_now"), t("ui.room.shares_come_back")));
  }
  return V({ tag: "main", cls: "eq", g: 0 }, renderHead(r), H({ cls: "eq st minh0", g: 0 }, center));
}

/** The room's header band: title, status pill, meta. Since the stage the controls live in its bar; a room with no stage keeps a `⋯` for the Room settings menu. */
function renderHead(r) {
  const status = r.state === "joined" ? h("span", { class: "badge badge-live" }, h("span", { class: "dot" }), t("ui.sidebar.status_connected"))
    : r.state === "reconnecting" ? h("span", { class: "badge badge-degraded" }, ic("sync"), t("ui.sidebar.status_reconnecting"))
      : r.state === "connecting" ? h("span", { class: "badge badge-progress" }, h("span", { class: "bar" }), t("ui.room.connecting"))
        : h("span", { class: "badge badge-muted" }, t("ui.sidebar.status_disconnected"));
  const nSharing = r.videoStreams().length + (capInRoom(r.id) ? 1 : 0);
  return H({ tag: "header", cls: "band top p-head", g: 2 },
    H({ g: 3, cls: "eq" }, h("h2", { text: r.name, title: r.name }), status,
      r.members.length ? T(t("ui.room.meta", { people: r.members.length, sharing: nSharing }), "meta") : null),
    r.state === "joined" ? null
      : H({ g: 2 }, h("button", { class: "icon-btn bordered", type: "button", id: "btn-room-menu", title: t("ui.room.options"), "aria-label": t("ui.room.options"), ...opener("room", "room", { roomId: r.id }) }, ic("more_horiz"))));
}

async function doPopOut(r, w) {
  try {
    await popOut(w, () => invalidate());
  } catch (e) {
    toast(t("ui.toast.popout_failed", { error: e.message }), "error", { err: true });
  }
  render();
}
/** Double-click on a tile: the stream fills the screen. `Why` a body-level stage (popout.js, mode "fullscreen") and not
 *  tile.requestFullscreen(): render() replaces the whole tree on every state change, which would drop a fullscreen tile. */
async function doFullscreen(r, w) {
  try {
    if (w.popped === "fullscreen") closePopout(w);
    else if (!w.popped) await popOut(w, () => invalidate(), "fullscreen");
  } catch (e) {
    toast(t("ui.toast.popout_failed", { error: e.message }), "error", { err: true });
  }
  render();
}
function watchAndPop(r, key) {
  const w = watchStream(r, key);
  if (w) doPopOut(r, w);
}
/** The whole stage into its own window (Document PiP), or, with `back`, home again. Needs the click's user activation. */
async function popStage(key, back) {
  if (back) return closeStage();
  try {
    const sw = await popoutStage(key, () => {
      S.stage.popped.delete(key);
      if (S.menu && S.menu.hostId === "pop") S.menu = null;
      S.card = null;
      invalidate();
    });
    bindWindow(sw.win, "pop");
    S.stage.popped.add(key);
    if (S.stage.fullscreen) toggleStageFullscreen(); // its own window cannot share the fullscreen of the main one
  } catch (e) {
    console.error("stage pop-out failed:", e);
    toast(t("ui.toast.popout_failed", { error: e.message }), "error", { err: true });
  }
  render();
}
/** Leave a room: the confirm modal when something would be lost (an outgoing share or a live watch, native rule), else at once. */
async function leaveRoom(r) {
  if (capInRoom(r.id) || r.watches.size) {
    S.menu = null;
    S.modal = { kind: "leave", room: r.id };
    return render();
  }
  await disconnectRoom(r);
  toast(t("ui.toast.left", { room: r.name }), "logout");
  render();
}

async function startStage(r) {
  S.menu = null;
  const c = await stage(r);
  if (c && S.view === "room" && S.selected === r.id && !S.modal) openStreamModal(c.id, r.id);
}

/* ============================== stream settings modal ============================== */

function openStreamModal(capId, roomId) {
  S.menu = null;
  S.modal = { kind: "stream", cap: capId, room: roomId };
  render();
}

function renderStreamModal(M, close) {
  const c = capById(M.cap);
  const r = roomById(M.room);
  if (!c || !r) {
    S.modal = null;
    return h("div");
  }
  const staged = c.state !== "live";
  const d = staged ? c.staged : c.dests.get(r.id);
  if (!d) {
    S.modal = null;
    return h("div");
  }
  const edit = (patch) => {
    if (staged) {
      Object.assign(c.staged, patch);
      render();
    } else editDest(c, r, patch);
  };

  // Source: the Change button is there while setting up and while live (a live change restarts the share on the new source)
  const audioCell = h("span", { class: "val center" }, liveMeter(() => captureLevel(c)));
  const source = card(t("ui.stream.source"), null,
    ctrlRow(t("ui.stream.screen"), T(`${captureName(c)}${captureRes(c) ? " · " + captureRes(c) : ""}`, "ro"),
      h("button", { class: "btn btn-secondary wide", type: "button", id: "btn-change-source", onclick: () => changeSource(c) }, t("ui.stream.change"))),
    ctrlRow(t("ui.stream.audio"), T(c.audio ? t("ui.stream.audio_from_source") : t("ui.stream.audio_none"), "ro"), audioCell, t("ui.stream.audio_tip")),
    ctrlRow(null, cbx(t("ui.stream.mute_audio"), c.audioMuted, () => setAudioMuted(c, !c.audioMuted), { disabled: !c.audio })));

  // Destinations
  const rows = staged ? [[r.id, null]] : [...c.dests.entries()];
  const dests = card(staged ? t("ui.stream.goes_live_in") : t("ui.stream.sending_to"), staged ? t("ui.stream.one_room") : c.dests.size === 1 ? t("ui.stream.one_room") : t("ui.stream.n_rooms", { count: c.dests.size }),
    V({ g: 1 }, rows.map(([rid, dd]) => {
      const rr = roomById(rid);
      return destRow({
        cur: rid === r.id, name: rr ? rr.name : "?", curLabel: t("ui.stream.this_room"),
        pill: !dd ? h("span", { class: "badge badge-muted" }, t("ui.stream.not_live"))
          : dd.state === "live" ? h("span", { class: "badge badge-live" }, h("span", { class: "dot" }), t("ui.room.live_pill"))
            : h("span", { class: "badge badge-progress" }, h("span", { class: "bar" }), dd.state === "moving" ? t("ui.bar.moving") : dd.state === "restarting" ? t("ui.bar.applying") : t("ui.bar.starting")),
        kbps: dd ? `${allocated(dd)} kbps` : "—",
        action: dd ? h("button", { class: "icon-btn sm", type: "button", title: t("ui.stream.stop_sending_to", { room: rr ? rr.name : "?" }), "aria-label": t("ui.stream.stop_sending_to", { room: rr ? rr.name : "?" }),
          onclick: () => { const last = c.dests.size === 1; stopDest(c, rid); if (last) S.modal = null; else if (rid === M.room) S.modal.room = [...c.dests.keys()][0]; render(); } }, ic("close")) : null,
      });
    })),
    staged ? null : btnRow(h("button", { class: "btn btn-secondary", type: "button", ...opener("alsosend", "alsosend", { roomId: r.id, cap: c.id }) }, ic("add"), t("ui.stream.also_send")),
      h("button", { class: "btn btn-secondary", type: "button", ...opener("move", "move", { roomId: r.id, cap: c.id }) }, ic("swap_horiz"), t("ui.stream.move_to"))));

  // Upload bandwidth
  const bw = S.settings.bw;
  const n = allDests().length + (staged ? 1 : 0);
  const commitBw = () => { persistSettings(); applyBandwidth(); render(); };
  const bwRows = [ctrlRow(t("bandwidth.mode_label"), seg([["per_stream", t("bandwidth.mode_per_stream")], ["absolute", t("bandwidth.mode_absolute")]], bw.mode, (v) => { bw.mode = v; commitBw(); }))];
  if (bw.mode === "absolute") {
    bwRows.push(sliderRow(t("bandwidth.total_label"), "st-total", 2000, 40000, 500, () => bw.total, (v) => { bw.total = v; }, String, t("ui.stream.total_tip"), commitBw));
    bwRows.push(sliderRow(t("bandwidth.floor_label"), "st-floor", 500, 10000, 250, () => bw.floor, (v) => { bw.floor = v; }, String, t("ui.stream.floor_tip"), commitBw));
    if (bw.floor * n > bw.total) bwRows.push(banner("warning", t("ui.stream.over_budget", { n, floor: bw.floor, total: bw.total })));
    else bwRows.push(ctrlRow(t("ui.stream.per_stream"), h("span", { class: "ro txt", style: "width:100%;text-align:right", text: n === 1 ? t("ui.stream.one_stream_budget") : t("ui.stream.n_streams_budget", { count: n }) }),
      valBox(String(Math.max(bw.floor, Math.round(bw.total / Math.max(1, n) / 100) * 100)))));
  }
  const bwCard = card(t("ui.stream.upload_bandwidth"), t("ui.stream.all_streams"), bwRows);

  // Compression: codec and bitrate. A codec this browser cannot encode does nothing when picked: its button is not drawn (#93, was greyed
  // out). Not before the probe. A staged config whose codec is not encodable (an old or edited stored default) starts with the first
  // encodable one instead, on the staged copy only: the stored default in S.settings.stream is never rewritten.
  const codecOpts = CODECS.filter(([v]) => !encoders || encoders.has(v));
  if (staged && encoders && encoders.size && !encoders.has(d.codec)) d.codec = codecOpts[0][0];
  const compression = card(t("ui.stream.compression"), null,
    // A single encodable codec leaves nothing to choose: no row.
    codecOpts.length <= 1 && encoders ? null : ctrlRow(t("ui.stream.codec"), seg(codecOpts, d.codec, (v) => edit({ codec: v }))),
    bw.mode === "per_stream"
      ? sliderRow(t("ui.stream.bitrate"), "st-kbps", 500, 20000, 250, () => d.kbps, (v) => { d.kbps = v; }, String, t("ui.stream.bitrate_tip"), () => edit({ kbps: d.kbps }))
      : ctrlRow(t("ui.stream.bitrate"), h("span", { class: "ro txt", style: "width:100%;text-align:right", text: t("ui.stream.set_by_budget") }), valBox(String(staged ? Math.max(bw.floor, Math.round(bw.total / Math.max(1, n) / 100) * 100) : allocated(d))), t("ui.stream.budget_tip")));

  // Video: FPS (a dropdown in the app's own menu style), the preset and what it sets. A preset sets the FPS once; changing the FPS afterwards
  // keeps its encoding mode and keyframe interval, so the caption is always "fps · mode · keyframe", never "(custom)".
  const fpsSel = selectField({ id: "st-fps", opts: FPS_LIST.map((f) => [f, t("ui.stream.fps", { fps: f })]), cur: d.fps, onPick: (v) => edit({ fps: Number(v) }), title: t("share.framerate_label") });
  const video = card(t("ui.stream.video"), null,
    ctrlRow(t("ui.stream.fps_label"), fpsSel, undefined, null, "st-fps"),
    ctrlRow(t("ui.stream.preset"), seg(PRESET_ORDER.map((p) => [p, presetLabel(p)]), d.preset, (v) => edit({ preset: v, fps: ENCODER_PRESETS[v].fps }))),
    H({ g: 2 }, SL(LABEL_W, null), FILL(h("span", { class: "cap", id: "preset-cap", text: presetCaption(d.preset, d.fps) }))));

  const body = H({ g: 3, cls: "st", style: "align-items:flex-start" },
    V({ cls: "eq", g: 3 }, source, dests, bwCard),
    V({ cls: "eq", g: 3 }, compression, video, staged ? null : h("span", { class: "hint", text: t("ui.stream.restart_note") })));
  const applying = !staged && d.state === "restarting";
  const foot = staged
    ? [h("button", { class: "btn btn-secondary btn-lg", type: "button", onclick: () => discard(c) }, t("ui.bar.discard")), FILL(null),
      h("button", { class: "btn btn-secondary btn-lg", type: "button", onclick: close }, t("ui.stream.later")),
      h("button", { class: "btn btn-primary btn-lg", type: "button", id: "btn-modal-go-live", disabled: c.state !== "ready" || r.state !== "joined", onclick: () => { S.modal = null; goLive(c, r); render(); } }, ic("cast"), t("ui.stream.go_live_in", { room: r.name }))]
    : [h("button", { class: "btn btn-danger btn-lg", type: "button", onclick: () => { S.modal = null; stopDest(c, r.id); toast(t("ui.toast.stopped_in", { room: r.name }), "stop_circle"); } }, ic("stop_screen_share"), t("ui.stream.stop_in", { room: r.name })),
      FILL(null), H({ cls: "note", g: 1 }, ic("check_circle"), t("ui.stream.saved")),
      h("button", { class: "btn btn-primary btn-lg", type: "button", onclick: close }, t("ui.stream.done"))];
  return modalShell("xwide", staged ? t("ui.stream.title_staged") : t("ui.stream.title_live"),
    staged ? t("ui.stream.sub_staged", { screen: captureName(c), room: r.name }) : t("ui.stream.sub_live", { room: r.name }), close, body, foot,
    SL(96, applying ? h("span", { class: "badge badge-progress lg" }, h("span", { class: "bar" }), t("ui.bar.applying")) : null, { al: "r", min: true }), t("ui.close"));
}

/* ============================== modals ============================== */

function openAdd(tab = "join") {
  S.menu = null;
  S.modal = { kind: "add", tab, name: "", link: "", server: S.settings.lastServer, hash: S.settings.lastCertHash, error: null, minted: null };
  render();
}

function renderModal() {
  const M = S.modal;
  const close = () => { S.modal = null; render(); };
  const scrim = V({ cls: "scrim", g: 0, attrs: { onmousedown: (e) => { if (e.target === scrim && !S.menu) close(); } } });
  let modal;
  const lgBtn = (cls, label, onclick, icon, disabled, id) => h("button", { class: "btn btn-lg " + cls, type: "button", disabled: !!disabled, onclick, id: id || null }, icon ? ic(icon) : null, label);
  const spacer = () => FILL(null);
  if (M.kind === "stream") modal = renderStreamModal(M, close);
  else if (M.kind === "settings") modal = renderSettings(close, lgBtn, spacer);
  else if (M.kind === "add") modal = renderAdd(M, close, lgBtn, spacer);
  else if (M.kind === "leave" || M.kind === "forget") {
    const r = roomById(M.room);
    if (!r) { S.modal = null; return h("div"); }
    const forget = M.kind === "forget";
    const c = capInRoom(r.id);
    const w = r.watches.size;
    modal = modalShell("", forget ? t("ui.modal.forget_title", { room: r.name }) : t("ui.modal.leave_title", { room: r.name }), null, close, [
      h("p", { text: forget ? t("ui.modal.forget_text") : t("ui.modal.leave_text") }),
      c ? banner("warning", t("ui.modal.stream_stops", { room: r.name }) + (c.dests.size > 1 ? " " + t("ui.modal.keeps_going") : "")) : null,
      w ? banner("warning", w === 1 ? t("ui.modal.stop_watching_one") : t("ui.modal.stop_watching_n", { count: w })) : null,
    ], [spacer(), lgBtn("btn-secondary", t("ui.modal.cancel"), close), lgBtn("btn-danger-solid", forget ? t("ui.room.forget") : t("ui.modal.leave_room"), async () => {
      S.modal = null;
      if (forget) {
        await forgetRoom(r);
        if (S.selected === r.id) S.selected = S.rooms[0] ? S.rooms[0].id : null;
        toast(t("ui.toast.forgot", { room: r.name }), "delete");
      } else {
        await disconnectRoom(r);
        toast(t("ui.toast.left", { room: r.name }), "logout");
      }
      render();
    }, null, false, "btn-confirm")], null, t("ui.close"));
  } else if (M.kind === "choose-name") {
    const save = () => {
      const v = (M.value || "").trim();
      if (!v) return;
      S.settings.name = v;
      S.settings.nameSet = true;
      persistSettings();
      S.modal = null;
      render();
    };
    const inp = h("input", { class: "field", id: "choose-name-in", value: M.value, maxlength: "64", autocomplete: "off", spellcheck: "false", "aria-label": t("ui.modal.name") });
    const saveBtn = lgBtn("btn-primary", t("ui.modal.save"), save, null, !(M.value || "").trim(), "btn-choose-name-save");
    inp.addEventListener("input", () => { M.value = inp.value; saveBtn.disabled = !inp.value.trim(); });
    inp.addEventListener("keydown", (e) => e.key === "Enter" && save());
    modal = modalShell("", t("ui.modal.choose_name_title"), null, close, [h("p", { text: t("ui.modal.choose_name_body") }), card(null, null, ctrlRow(t("ui.modal.name"), inp, undefined, undefined, "choose-name-in"))],
      [spacer(), lgBtn("btn-secondary", t("ui.stream.later"), close, null, false, "btn-choose-name-later"), saveBtn], null, t("ui.close"));
  } else if (M.kind === "rename") {
    const r = roomById(M.room);
    if (!r) { S.modal = null; return h("div"); }
    const save = () => { r.name = (M.value || "").trim() || r.name; persistRooms(); S.modal = null; render(); };
    const inp = h("input", { class: "field", id: "rename-in", value: M.value, "aria-label": t("ui.modal.room_name") });
    inp.addEventListener("input", () => { M.value = inp.value; });
    inp.addEventListener("keydown", (e) => e.key === "Enter" && save());
    modal = modalShell("", t("ui.modal.rename_title"), null, close, [card(null, null, ctrlRow(t("ui.modal.name"), inp, undefined, t("ui.modal.room_name_tip"), "rename-in"))],
      [spacer(), lgBtn("btn-secondary", t("ui.modal.cancel"), close), lgBtn("btn-primary", t("ui.modal.save"), save)], null, t("ui.close"));
  }
  scrim.appendChild(modal);
  return scrim;
}

function renderSettings(close, lgBtn, spacer) {
  const st = S.settings;
  const nameIn = h("input", { class: "field", id: "set-name", value: st.name, placeholder: autoName(), maxlength: "64", "aria-label": t("join.display_name_label") });
  nameIn.addEventListener("input", () => { st.name = nameIn.value; if (st.name.trim()) st.nameSet = true; persistSettings(); });
  nameIn.addEventListener("change", () => render());
  const commit = () => { persistSettings(); applyJitterSettings(); };
  return modalShell("wide", t("settings.header"), null, close, [
    card(t("ui.settings.profile"), null, ctrlRow(t("join.display_name_label"), nameIn, undefined, t("ui.settings.name_tip"), "set-name"),
      h("span", { class: "hint", text: t("ui.settings.name_note") }),
      // The avatar: an emoji instead of the initial, on a colour taken from the emoji. A browser has system emoji fonts, so any emoji can be pasted.
      ctrlRow(t("ui.settings.avatar"), h("button", { class: "btn btn-secondary", type: "button", id: "btn-avatar", title: t("ui.settings.avatar_tip"), ...opener("emoji", "emoji") },
        avatarEl(displayName(), st.emoji, { sm: true }), st.emoji ? t("ui.settings.avatar_change") : t("ui.settings.avatar_pick")), undefined, t("ui.settings.avatar_tip"))),
    card(t("settings.language_label"), null, ctrlRow(t("settings.language_label"), seg([["en", "English"], ["de", "Deutsch"]], st.lang, (v) => { st.lang = v; setLang(v); persistSettings(); render(); }))),
    card(t("ui.settings.playback"), null,
      sliderRow(t("ui.settings.jitter"), "set-jitter", 0, 5000, 100, () => st.jitter, (v) => { st.jitter = v; }, (v) => (v / 1000).toFixed(1) + " s", t("ui.settings.jitter_tip"), commit),
      sliderRow(t("ui.settings.catchup"), "set-catchup", 5, 10, 1, () => st.catchup, (v) => { st.catchup = v; }, (v) => v + "%", t("ui.settings.catchup_tip"), commit),
      ctrlRow(t("ui.settings.level_loudness"), cbx(t("ui.settings.level_loudness_on"), st.levelLoudness, () => { st.levelLoudness = !st.levelLoudness; persistSettings(); applyLevelLoudness(); render(); }, { id: "set-level-loudness" }), undefined, t("ui.settings.level_loudness_tip"), "set-level-loudness")),
  ], [spacer(), lgBtn("btn-primary", t("ui.stream.done"), close, null, false, "btn-settings-done")], null, t("ui.close"));
}

function renderAdd(M, close, lgBtn, spacer) {
  let body, foot;
  const tabs = H({ g: 0 }, FILL(seg([["join", t("ui.modal.join_tab")], ["create", t("ui.modal.create_tab")]], M.tab, (v) => { M.tab = v; M.minted = null; M.error = null; render(); })));
  const err = M.error ? h("span", { class: "hint err", text: M.error }) : null;
  // Inline server-address check (task #84): shown under the link / server field, disables the primary button.
  const hintNode = h("span", { class: "hint err", id: "add-hint", role: "alert" });
  const curHint = () => (M.tab === "join" ? joinLinkHint(M.link) : serverHint(M.server));
  const syncHint = () => {
    const x = curHint();
    hintNode.textContent = x || "";
    hintNode.hidden = !x;
    const b = document.getElementById(M.tab === "join" ? "btn-join" : "btn-create");
    if (b) b.disabled = !!x;
  };
  const blocked = !!curHint();
  syncHint();
  const field = (id, key, placeholder, label, cls, tip) => {
    const inp = h("input", { class: "field" + (cls ? " " + cls : ""), id, placeholder, value: M[key] || "", "aria-label": label, spellcheck: "false", autocomplete: "off" });
    inp.addEventListener("input", () => {
      M[key] = inp.value;
      if (key === "link" || key === "server") syncHint();
      if (key === "link" && prefillName(M)) {
        const n = document.getElementById("add-name");
        if (n) n.value = M.name;
      }
    });
    inp.addEventListener("keydown", (e) => e.key === "Enter" && submit());
    return ctrlRow(label, inp, undefined, tip, id);
  };
  const submit = () => {
    if (M.tab === "join") return doJoin(M);
    if (!M.minted) return doCreate(M);
    return openMinted(M);
  };
  if (M.tab === "join") {
    body = [tabs, card(null, null, field("add-link", "link", "relay.example.com:4433|…|…|…", t("ui.modal.join_link"), "mono"), hintNode, field("add-name", "name", t("ui.modal.name_placeholder"), t("ui.modal.room_name"), null, t("ui.modal.room_name_tip")), err)];
    foot = [spacer(), lgBtn("btn-secondary", t("ui.modal.cancel"), close), lgBtn("btn-primary", t("ui.modal.join"), () => doJoin(M), "login", blocked, "btn-join")];
  } else if (!M.minted) {
    body = [tabs, card(null, null,
      field("add-cname", "name", t("ui.modal.name_placeholder"), t("ui.modal.room_name"), null, t("ui.modal.room_name_tip")),
      field("add-server", "server", "relay.example.com:4433", t("ui.modal.server"), "mono", t("ui.modal.server_tip")), hintNode,
      field("add-hash", "hash", t("ui.modal.hash_placeholder"), t("ui.modal.hash"), "mono", t("ui.modal.hash_tip")), err)];
    foot = [spacer(), lgBtn("btn-secondary", t("ui.modal.cancel"), close), lgBtn("btn-primary", t("ui.modal.create"), () => doCreate(M), "add", blocked, "btn-create")];
  } else {
    body = [tabs, h("p", { text: t("ui.modal.created_text") }),
      card(null, null,
        ctrlRow(t("ui.modal.room_id"), T(M.minted.roomIdHex, "ro")),
        ctrlRow(t("ui.modal.join_link"), T(M.minted.link, "ro"), h("button", { class: "btn btn-secondary wide", type: "button", onclick: () => clip(M.minted.link) }, t("ui.modal.copy"))))];
    foot = [spacer(), lgBtn("btn-primary", t("ui.modal.open_room"), () => openMinted(M), null, false, "btn-open-room")];
  }
  return modalShell("", t("ui.sidebar.create_or_join"), null, close, body, foot, null, t("ui.close"));
}

/** Inline hint for a relay address: null when it is empty (nothing to say yet) or valid. Native twin: `modals.rs` `server_hint`. */
function serverHint(server) {
  try {
    parseServerAddr(server);
    return null;
  } catch (e) {
    return e.code === "no-port" ? t("ui.modal.server_no_port") : e.code === "bad-port" ? t("ui.modal.server_bad_port") : null;
  }
}
/** Same for the server part of a pasted join link; silent until the first `|` (still typing the address before that). */
function joinLinkHint(link) {
  const s = String(link || "").trim();
  const at = s.indexOf("|");
  return at < 0 ? null : serverHint(s.slice(0, at));
}

/**
 * When the join-link field changed to a link carrying a room name (5th field), fill the name field with it, unless the
 * user typed a name of their own: the field is empty or still holds the previous prefill (native `prefill_name`). Clearing
 * the field lets the next paste prefill again. Returns whether the name field changed.
 * Why: user request 2026-10-15, a pasted link pre-fills the name but it stays editable and what is in the field on "Join" is saved.
 */
function prefillName(M) {
  if (M.link === M.seenLink) return false;
  M.seenLink = M.link;
  let name = null;
  try {
    name = parseJoinLink(M.link || "").name;
  } catch {}
  if (!name) return false;
  const untouched = !(M.name || "").trim() || M.prefilledName === M.name;
  if (!untouched) return false;
  M.name = name;
  M.prefilledName = name;
  return true;
}

function doJoin(M) {
  if (joinLinkHint(M.link)) return;
  let p;
  try {
    p = parseJoinLink(M.link || "");
  } catch (e) {
    M.error = t("ui.err.bad_link", { error: e.message });
    render();
    return;
  }
  rememberServer(p);
  finishAdd(M.name, M.link);
}
function doCreate(M) {
  const server = (M.server || "").trim();
  if (server && serverHint(server)) return;
  const hashText = (M.hash || "").trim();
  let certHash = null;
  try {
    if (!server) throw new Error(t("ui.err.server_required"));
    if (hashText) {
      certHash = base64urlDecode(hashText);
      if (certHash.length !== 32) throw new Error(t("ui.err.bad_hash"));
    }
    const room = createRoom(server, { name: M.name, certHash });
    const p = parseJoinLink(room.link);
    M.minted = { link: room.link, roomIdHex: p.roomIdHex };
    S.settings.lastServer = server;
    S.settings.lastCertHash = hashText;
    persistSettings();
    M.error = null;
  } catch (e) {
    M.error = e.message;
  }
  render();
}
function openMinted(M) {
  finishAdd(M.name, M.minted.link);
}
function rememberServer(p) {
  S.settings.lastServer = p.server;
  S.settings.lastCertHash = p.certHash ? p.certHash && b64(p.certHash) : "";
  persistSettings();
}
const b64 = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

function finishAdd(name, link) {
  let res;
  try {
    res = addRoom({ name: (name || "").trim(), link });
  } catch (e) {
    S.modal.error = t("ui.err.bad_link", { error: e.message });
    render();
    return;
  }
  S.modal = null;
  select(res.entry, false);
  render();
  if (res.entry.state === "disconnected") connectRoom(res.entry);
}

/* ============================== actions ============================== */

async function clip(text) {
  try {
    await navigator.clipboard.writeText(text);
    toast(t("ui.toast.invite_copied"), "content_copy");
  } catch (e) {
    toast(t("ui.toast.copy_failed", { error: e.message }), "error", { err: true });
  }
}
const copyLink = (r) => clip(r.inviteLink());

// Escape closes the modal (an open menu takes the key first, in stage.js: Escape in a dropdown closes the dropdown alone).
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && S.modal) {
    S.modal = null;
    render();
  }
});

/** My emoji avatar (Settings > Profile): saved, and announced to every connected room's relay. */
function setEmoji(e) {
  S.settings.emoji = e;
  persistSettings();
  for (const r of S.rooms) if (r.client) r.client.setAvatar(e);
  render();
}

/* ============================== boot ============================== */

setLang(S.settings.lang);
for (const saved of loadRooms()) {
  try {
    S.rooms.push(new RoomEntry(saved));
  } catch {}
}
S.selected = S.rooms[0] ? S.rooms[0].id : null;
joinFromHash(); // before the first render: an open join dialog holds back the "Choose your name" modal
render();
if (caps.canWatch) for (const r of S.rooms) if (r.auto) connectRoom(r, { quiet: true });
if (caps.webCodecsEncode) {
  probeEncoders({ width: 1920, height: 1080, fps: 60 }).then((list) => {
    encoders = new Set(list.map((x) => x.codec));
    invalidate();
  }).catch(() => {});
}
// A pasted join link in the URL hash (#join=<link>) opens the join dialog prefilled.
function joinFromHash() {
  const m = /^#join=(.+)$/.exec(location.hash);
  if (!m) return;
  openAdd("join");
  S.modal.link = decodeURIComponent(m[1]);
  prefillName(S.modal);
  history.replaceState(null, "", location.pathname + location.search);
  render();
}
window.addEventListener("hashchange", joinFromHash);
Object.assign(A, {
  render, liveText, liveMeter, openStreamModal, startStage, copyLink, leaveRoom, doPopOut, doFullscreen, watchAndPop, popStage, setEmoji,
  toggleStageFullscreen,
  openModal: (m) => { S.menu = null; S.modal = m; render(); },
  selectRoom: (r, then) => { select(r, false); if (then) then(); },
});
bindWindow(window, "main");
window.__sss = { S, render, vg, stageWindow }; // vg / stageWindow: for the dev harness (docs/dev/e2e/stage.mjs) // debugging handle (also used by the local verification scripts)
