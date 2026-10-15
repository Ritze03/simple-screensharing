// Browser client UI: the room-centric layout of the approved mockup (and of the native egui app), driven by the
// browser core in ../core. State lives in store.js (S); rooms.js and capture.js turn core events into that state;
// render() rebuilds the DOM from S (cheap: a few hundred nodes) and keeps long-lived nodes (video canvases, preview
// images) by moving them into the new tree. Numbers that change every second (fps, kbps, meters) are patched in
// place instead of re-rendering, so sliders and hovers are never interrupted.

import { capabilities, createRoom, parseJoinLink, base64urlDecode, ENCODER_PRESETS, probeEncoders } from "../core/index.js";
import { S, onInvalidate, invalidate, toast, persistSettings, persistRooms, loadRooms, autoName, displayName } from "./store.js";
import { t, setLang, getLang } from "./i18n.js";
import {
  h, ic, T, V, H, SL, FILL, pct, fmtKbps, ctrlRow, valBox, sliderRow, seg, cbx, card, secHead, roomRow, sharerRow, personRow, destRow,
  popItem, btnRow, gridRows, cols, emptyState, banner, meter, modalShell, avatarBg, initial,
} from "./dom.js";
import {
  RoomEntry, roomById, connectRoom, disconnectRoom, retryNow, watchStream, unwatchStream, setWatchVolume, setWatchMuted, allWatches,
  addRoom, forgetRoom, applyJitterSettings, connectErrorText,
} from "./rooms.js";
import {
  capById, capInRoom, stagedFor, liveRoomIds, allDests, allocated, applyBandwidth, captureName, captureRes, stage, discard, goLive, addDest,
  moveDest, stopDest, stopAll, editDest, setAudioMuted, captureLevel,
} from "./capture.js";
import { popOut, closePopout, popoutMode } from "./popout.js";

const caps = capabilities();
const FPS = [15, 30, 60, 90, 120, 144, 165, 240];
const CODECS = [["h264", "H.264"], ["av1", "AV1"], ["h265", "H.265"]];
const PRESETS = [["low_latency", "ui.preset.low_latency"], ["balanced", "ui.preset.balanced"], ["quality", "ui.preset.quality"], ["gaming", "ui.preset.gaming"]];
let encoders = null; // Set of codecs this browser can encode (probeEncoders), null until probed

const root = document.getElementById("app");
const SCROLLERS = [".p-center", ".p-side-scroll", ".p-right", ".modal-body"];
let centerW = 700;
let reflowing = false;
let live = []; // [{ el, fn }] text patched every stats tick
let meters = []; // [{ el, fn }] level meters, patched ~7x/s

const codecLabel = (c) => (CODECS.find(([v]) => v === c) || [c, String(c || "").toUpperCase()])[1];
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

function render() {
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

  const app = H({ cls: "p-app st", g: 0 }, renderSide(), renderMain());
  if (S.modal) app.appendChild(renderModal());
  const layer = h("div", { class: "layer" });
  if (S.popup) layer.appendChild(renderPopup());
  app.appendChild(layer);
  app.appendChild(V({ cls: "toasts", g: 1, attrs: { role: "status", "aria-live": "polite" } },
    S.toasts.map((x) => H({ cls: "toast" + (x.err ? " err" : ""), g: 2 }, ic(x.icon), T(x.text)))));
  root.replaceChildren(app);

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
  if (S.popup) positionPopup();
  patchLive();
  const c = root.querySelector(".p-center");
  if (c && !reflowing) {
    const w = c.clientWidth - 32;
    if (Math.abs(w - centerW) > 4) {
      const before = cols(centerW);
      centerW = w;
      if (cols(w) !== before) {
        reflowing = true;
        render();
        reflowing = false;
      }
    }
  }
}
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

/* ---------- small formatters ---------- */
const watchFps = (w) => (w && w.stats && Number.isFinite(w.stats.decodedFps) ? String(Math.round(w.stats.decodedFps)) : "");
const watchKbps = (w) => (w && w.stats && Number.isFinite(w.stats.bitrateKbps) ? fmtKbps(w.stats.bitrateKbps) : "");
const watchRes = (w, info) => {
  const st = w && w.stats;
  if (st && st.width && st.height) return `${st.width}x${st.height}`;
  return info && info.width ? `${info.width}x${info.height}` : "";
};
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
      SL(32, h("span", { class: "avatar", style: avatarBg(me()) }, initial(me()), h("span", { class: "pres" + (S.rooms.some((r) => r.state === "joined") ? "" : " off") }))),
      FILL(V({ cls: "who", g: 0, attrs: { role: "button", tabindex: "0", title: t("ui.sidebar.outgoing_tip"), onclick: (e) => openPopup("outgoing", e.currentTarget) } },
        T(me(), "b"), h("span", { class: "txt" + (nLive ? " live" : ""), text: nLive ? (nLive === 1 ? t("ui.sidebar.live_in_one_room") : t("ui.sidebar.live_in_rooms", { count: nLive })) : t("ui.sidebar.not_sharing") }))),
      SL(32, h("button", { class: "icon-btn", type: "button", id: "btn-settings", title: t("ui.sidebar.settings"), "aria-label": t("ui.sidebar.settings"), onclick: () => { S.modal = { kind: "settings" }; render(); } }, ic("settings")))));
}

function select(r, rerender = true) {
  S.view = "room";
  S.selected = r.id;
  S.popup = null;
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

function renderMain() {
  if (S.view === "watching") {
    const ws = allWatches();
    return V({ tag: "main", cls: "eq", g: 0 },
      H({ tag: "header", cls: "band top p-head", g: 2 }, h("h2", { text: t("ui.sidebar.watched_streams") }), T(t("ui.sidebar.watched_streams_tip"), "meta")),
      H({ cls: "eq st minh0", g: 0 },
        V({ cls: "p-center eq", g: 3 }, capsBanner(),
          ws.length ? renderTiles(ws, true) : emptyState("visibility_off", t("ui.watching.empty_title"), t("ui.watching.empty_text")))));
  }
  const r = sel();
  if (!r) {
    return V({ tag: "main", cls: "eq", g: 0 },
      H({ tag: "header", cls: "band top p-head", g: 2 }, h("h2", { text: t("app.title") })),
      H({ cls: "eq st minh0", g: 0 },
        V({ cls: "p-center eq", g: 4 }, capsBanner(),
          emptyState("meeting_room", t("ui.home.title"), t("ui.home.text"),
            h("button", { class: "btn btn-primary btn-lg wide", type: "button", id: "btn-home-add", onclick: () => openAdd() }, ic("add"), t("ui.sidebar.create_or_join"))))));
  }
  const center = V({ cls: "p-center eq", g: 4 });
  const cb = capsBanner();
  if (cb) center.appendChild(cb);
  if (r.state === "disconnected") {
    center.appendChild(emptyState(r.error ? "cloud_off" : "cloud_off", t("ui.room.not_connected_title", { room: r.name }), r.error ? null : t("ui.room.not_connected_text"),
      btnRow(h("button", { class: "btn btn-primary btn-lg", type: "button", id: "btn-connect", onclick: () => connectRoom(r) }, ic("login"), t("ui.room.connect")),
        h("button", { class: "btn btn-danger btn-lg", type: "button", onclick: () => { S.modal = { kind: "forget", room: r.id }; render(); } }, ic("delete"), t("ui.room.forget"))),
      { err: !!r.error, errText: r.error }));
  } else if (r.state === "connecting") {
    center.appendChild(emptyState("sync", t("ui.room.connecting_title", { room: r.name }), t("ui.room.connecting_text")));
  } else {
    if (r.state === "reconnecting") {
      // The browser cannot tell a stale certificate hash from a relay that is down, so the expiry hint only shows
      // once a few attempts have failed (a relay restart is back within one or two).
      const issue = r.issue; // "unreachable" | "full" | null (null: a connection was just lost)
      const attempt = Math.max(1, r.attempt);
      center.appendChild(banner(issue ? "link_off" : "sync_problem",
        issue === "unreachable" ? t("ui.room.reconnect_unreachable", { attempt }) : issue === "full" ? t("ui.room.reconnect_full", { attempt }) : t("ui.room.reconnecting_banner", { attempt }),
        h("button", { class: "btn btn-secondary", type: "button", onclick: () => retryNow(r) }, t("ui.room.retry_now")), { error: !!issue }));
    }
    renderRoomContent(r, center);
  }
  const showSides = r.state === "joined" || r.state === "reconnecting";
  return V({ tag: "main", cls: "eq", g: 0 },
    renderHead(r),
    H({ cls: "eq st minh0", g: 0 }, center, showSides ? renderPeople(r) : null),
    showSides ? renderBar(r) : null);
}

function renderHead(r) {
  const status = r.state === "joined" ? h("span", { class: "badge badge-live" }, h("span", { class: "dot" }), t("ui.sidebar.status_connected"))
    : r.state === "reconnecting" ? h("span", { class: "badge badge-degraded" }, ic("sync"), t("ui.sidebar.status_reconnecting"))
      : r.state === "connecting" ? h("span", { class: "badge badge-progress" }, h("span", { class: "bar" }), t("ui.room.connecting"))
        : h("span", { class: "badge badge-muted" }, t("ui.sidebar.status_disconnected"));
  const nSharing = r.videoStreams().length + (capInRoom(r.id) ? 1 : 0);
  const connected = r.state !== "disconnected";
  return H({ tag: "header", cls: "band top p-head", g: 2 },
    H({ g: 3, cls: "eq" }, h("h2", { text: r.name, title: r.name }), status,
      r.members.length ? T(t("ui.room.meta", { people: r.members.length, sharing: nSharing }), "meta") : null),
    H({ g: 2 },
      cbx(t("ui.room.auto_connect"), r.auto, () => { r.auto = !r.auto; persistRooms(); render(); }, { title: t("ui.room.auto_connect_tip"), style: "margin-right:var(--s1)" }),
      h("button", { class: "btn btn-secondary", type: "button", title: t("ui.room.copy_invite_tip"), onclick: () => copyLink(r) }, ic("link"), t("ui.room.copy_invite")),
      h("button", { class: "btn btn-danger", type: "button", id: "btn-leave", disabled: !connected, title: connected ? t("ui.room.leave_tip") : t("ui.room.leave_disabled_tip"),
        onclick: () => { S.modal = { kind: "leave", room: r.id }; render(); } }, ic("logout"), t("ui.room.leave")),
      h("button", { class: "icon-btn bordered", type: "button", title: t("ui.room.options"), "aria-label": t("ui.room.options"), onclick: (e) => openPopup("roommenu", e.currentTarget) }, ic("more_horiz"))));
}

function renderRoomContent(r, center) {
  const vids = r.videoStreams();
  const watched = vids.filter((s) => r.watches.has(s.key));
  const notWatched = vids.filter((s) => !r.watches.has(s.key));
  if (watched.length) {
    center.appendChild(V({ g: 3 }, secHead(t("ui.room.on_screen"), watched.length), renderTiles(watched.map((s) => ({ r, w: r.watches.get(s.key) })), false)));
  }
  const mine = capInRoom(r.id);
  const sec = V({ g: 3 }, secHead(watched.length ? t("ui.room.also_sharing") : t("ui.room.sharing"), notWatched.length + (mine ? 1 : 0)));
  if (!notWatched.length && !mine) {
    sec.appendChild(emptyState("tv_off",
      watched.length ? t("ui.room.watching_everything") : r.state === "reconnecting" ? t("ui.room.no_shares_now") : t("ui.room.nobody_sharing"),
      watched.length ? null : r.state === "reconnecting" ? t("ui.room.shares_come_back") : t("ui.room.start_own")));
  } else {
    const items = notWatched.map((s) => ({ s })).concat(mine ? [{ mine }] : []);
    sec.appendChild(gridRows(items, cols(centerW), (it) => (it.s ? shareCard(r, it.s) : myCard(it.mine, r))));
  }
  center.appendChild(sec);
}

const liveChip = () => h("span", { class: "chip-dark" }, h("span", { class: "dot" }), t("ui.room.live_chip"));

function shareCard(r, s) {
  const prev = r.previews.get(s.key);
  const owner = s.ownerName || r.memberName(s.owner);
  const canWatch = caps.canWatch && r.state === "joined";
  return V({ cls: "scard" + (S.flash === s.key ? " flash" : ""), g: 0, attrs: { "data-key": s.key } },
    h("div", { class: "thumb" },
      prev ? h("img", { src: prev, alt: "" }) : h("div", { class: "noprev" }, ic("screen_share")),
      V({ cls: "ovl", g: 0 }, H({ g: 2 }, liveChip(), FILL(null), r.hasAudio(s.owner) ? h("span", { class: "chip-dark", title: t("ui.room.shares_audio") }, ic("volume_up")) : null))),
    V({ cls: "card-pad", g: 2 },
      H({ cls: "who", g: 2, style: "height:22px" }, SL(22, h("span", { class: "avatar sm", style: avatarBg(owner), text: initial(owner) })), h("b", { text: owner }), FILL(T(s.label || "", "t"))),
      H({ g: 0 }, T(s.width ? `${s.width}×${s.height}` : t("ui.room.size_unknown"), "specs")),
      btnRow(
        h("button", { class: "btn btn-primary", type: "button", "data-watch": s.key, disabled: !canWatch, onclick: () => { watchStream(r, s.key); } }, ic("play_arrow"), t("ui.room.watch")),
        h("button", { class: "btn btn-secondary", type: "button", disabled: !canWatch || !popoutMode(), title: t("ui.room.pop_out_tip"), onclick: () => watchAndPop(r, s.key) }, ic("open_in_new"), t("ui.room.pop_out")))));
}

function myCard(c, r) {
  const d = c.dests.get(r.id);
  return V({ cls: "scard", g: 0 },
    h("div", { class: "thumb mine" },
      V({ cls: "ovl", g: 0 },
        H({ g: 2 }, d.state === "live" ? liveChip() : h("span", { class: "chip-dark", text: t("ui.room.starting_chip") }), FILL(null)),
        FILL(V({ cls: "mine-label", g: 1, style: "align-items:center;padding:8px" }, ic("cast"), T(t("ui.room.your_screen"))), { v: true, style: "align-items:center" }))),
    V({ cls: "card-pad", g: 2 },
      H({ cls: "who", g: 2, style: "height:22px" }, SL(22, h("span", { class: "avatar sm", style: avatarBg(me()), text: initial(me()) })), h("b", { text: t("ui.sidebar.you") }), FILL(T(captureName(c), "t"))),
      H({ g: 0 }, T(`${captureRes(c)} · ${d.fps} fps · ${codecLabel(d.codec)}`, "specs")),
      btnRow(h("button", { class: "btn btn-secondary", type: "button", onclick: () => openStreamModal(c.id, r.id) }, ic("tune"), t("ui.bar.stream_settings")))));
}

/* ---------- tiles ---------- */

function renderTiles(list, crossRoom) {
  const n = list.length === 1 ? 1 : 2;
  return gridRows(list, n, ({ r, w }) => renderTile(r, w, crossRoom), list.length === 1 ? "calc(46vh * 16 / 9)" : null);
}

function renderTile(r, w, crossRoom) {
  const info = r.streams.find((s) => s.key === w.key);
  const name = crossRoom ? `${w.ownerName} · ${r.name}` : w.ownerName;
  if (w.popped) {
    return V({ cls: "popped", g: 3, style: "align-items:center;justify-content:center" }, ic("open_in_new", "big-ico"),
      h("span", {}, h("b", { text: w.ownerName }), crossRoom ? ` · ${r.name}` : "", " ", t(w.popped === "pip" ? "ui.tile.popped_pip" : "ui.tile.popped_fs")),
      H({ g: 0, style: "width:300px" }, FILL(btnRow(
        h("button", { class: "btn btn-secondary", type: "button", onclick: () => { closePopout(w); render(); } }, ic("move_down"), t("ui.room.bring_back")),
        h("button", { class: "btn btn-danger", type: "button", onclick: () => unwatchStream(r, w.key) }, ic("close"), t("ui.room.stop_watching"))))));
  }
  const kids = [w.canvas];
  if (w.error) kids.push(h("div", { class: "wait err" }, ic("error"), h("span", { text: w.error })));
  else if (!w.firstFrame) kids.push(h("div", { class: "wait" }, ic("hourglass_empty"), h("span", { text: t("ui.tile.waiting") })));
  const fpsEl = liveText(h("span", { class: "v fps" }), () => (watchFps(w) ? watchFps(w) + "fps" : ""));
  const kbpsEl = liveText(h("span", { class: "v kbps" }), () => watchKbps(w));
  const resEl = liveText(h("span", { class: "v" }), () => watchRes(w, info));
  const hasAudio = r.hasAudio(w.owner);
  return h("div", { class: "tile", tabindex: "0", "aria-label": t("ui.tile.aria", { name: w.ownerName }), "data-key": w.key, ondblclick: () => doFullscreen(r, w) },
    kids, h("div", { class: "vig hover-ui" }),
    V({ cls: "ovl hover-ui", g: 0 },
      H({ g: 2 },
        FILL(H({ cls: "statbar", g: 2 }, h("span", { class: "dot" + (w.stats && w.stats.stalled ? " warn" : "") }), T(name, "nm"), resEl, fpsEl, kbpsEl)),
        H({ g: 1 },
          popoutMode() ? h("button", { class: "round-btn", type: "button", title: t("ui.room.pop_out_tip"), "aria-label": t("ui.room.pop_out"), onclick: () => doPopOut(r, w) }, ic("open_in_new")) : null,
          h("button", { class: "round-btn stop", type: "button", title: t("ui.room.stop_watching"), "aria-label": t("ui.room.stop_watching"), onclick: () => unwatchStream(r, w.key) }, ic("close")))),
      FILL(null, { v: true }),
      H({ g: 2 }, hasAudio ? audioPill(r, w) : h("span", { class: "chip-dark" }, ic("volume_off"), t("ui.tile.no_audio")))));
}

function audioPill(r, w) {
  const pctEl = h("span", { class: "pct", text: Math.round(w.vol * 100) + "%" });
  const slider = h("input", { type: "range", min: "0", max: "200", step: "5", value: String(Math.round(w.vol * 100)), "aria-label": t("ui.tile.volume"), id: "vol-" + w.key.replace(":", "-") });
  slider.addEventListener("input", () => {
    setWatchVolume(r, w, Number(slider.value) / 100);
    pctEl.textContent = slider.value + "%";
    pct(slider);
  });
  return H({ cls: "audio-pill" + (w.muted ? " muted" : ""), g: 1 },
    SL(25, h("button", { type: "button", title: w.muted ? t("ui.room.listen") : t("ui.tile.mute"), "aria-label": w.muted ? t("ui.room.listen") : t("ui.tile.mute"),
      onclick: () => { r.client && r.client.resumeAudio(); setWatchMuted(r, w, !w.muted); render(); } }, ic(w.muted ? "volume_off" : "volume_up"))),
    w.muted ? T(t("ui.tile.muted"), "pct") : [SL(110, slider), SL(34, pctEl, { al: "r" })]);
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

/* ---------- People ---------- */

function renderPeople(r) {
  const stale = r.state === "reconnecting";
  const members = [...r.members].sort((a, b) => (b.self ? 1 : 0) - (a.self ? 1 : 0));
  return V({ tag: "aside", cls: "p-right", g: 3 },
    secHead(stale ? t("ui.room.people_last_known") : t("ui.room.people"), members.length),
    V({ g: 0 }, members.map((m) => {
      const sharing = m.self ? !!capInRoom(r.id) : r.streams.some((s) => s.owner === m.id && s.kind === "video");
      const watching = !m.self && [...r.watches.values()].some((w) => w.owner === m.id);
      return personRow({
        avatar: h("span", { class: "avatar sm", style: avatarBg(m.name), text: initial(m.name) }),
        name: m.name || "?", you: m.self, youLabel: t("ui.room.you_suffix"), idle: !sharing, stale,
        pill: sharing ? h("span", { class: "badge badge-live" }, t("ui.room.live_pill")) : null,
        eye: watching ? ic("visibility", "ico watch") : null,
      });
    })));
}

/* ---------- bottom action bar ---------- */

function barStatus(pill, t1, t2) {
  return [SL(84, pill, { al: "l", min: true }), FILL(V({ g: 0 }, t1 ? T(t1, "t1") : null, t2 ? T(t2, "t2") : null), { v: true })];
}
function renderBar(r) {
  const liveCap = capInRoom(r.id);
  const staged = stagedFor(r.id);
  const bar = (...kids) => H({ tag: "footer", cls: "band bottom p-bar", g: 2 }, kids);
  if (r.state === "reconnecting") return bar(barStatus(h("span", { class: "badge badge-muted lg" }, ic("pause"), t("ui.bar.paused")), t("ui.bar.paused_text"), null));
  if (liveCap) {
    const d = liveCap.dests.get(r.id);
    const nRooms = liveCap.dests.size;
    const pill = d.state === "live" ? h("span", { class: "badge badge-live lg" }, h("span", { class: "dot" }), t("ui.room.live_pill"))
      : h("span", { class: "badge badge-progress lg" }, h("span", { class: "bar" }), d.state === "moving" ? t("ui.bar.moving") : d.state === "restarting" ? t("ui.bar.applying") : t("ui.bar.starting"));
    const t2 = liveText(T("", "t2 mono"), () => {
      const st = d.stats;
      const subs = st ? t("ui.bar.viewers", { count: st.subscribers }) : "";
      return `${codecLabel(d.codec)} · ${fmtKbps(allocated(d))}${subs ? " · " + subs : ""}`;
    });
    return bar(SL(84, pill, { al: "l", min: true }), FILL(V({ g: 0 }, T(`${captureName(liveCap)} · ${d.fps} fps`, "t1"), t2), { v: true }),
      audioControl(liveCap),
      h("button", { class: "btn btn-secondary btn-lg", type: "button", id: "btn-stream-settings", onclick: () => openStreamModal(liveCap.id, r.id) }, ic("tune"), t("ui.bar.stream_settings")),
      h("button", { class: "btn btn-secondary btn-lg", type: "button", title: t("ui.bar.rooms_tip"), onclick: (e) => openPopup("rooms", e.currentTarget) }, ic("cast"),
        nRooms === 1 ? t("ui.bar.one_room") : t("ui.bar.n_rooms", { count: nRooms }), ic("expand_more")),
      h("span", { class: "vsep" }),
      h("button", { class: "btn btn-danger-solid btn-lg", type: "button", id: "btn-stop-sharing", onclick: () => { stopDest(liveCap, r.id); toast(t("ui.toast.stopped_in", { room: r.name }), "stop_circle"); } },
        ic("stop_screen_share"), t("ui.bar.stop_sharing")));
  }
  if (staged) {
    const picking = staged.state === "picking";
    return bar(barStatus(picking ? h("span", { class: "badge badge-progress lg" }, h("span", { class: "bar" }), t("ui.bar.picking")) : h("span", { class: "badge badge-muted lg" }, t("ui.bar.ready")),
      picking ? t("ui.bar.picking_text") : t("ui.bar.not_live_yet", { screen: captureName(staged) }), picking ? t("ui.bar.picking_sub") : t("ui.bar.goes_live_in", { room: r.name })),
    audioControl(staged, picking),
    h("button", { class: "btn btn-secondary btn-lg", type: "button", disabled: picking, onclick: () => openStreamModal(staged.id, r.id) }, ic("tune"), t("ui.bar.stream_settings")),
    h("span", { class: "vsep" }),
    h("button", { class: "btn btn-secondary btn-lg", type: "button", onclick: () => discard(staged) }, t("ui.bar.discard")),
    h("button", { class: "btn btn-primary btn-lg", type: "button", id: "btn-go-live", disabled: picking, onclick: () => goLive(staged, r) }, ic("cast"), t("ui.bar.go_live")));
  }
  const others = S.captures.filter((c) => c.state === "live");
  const canShare = caps.canShare && r.state === "joined";
  return bar(
    h("button", { class: "btn btn-primary btn-lg", type: "button", id: "btn-share", disabled: !canShare, title: caps.canShare ? null : t("ui.err.cannot_share"),
      onclick: (e) => (others.length ? openPopup("share", e.currentTarget) : startStage(r)) }, ic("screen_share"), t("ui.bar.share_screen"), others.length ? ic("expand_more") : null),
    h("button", { class: "btn btn-secondary btn-lg", type: "button", disabled: true, title: t("ui.bar.share_audio_tip") }, ic("volume_up"), t("ui.bar.share_audio")),
    FILL(others.length ? T(t("ui.bar.live_elsewhere", { rooms: others.flatMap((c) => [...c.dests.keys()]).map((x) => (roomById(x) || { name: "?" }).name).join(", ") }), "t2") : null, { style: "padding-left:var(--s2)" }));
}

async function startStage(r) {
  S.popup = null;
  const c = await stage(r);
  if (c && S.view === "room" && S.selected === r.id && !S.modal) openStreamModal(c.id, r.id);
}

/** Audio control: hbox [slot 36 mute][source button]. The browser decides the audio source at pick time. */
function audioControl(c, disabled) {
  if (!c.audio) {
    return h("button", { class: "btn btn-secondary btn-lg", type: "button", disabled: true, title: c.state === "picking" ? null : t("ui.bar.no_audio_tip") }, ic("volume_off"), t("ui.bar.no_audio"));
  }
  return H({ cls: "audio-ctl" + (c.audioMuted ? " muted" : ""), g: 0 },
    SL(36, h("button", { class: "mute", type: "button", disabled: !!disabled, title: c.audioMuted ? t("ui.bar.unmute_audio") : t("ui.bar.mute_audio"), "aria-label": c.audioMuted ? t("ui.bar.unmute_audio") : t("ui.bar.mute_audio"),
      onclick: () => setAudioMuted(c, !c.audioMuted) }, ic(c.audioMuted ? "volume_off" : "volume_up")), { style: "height:100%" }),
    h("button", { type: "button", disabled: !!disabled, title: t("ui.bar.audio_source_tip"), onclick: (e) => openPopup("audio", e.currentTarget, { cap: c.id }) },
      h("span", { text: t("ui.capture.audio_with", { screen: captureName(c) }) }), liveMeter(() => captureLevel(c)), ic("expand_more")));
}

/* ============================== stream settings modal ============================== */

function openStreamModal(capId, roomId) {
  S.popup = null;
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

  // Source
  const audioCell = h("span", { class: "val center" }, liveMeter(() => captureLevel(c)));
  const source = card(t("ui.stream.source"), null,
    ctrlRow(t("ui.stream.screen"), T(`${captureName(c)}${captureRes(c) ? " · " + captureRes(c) : ""}`, "ro"),
      staged ? h("button", { class: "btn btn-secondary wide", type: "button", onclick: () => repick(c, r) }, t("ui.stream.change")) : undefined),
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
    staged ? null : btnRow(h("button", { class: "btn btn-secondary", type: "button", onclick: (e) => openPopup("alsosend", e.currentTarget) }, ic("add"), t("ui.stream.also_send")),
      h("button", { class: "btn btn-secondary", type: "button", onclick: (e) => openPopup("move", e.currentTarget) }, ic("swap_horiz"), t("ui.stream.move_to"))));

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

  // Video
  const fpsSel = h("select", { class: "field", id: "st-fps", "aria-label": t("share.framerate_label"), onchange: (e) => edit({ fps: Number(e.target.value) }) },
    FPS.map((f) => h("option", { value: String(f), selected: f === d.fps ? true : null, text: f + " fps" })));
  fpsSel.value = String(d.fps);
  const codecOpts = CODECS.map(([v, l]) => {
    const ok = !encoders || encoders.has(v);
    return [v, l, !ok, ok ? l : t("ui.stream.codec_unavailable", { codec: l })];
  });
  const video = card(t("ui.stream.video"), null,
    ctrlRow(t("share.framerate_label"), fpsSel, undefined, null, "st-fps"),
    bw.mode === "per_stream"
      ? sliderRow(t("ui.stream.bitrate"), "st-kbps", 500, 20000, 250, () => d.kbps, (v) => { d.kbps = v; }, String, t("ui.stream.bitrate_tip"), () => edit({ kbps: d.kbps }))
      : ctrlRow(t("ui.stream.bitrate"), h("span", { class: "ro txt", style: "width:100%;text-align:right", text: t("ui.stream.set_by_budget") }), valBox(String(staged ? Math.max(bw.floor, Math.round(bw.total / Math.max(1, n) / 100) * 100) : allocated(d))), t("ui.stream.budget_tip")),
    ctrlRow(t("ui.stream.codec"), seg(codecOpts, d.codec, (v) => edit({ codec: v }))));

  // Encoder
  const p = ENCODER_PRESETS[d.preset] || ENCODER_PRESETS.balanced;
  const encRows = [
    ctrlRow(t("ui.stream.preset"), seg(PRESETS.map(([v, k]) => [v, t(k)]), d.preset, (v) => edit({ preset: v }))),
    H({ g: 2 }, h("button", { class: "collapser" + (S.advOpen ? " open" : ""), type: "button", "aria-expanded": String(S.advOpen), onclick: () => { S.advOpen = !S.advOpen; render(); } }, ic("chevron_right"), t("ui.stream.advanced"))),
  ];
  if (S.advOpen) {
    encRows.push(V({ g: 2 },
      ctrlRow(t("ui.stream.tune"), T(p.latencyMode === "realtime" ? t("ui.stream.tune_low_latency") : t("ui.stream.tune_best_quality"), "ro")),
      ctrlRow(t("ui.stream.rate_control"), T(p.bitrateMode === "constant" ? t("ui.stream.rc_constant") : t("ui.stream.rc_variable"), "ro")),
      ctrlRow(t("ui.stream.keyframe_interval"), T(`${p.gopMs} ms`, "ro")),
      ctrlRow(t("ui.stream.quality_level"), T(t("ui.stream.native_only"), "ro")),
      h("span", { class: "hint", text: t("ui.stream.advanced_browser_note") })));
  }
  const enc = card(t("ui.stream.encoder"), null, encRows);

  const body = H({ g: 3, cls: "st", style: "align-items:flex-start" },
    V({ cls: "eq", g: 3 }, source, dests, bwCard),
    V({ cls: "eq", g: 3 }, video, enc, staged ? null : h("span", { class: "hint", text: t("ui.stream.restart_note") })));
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

async function repick(c, r) {
  const cfg = { ...c.staged };
  discard(c);
  S.modal = null;
  const nc = await stage(r);
  if (nc) {
    Object.assign(nc.staged, cfg);
    openStreamModal(nc.id, r.id);
  }
}

/* ============================== popups ============================== */

let popupAnchor = null;
function openPopup(kind, anchor, extra) {
  popupAnchor = anchor.getBoundingClientRect();
  S.popup = Object.assign({ kind }, extra || {});
  render();
}
function positionPopup() {
  const p = root.querySelector(".popup");
  if (!p || !popupAnchor) return;
  const pr = p.getBoundingClientRect();
  const W = window.innerWidth;
  const Hh = window.innerHeight;
  let left = popupAnchor.left;
  if (left + pr.width > W - 8) left = W - pr.width - 8;
  let top = popupAnchor.bottom + 6;
  if (top + pr.height > Hh - 8) top = popupAnchor.top - pr.height - 6;
  p.style.left = Math.max(8, left) + "px";
  p.style.top = Math.max(8, top) + "px";
}
function closePopup() {
  S.popup = null;
  render();
}

function renderPopup() {
  const P = S.popup;
  const r = S.modal && S.modal.kind === "stream" ? roomById(S.modal.room) : sel();
  const items = [];
  const ph = (x) => H({ cls: "ph", g: 0 }, T(x));
  const none = (x) => items.push(H({ cls: "pi none", g: 0 }, T(x)));
  const c = r && (capInRoom(r.id) || (S.modal && S.modal.cap ? capById(S.modal.cap) : null));
  const targets = () => S.rooms.filter((x) => r && x.id !== r.id && x.state === "joined");
  const alsoItems = () => {
    if (!targets().length) none(t("ui.popup.no_other_rooms"));
    targets().forEach((x) => {
      const has = c.dests.has(x.id);
      items.push(popItem({
        iconEl: h("span", { class: "cbx bare" + (has ? " on" : "") }, h("span", { class: "box" }, ic("check"))), label: x.name,
        sub: has ? t("ui.popup.live_there") : t("ui.popup.people", { count: x.members.length }), role: "menuitemcheckbox",
        onclick: () => { S.popup = null; if (has) stopDest(c, x.id); else addDest(c, x); render(); },
      }));
    });
  };
  const moveItems = () => {
    if (!targets().length) none(t("ui.popup.no_other_rooms"));
    targets().forEach((x) => items.push(popItem({
      icon: "meeting_room", label: x.name, sub: c.dests.has(x.id) ? t("ui.popup.already_live") : t("ui.popup.move_sub"), disabled: c.dests.has(x.id),
      onclick: () => { S.popup = null; moveDest(c, r, x); render(); },
    })));
  };
  if (P.kind === "roommenu" && r) {
    items.push(popItem({ icon: "edit", label: t("ui.room.rename"), onclick: () => { S.popup = null; S.modal = { kind: "rename", room: r.id, value: r.name }; render(); } }));
    items.push(popItem({ icon: "content_copy", label: t("ui.room.copy_invite"), onclick: () => { S.popup = null; copyLink(r); render(); } }));
    items.push(h("hr"));
    items.push(popItem({ icon: "delete", label: t("ui.room.forget"), sub: t("ui.popup.forget_sub"), cls: "danger", onclick: () => { S.popup = null; S.modal = { kind: "forget", room: r.id }; render(); } }));
  } else if (P.kind === "audio") {
    const cap = capById(P.cap);
    if (!cap) return h("div");
    items.push(ph(t("ui.popup.audio_source")));
    items.push(popItem({ icon: "speaker", label: t("ui.capture.audio_with", { screen: captureName(cap) }), sub: (cap.audio && cap.audio.label) || t("ui.popup.audio_from_browser"), cls: "sel", role: "menuitemradio", trailing: liveMeter(() => captureLevel(cap)), onclick: closePopup }));
    items.push(h("hr"));
    items.push(popItem({ icon: cap.audioMuted ? "volume_up" : "volume_off", label: cap.audioMuted ? t("ui.bar.unmute_audio") : t("ui.bar.mute_audio"), cls: cap.audioMuted ? "" : "danger", onclick: () => { S.popup = null; setAudioMuted(cap, !cap.audioMuted); } }));
    none(t("ui.popup.audio_browser_note"));
  } else if (P.kind === "alsosend" && c) {
    items.push(ph(t("ui.popup.also_send_heading")));
    alsoItems();
  } else if (P.kind === "move" && c) {
    items.push(ph(t("ui.popup.move_heading")));
    moveItems();
  } else if (P.kind === "rooms" && c) {
    items.push(ph(t("ui.popup.also_send_short")));
    alsoItems();
    items.push(h("hr"));
    items.push(ph(t("ui.stream.move_to")));
    moveItems();
  } else if (P.kind === "share" && r) {
    items.push(ph(t("ui.popup.share_in", { room: r.name })));
    S.captures.filter((x) => x.state === "live").forEach((x) => {
      items.push(popItem({ icon: "add_to_queue", label: t("ui.popup.add_here", { screen: captureName(x) }), sub: t("ui.popup.add_here_sub"), onclick: () => { S.popup = null; addDest(x, r); render(); } }));
      const from = [...x.dests.keys()].map((y) => roomById(y)).filter(Boolean);
      items.push(popItem({ icon: "swap_horiz", label: t("ui.popup.move_here", { screen: captureName(x) }), sub: t("ui.popup.move_here_sub", { rooms: from.map((y) => y.name).join(", ") }),
        onclick: async () => { S.popup = null; render(); for (const f of from) await moveDest(x, f, r); } }));
    });
    items.push(h("hr"));
    items.push(popItem({ icon: "screen_share", label: t("ui.popup.share_different"), sub: t("ui.popup.share_different_sub"), onclick: () => startStage(r) }));
  } else if (P.kind === "outgoing") {
    items.push(ph(t("ui.sidebar.outgoing.heading")));
    const ds = allDests();
    if (!ds.length) none(t("ui.sidebar.outgoing.none"));
    ds.forEach(({ cap, d }) => {
      const rr = roomById(d.roomId);
      items.push(popItem({ icon: "cast", label: rr ? rr.name : "?", sub: t("ui.sidebar.outgoing.sub", { screen: captureName(cap), fps: d.fps, kbps: allocated(d) }),
        onclick: () => { if (rr) { select(rr, false); openStreamModal(cap.id, rr.id); } } }));
    });
    if (ds.length) {
      items.push(h("hr"));
      items.push(popItem({ icon: "stop_screen_share", label: t("ui.sidebar.outgoing.stop_all"), cls: "danger", onclick: () => { S.popup = null; stopAll(); toast(t("ui.toast.stopped_all"), "stop_circle"); } }));
    }
  }
  return V({ cls: "popup", g: 0, attrs: { role: "menu" } }, items);
}

document.addEventListener("mousedown", (e) => {
  if (!S.popup) return;
  if (e.target.closest(".popup")) return;
  S.popup = null;
  setTimeout(render, 0);
}, true);
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  if (S.popup) closePopup();
  else if (S.modal) {
    S.modal = null;
    render();
  }
});

/* ============================== modals ============================== */

function openAdd(tab = "join") {
  S.popup = null;
  S.modal = { kind: "add", tab, name: "", link: "", server: S.settings.lastServer, hash: S.settings.lastCertHash, error: null, minted: null };
  render();
}

function renderModal() {
  const M = S.modal;
  const close = () => { S.modal = null; render(); };
  const scrim = V({ cls: "scrim", g: 0, attrs: { onmousedown: (e) => { if (e.target === scrim) close(); } } });
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
      h("span", { class: "hint", text: t("ui.settings.name_note") })),
    card(t("settings.language_label"), null, ctrlRow(t("settings.language_label"), seg([["en", "English"], ["de", "Deutsch"]], st.lang, (v) => { st.lang = v; setLang(v); persistSettings(); render(); }))),
    card(t("ui.settings.playback"), null,
      sliderRow(t("ui.settings.jitter"), "set-jitter", 0, 5000, 100, () => st.jitter, (v) => { st.jitter = v; }, (v) => (v / 1000).toFixed(1) + " s", t("ui.settings.jitter_tip"), commit),
      sliderRow(t("ui.settings.catchup"), "set-catchup", 5, 10, 1, () => st.catchup, (v) => { st.catchup = v; }, (v) => v + "%", t("ui.settings.catchup_tip"), commit)),
  ], [spacer(), lgBtn("btn-primary", t("ui.stream.done"), close, null, false, "btn-settings-done")], null, t("ui.close"));
}

function renderAdd(M, close, lgBtn, spacer) {
  let body, foot;
  const tabs = H({ g: 0 }, FILL(seg([["join", t("ui.modal.join_tab")], ["create", t("ui.modal.create_tab")]], M.tab, (v) => { M.tab = v; M.minted = null; M.error = null; render(); })));
  const err = M.error ? h("span", { class: "hint err", text: M.error }) : null;
  const field = (id, key, placeholder, label, cls, tip) => {
    const inp = h("input", { class: "field" + (cls ? " " + cls : ""), id, placeholder, value: M[key] || "", "aria-label": label, spellcheck: "false", autocomplete: "off" });
    inp.addEventListener("input", () => {
      M[key] = inp.value;
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
    body = [tabs, card(null, null, field("add-link", "link", "relay.example.com:4433|…|…|…", t("ui.modal.join_link"), "mono"), field("add-name", "name", t("ui.modal.name_placeholder"), t("ui.modal.room_name"), null, t("ui.modal.room_name_tip")), err)];
    foot = [spacer(), lgBtn("btn-secondary", t("ui.modal.cancel"), close), lgBtn("btn-primary", t("ui.modal.join"), () => doJoin(M), "login", false, "btn-join")];
  } else if (!M.minted) {
    body = [tabs, card(null, null,
      field("add-cname", "name", t("ui.modal.name_placeholder"), t("ui.modal.room_name"), null, t("ui.modal.room_name_tip")),
      field("add-server", "server", "relay.example.com:4433", t("ui.modal.server"), "mono", t("ui.modal.server_tip")),
      field("add-hash", "hash", t("ui.modal.hash_placeholder"), t("ui.modal.hash"), "mono", t("ui.modal.hash_tip")), err)];
    foot = [spacer(), lgBtn("btn-secondary", t("ui.modal.cancel"), close), lgBtn("btn-primary", t("ui.modal.create"), () => doCreate(M), "add", false, "btn-create")];
  } else {
    body = [tabs, h("p", { text: t("ui.modal.created_text") }),
      card(null, null,
        ctrlRow(t("ui.modal.room_id"), T(M.minted.roomIdHex, "ro")),
        ctrlRow(t("ui.modal.join_link"), T(M.minted.link, "ro"), h("button", { class: "btn btn-secondary wide", type: "button", onclick: () => clip(M.minted.link) }, t("ui.modal.copy"))))];
    foot = [spacer(), lgBtn("btn-primary", t("ui.modal.open_room"), () => openMinted(M), null, false, "btn-open-room")];
  }
  return modalShell("", t("ui.sidebar.create_or_join"), null, close, body, foot, null, t("ui.close"));
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
window.__sss = { S, render }; // debugging handle (also used by the local verification scripts)
