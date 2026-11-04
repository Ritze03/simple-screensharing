// Every dropdown and context menu of the app, in ONE style (D1: "every dropdown uses the app's own menu style, icons and sub-lines
// where they exist, under the field, as wide as it; no native <select> popups"): the control bar's Share / Audio / Layout / Room
// menus, the stream menu (the tile's `⋯` or a right click), the avatar picker, the stream modal's dropdown fields and the lists
// of "Also send to…" / "Move to…". Native twin: `client/src/ui/menu.rs` + `stage/menus.rs`; the CSS is the replica's `.menu`.
//
// At most one menu is open (`S.menu = { kind, mk, hostId, anchor | pos, ... }`). It is rebuilt by every render() like the other
// overlays; `mk` pairs it with the element that opened it (`data-mk`), so a second click on that element closes it again.

import { ENCODER_PRESETS } from "../core/index.js";
import { S, persistSettings, persistRooms, toast, displayName } from "./store.js";
import { t } from "./i18n.js";
import { h, ic, pct, cbx, seg, avatarEl, EMOJI, pickEmoji, snapVolumePct, volumeWheel, VOLUME_MAX_PCT, fmtKbps, meter } from "./dom.js";
import { roomById, setWatchVolume, setWatchMuted, unwatchStream, watchStream, connectRoom, applyLevelLoudness } from "./rooms.js";
import {
  allocated, allSends, capById, capInRoom, captureName, captureRes, stagedFor, captureLevel, setAudioMuted, addDest, moveDest, editDest, stopDest, stopAll, goLive, changeSource,
  screens, audioInRoom, startAudio, stopWebcam, cameras, camChoice, camsListing, useCamera,
} from "./capture.js";
import { A } from "./actions.js";
import { viewOf, viewKey, itemById, layoutOf, setLayout, resetView, pipOn, LAYOUTS, codecName } from "./stage-state.js";
import { popoutMode } from "./popout.js";

export const FPS_LIST = [15, 30, 60, 90, 120, 144, 165, 240];
export const PRESET_ORDER = ["text", "quality", "gaming"];
export const presetLabel = (p) => t("ui.preset." + p);
/** "60 FPS · Quality encoding · Keyframe 1000 ms": what a preset sets (native `preset_caption`); never "(custom)". */
export function presetCaption(preset, fps) {
  const e = ENCODER_PRESETS[preset] || ENCODER_PRESETS.quality;
  return t("ui.stream.preset_caption", { fps, mode: e.latencyMode === "realtime" ? t("ui.stream.ql_performance") : t("ui.stream.ql_quality"), ms: e.gopMs });
}

const rectOf = (el) => {
  const r = el.getBoundingClientRect();
  return { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height };
};
const hostIdOf = (el) => (el.ownerDocument === document ? "main" : "pop");

/** Open (or, for the element that opened it already, close) a menu anchored to `anchorEl`. `extra.pos` ({x, y} in the host window)
 *  places it at a point instead (a context menu). */
export function openMenu(kind, anchorEl, extra = {}) {
  const mk = extra.mk || kind;
  if (S.menu && S.menu.mk === mk && !extra.pos) {
    S.menu = null;
    return A.render();
  }
  S.menu = { kind, mk, hostId: hostIdOf(anchorEl), anchor: rectOf(anchorEl), ...extra };
  A.render();
}
export function closeMenu() {
  if (!S.menu) return false;
  S.menu = null;
  A.render();
  return true;
}
/** A control that opens a menu: marks itself with `data-mk` so the document's click handler can toggle. */
export const opener = (mk, kind, extra) => ({ "data-mk": mk, onclick: (e) => openMenu(kind || mk, e.currentTarget, { mk, ...(extra || {}) }) });

/* ---------- pieces ---------- */
const stay = (fn) => () => {
  fn();
  A.render();
};
const done = (fn) => () => {
  S.menu = null;
  if (fn) fn();
  A.render();
};
/** A menu row: `[icon][label + faint sub-line][extra][check]`; the current row has a lit icon and a check (`.on`, the replica's `.mi.on`). */
export function menuItem({ icon, iconEl, label, sub, onclick, cls = "", disabled, extra, on, role, title }) {
  return h("button", { type: "button", class: "mi" + (sub ? " two" : "") + (on ? " on" : "") + (cls ? " " + cls : ""), role: role || (on != null ? "menuitemradio" : "menuitem"), "aria-checked": on != null ? String(!!on) : null, disabled: !!disabled, title: title || null, onclick },
    iconEl || (icon ? ic(icon) : null), h("span", { class: "t" }, label, sub ? h("small", { text: sub }) : null), extra || null, on ? ic("check", "chk") : null);
}
const switchRow = (icon, label, on, onclick, { disabled = false, title = null } = {}) =>
  h("button", { type: "button", class: "mi", role: "menuitemcheckbox", "aria-checked": String(!!on), disabled, title, onclick }, ic(icon), h("span", { class: "t", text: label }), h("span", { class: "sw" + (on ? " on" : "") }));
const checkRow = (label, on, onclick, title, disabled) => h("div", { class: "mrow" }, cbx(label, on, onclick, { title, disabled }));
const label = (text) => h("div", { class: "ml", text });
const sep = () => h("div", { class: "msep" });
const pillLive = () => h("span", { class: "badge badge-live" }, h("span", { class: "dot" }), t("ui.room.live_pill"));
const head = (lead, title, sub, mono) => h("div", { class: "mh" }, lead, h("div", {}, h("b", { text: title }), sub ? h("small", { class: mono ? "mono" : null, text: sub }) : null));

/* ---------- control-bar menus ---------- */

/** The room the Share / Audio / Webcam buttons act on: a room's stage = that room; Watched = the room you share in (your screen's, else the webcam's or audio's). */
export function shareRoomOf(view) {
  if (view.kind === "room") return view.r;
  const c = S.captures.find((x) => x.kind === "screen" && (x.dests.size || x.stagedRoom)) || S.captures.find((x) => x.dests.size || x.stagedRoom);
  return c ? roomById([...c.dests.keys()][0] || c.stagedRoom) : null;
}

function shareMenu(view) {
  const r = shareRoomOf(view);
  if (!r) return [];
  const live = capInRoom(r.id);
  const staged = stagedFor(r.id);
  const cap = live || staged;
  const d = live ? live.dests.get(r.id) : staged ? staged.staged : S.settings.stream;
  const edit = (patch) => {
    if (live) editDest(live, r, patch);
    else if (staged) Object.assign(staged.staged, patch);
    else {
      Object.assign(S.settings.stream, patch);
      persistSettings();
    }
  };
  const lead = live ? pillLive() : h("span", { class: "badge badge-muted", text: cap ? t("ui.bar.ready") : t("ui.bar.off") });
  const sub = cap ? [captureRes(cap), codecName(d.codec), fmtKbps(live ? allocated(d) : d.kbps)].filter(Boolean).join(" · ") : "";
  const out = [
    head(lead, t("ui.bar.screen_share"), sub, true),
    label(t("ui.stream.fps_label")),
    h("div", { class: "mrow" }, denseSeg(FPS_LIST.map((f) => [f, String(f)]), d.fps, (v) => stay(() => edit({ fps: v }))())),
  ];
  out.push(label(t("ui.stream.preset")));
  out.push(h("div", { class: "mrow" }, seg(PRESET_ORDER.map((p) => [p, presetLabel(p)]), d.preset, (v) => stay(() => edit({ preset: v, fps: ENCODER_PRESETS[v].fps }))())));
  out.push(h("div", { class: "mrow" }, h("span", { class: "cap", text: presetCaption(d.preset, d.fps) })));
  out.push(sep());
  if (!cap) {
    // Not sharing here: an existing share can be added or moved here, a new one starts the picker.
    for (const x of screens().filter((c) => c.state === "live")) {
      out.push(menuItem({ icon: "add_to_queue", label: t("ui.popup.add_here", { screen: captureName(x) }), sub: t("ui.popup.add_here_sub"), onclick: done(() => addDest(x, r)) }));
      const from = [...x.dests.keys()].map((y) => roomById(y)).filter(Boolean);
      out.push(menuItem({ icon: "swap_horiz", label: t("ui.popup.move_here", { screen: captureName(x) }), sub: t("ui.popup.move_here_sub", { rooms: from.map((y) => y.name).join(", ") }), onclick: done(async () => { for (const f of from) await moveDest(x, f, r); }) }));
    }
    out.push(menuItem({ icon: "screen_share", label: screens().some((c) => c.state === "live") ? t("ui.popup.share_different") : t("ui.popup.share_here"), sub: screens().some((c) => c.state === "live") ? t("ui.popup.share_different_sub") : null, onclick: done(() => A.startStage(r)) }));
  } else {
    out.push(menuItem({ icon: "swap_horiz", label: t("ui.bar.change_source"), extra: h("span", { class: "unit", text: t("ui.bar.change_source_sub") }), onclick: done(() => changeSource(cap)) }));
    out.push(menuItem({ icon: "tune", label: t("ui.bar.all_stream_settings"), onclick: done(() => A.openStreamModal(cap.id, r.id)) }));
    out.push(live
      ? menuItem({ icon: "stop_screen_share", label: t("ui.bar.stop_sharing"), cls: "danger", onclick: done(() => { stopDest(live, r.id); toast(t("ui.toast.stopped_in", { room: r.name }), "stop_circle"); }) })
      : menuItem({ icon: "cast", label: t("ui.stream.go_live_in", { room: r.name }), cls: "primary", disabled: staged.state !== "ready" || r.state !== "joined", onclick: done(() => goLive(staged, r)) }));
  }
  return out;
}
const denseSeg = (opts, cur, onPick) => {
  const el = seg(opts, cur, onPick);
  el.classList.add("dense");
  return el;
};

/** Shared audio. A browser decides what audio can be shared (D9: Desktop audio, groups and the exclusion list are native Linux only, a browser can't list apps):
 *  the audio that came with the picked screen (its mute and meter) and an audio-only share of its own, picked in the browser's picker (a tab, or the system). */
function audioMenu(view) {
  const r = shareRoomOf(view);
  if (!r) return [];
  const cap = capInRoom(r.id) || stagedFor(r.id);
  const aud = audioInRoom(r.id);
  const out = [label(t("ui.popup.audio_source"))];
  if (cap && cap.audio) {
    out.push(menuItem({ icon: "speaker", label: t("ui.capture.audio_with", { screen: captureName(cap) }), sub: (cap.audio && cap.audio.label) || t("ui.popup.audio_from_browser"), on: true, extra: A.liveMeter(() => captureLevel(cap)), onclick: done() }));
  }
  if (aud) {
    out.push(menuItem({ icon: "music_note", label: aud.label, sub: t("ui.popup.audio_only_sub"), on: true, extra: A.liveMeter(() => captureLevel(aud)), onclick: done() }));
  } else {
    out.push(menuItem({ icon: "music_note", label: t("ui.popup.audio_only_item"), sub: t("ui.popup.audio_only_item_sub"), disabled: r.state !== "joined", onclick: done(() => startAudio(r)) }));
  }
  out.push(sep());
  if (cap && cap.audio) {
    out.push(menuItem({ icon: cap.audioMuted ? "volume_up" : "volume_off", label: cap.audioMuted ? t("ui.bar.unmute_audio") : t("ui.bar.mute_audio"), cls: cap.audioMuted ? "" : "danger", onclick: done(() => setAudioMuted(cap, !cap.audioMuted)) }));
  }
  if (aud) {
    out.push(menuItem({ icon: aud.audioMuted ? "volume_up" : "volume_off", label: aud.audioMuted ? t("ui.bar.unmute_audio") : t("ui.bar.mute_audio"), onclick: done(() => setAudioMuted(aud, !aud.audioMuted)) }));
    out.push(menuItem({ icon: "stop_circle", label: t("ui.popup.audio_stop"), cls: "danger", onclick: done(() => { stopDest(aud, r.id); toast(t("ui.toast.stopped_in", { room: r.name }), "stop_circle"); }) }));
  }
  out.push(h("div", { class: "mrow cap", text: t("ui.popup.audio_browser_note") }));
  return out;
}

/** The webcam's arrow (D1: "only offers the camera and 'All webcam settings…'"): the cameras the browser lists, the current one ticked, then the modal. */
function camMenu(view) {
  const r = shareRoomOf(view);
  if (!r) return [];
  const list = cameras();
  const cur = camChoice();
  const out = [label(t("ui.webcam.camera_heading"))];
  if (!list.length) out.push(h("div", { class: "mrow cap", text: camsListing() ? t("ui.bar.webcam_looking") : t("ui.bar.webcam_none") }));
  for (const cam of list) out.push(menuItem({ icon: "videocam", label: cam.name, on: cam === cur, onclick: done(() => useCamera(cam)) }));
  out.push(sep());
  out.push(menuItem({ icon: "tune", label: t("ui.webcam.all_settings"), onclick: done(() => A.openModal({ kind: "webcam", room: r.id })) }));
  return out;
}

function layoutMenu() {
  const key = S.menu.viewKey;
  const cur = layoutOf(key);
  return [
    label(t("ui.stage.layout_heading")),
    ...LAYOUTS.map(([k, icon, name, sub]) => menuItem({ icon, label: t(name), sub: t(sub), on: cur === k, onclick: done(() => setLayout(key, k)) })),
    sep(),
    checkRow(t("ui.stage.pip"), pipOn(key), stay(() => (pipOn(key) ? S.stage.pip.delete(key) : S.stage.pip.add(key))), t("ui.stage.pip_tip_menu")),
    checkRow(t("ui.stage.always_names"), S.stage.alwaysNames, stay(() => (S.stage.alwaysNames = !S.stage.alwaysNames)), t("ui.stage.always_names_tip")),
    checkRow(t("ui.stage.hide_own"), S.stage.hideOwn, stay(() => (S.stage.hideOwn = !S.stage.hideOwn)), t("ui.stage.hide_own_tip")),
    sep(),
    menuItem({ icon: "restart_alt", label: t("ui.stage.reset_layout"), onclick: done(() => resetView(key)) }),
  ];
}

/** Room settings (the bar's, or the header's `⋯` for a room with no stage): Auto-connect, Rename, Copy invite link, Leave / Connect, Forget. */
function roomMenu() {
  const r = roomById(S.menu.roomId);
  if (!r) return [];
  const joined = r.state === "joined";
  return [
    head(h("span", { class: "sdot " + r.state }), r.name, joined ? t("ui.bar.room_status", { count: r.members.length }) : r.state === "reconnecting" ? t("ui.sidebar.status_reconnecting") : t("ui.sidebar.status_disconnected")),
    switchRow("bolt", t("ui.room.auto_connect"), r.auto, stay(() => { r.auto = !r.auto; persistRooms(); }), { title: t("ui.room.auto_connect_tip") }),
    menuItem({ icon: "edit", label: t("ui.room.rename"), onclick: done(() => A.openModal({ kind: "rename", room: r.id, value: r.name })) }),
    menuItem({ icon: "link", label: t("ui.room.copy_invite"), disabled: !joined, title: joined ? null : t("ui.room.copy_invite_offline_tip"), onclick: done(() => A.copyLink(r)) }),
    r.state === "disconnected"
      ? menuItem({ icon: "login", label: t("ui.room.connect"), onclick: done(() => connectRoom(r)) })
      : menuItem({ icon: "logout", label: t("ui.bar.leave_room_item"), onclick: done(() => A.leaveRoom(r)) }),
    sep(),
    menuItem({ icon: "delete", label: t("ui.room.forget"), sub: t("ui.popup.forget_sub"), cls: "danger", onclick: done(() => A.openModal({ kind: "forget", room: r.id })) }),
  ];
}

/* ---------- the stream menu ---------- */

/** The stream menu's volume block: mute button, slider (0-500 %, acting during the drag, snapping at 100 %), the % and the Level loudness switch. */
function volumeRows(r, w) {
  const pctEl = h("span", { class: "pct", text: w.muted ? t("ui.tile.muted") : Math.round(w.vol * 100) + "%" });
  const slider = h("input", { type: "range", class: "vol", min: "0", max: String(VOLUME_MAX_PCT), step: "5", value: String(Math.round(w.vol * 100)), disabled: w.muted, "aria-label": t("ui.stage.volume") });
  // Acts during the drag, not on release (D1): every input event sets the decoder's gain.
  slider.addEventListener("input", (ev) => {
    if (!ev.fromWheel) slider.value = String(snapVolumePct(Number(slider.value)));
    setWatchVolume(r, w, Number(slider.value) / 100);
    pctEl.textContent = slider.value + "%";
    pct(slider);
  });
  volumeWheel(slider);
  return [
    label(t("ui.stage.volume")),
    h("div", { class: "mrow" },
      h("button", { type: "button", class: "icon-btn sm", title: w.muted ? t("ui.room.unmute") : t("ui.tile.mute"), "aria-label": w.muted ? t("ui.room.unmute") : t("ui.tile.mute"), onclick: stay(() => { r.client && r.client.resumeAudio(); setWatchMuted(r, w, !w.muted); }) }, ic(w.muted ? "volume_off" : "volume_up")),
      slider, pctEl),
    switchRow("graphic_eq", t("ui.room.level_label"), S.settings.levelLoudness, stay(() => { S.settings.levelLoudness = !S.settings.levelLoudness; persistSettings(); applyLevelLoudness(); })),
  ];
}

function tileMenu() {
  const M = S.menu;
  const view = viewOf(M.viewKey);
  const it = view && itemById(view, M.itemId);
  if (!it) return [];
  const { r, w, s, mine } = it;
  const out = [head(avatarEl(it.owner, r.emojiOf(it.conn) || (mine ? S.settings.emoji : ""), { sm: true }), mine ? (it.cam ? t("ui.stage.your_webcam") : t("ui.stage.your_stream")) : it.owner,
    `${it.label} · ${view.kind === "watched" ? r.name : mine ? captureRes(it.c) : it.kind === "audio" ? t("ui.room.audio_only") : [s.width ? `${s.width}×${s.height}` : ""].filter(Boolean).join("")}`)];
  if (it.kind === "audio") {
    // an audio-only sender: volume while listening, and Listen / Stop listening
    if (w) out.push(...volumeRows(r, w));
    if (w) out.push(sep());
    out.push(menuItem({ icon: w ? "volume_off" : "volume_up", label: w ? t("ui.room.stop_listening") : t("ui.room.listen"), onclick: done(() => (w ? unwatchStream(r, w.key) : watchStream(r, s.key))) }));
    return out;
  }
  if (w) {
    if (!it.cam && r.hasAudio(s.owner)) {
      out.push(...volumeRows(r, w));
    } else out.push(h("div", { class: "mrow dim" }, ic("volume_off"), t("ui.stage.no_audio_stream")));
    out.push(switchRow("monitoring", t("ui.stage.show_stats"), S.stage.stats.has(it.id), stay(() => (S.stage.stats.has(it.id) ? S.stage.stats.delete(it.id) : S.stage.stats.add(it.id)))));
  }
  out.push(sep());
  if (w) {
    if (popoutMode()) out.push(menuItem({ icon: "open_in_new", label: t("ui.room.pop_out"), onclick: done(() => A.doPopOut(r, w)) }));
    out.push(menuItem({ icon: "fullscreen", label: t("ui.stage.fullscreen"), onclick: done(() => A.doFullscreen(r, w)) }));
    out.push(menuItem({ icon: "visibility_off", label: t("ui.room.stop_watching"), cls: "danger", onclick: done(() => unwatchStream(r, w.key)) }));
  } else if (mine && it.cam) {
    out.push(menuItem({ icon: "tune", label: t("ui.webcam.settings_item"), onclick: done(() => A.openModal({ kind: "webcam", room: r.id })) }));
    out.push(menuItem({ icon: "videocam_off", label: t("ui.webcam.stop_item"), cls: "danger", onclick: done(() => { stopWebcam(); toast(t("ui.toast.stopped_in", { room: r.name }), "videocam_off"); }) }));
  } else if (mine) {
    out.push(menuItem({ icon: "tune", label: t("ui.bar.all_stream_settings"), onclick: done(() => A.openStreamModal(it.c.id, r.id)) }));
    out.push(menuItem({ icon: "stop_screen_share", label: t("ui.bar.stop_sharing"), cls: "danger", onclick: done(() => { stopDest(it.c, r.id); toast(t("ui.toast.stopped_in", { room: r.name }), "stop_circle"); }) }));
  } else {
    out.push(menuItem({ icon: "play_arrow", label: t("ui.stage.watch_stream"), onclick: done(() => watchStream(r, s.key)) }));
    if (popoutMode()) out.push(menuItem({ icon: "open_in_new", label: t("ui.room.pop_out"), onclick: done(() => A.watchAndPop(r, s.key)) }));
  }
  return out;
}

/* ---------- other menus ---------- */

function outgoingMenu() {
  const out = [label(t("ui.sidebar.outgoing.heading"))];
  const ds = allSends();
  if (!ds.length) out.push(h("div", { class: "mrow cap", text: t("ui.sidebar.outgoing.none") }));
  for (const { cap, d } of ds) {
    const rr = roomById(d.roomId);
    if (cap.kind === "audio") {
      out.push(menuItem({ icon: "music_note", label: rr ? rr.name : "?", sub: t("ui.sidebar.outgoing.audio_sub", { source: captureName(cap) }), onclick: done(() => rr && A.selectRoom(rr)) }));
    } else if (cap.kind === "webcam") {
      out.push(menuItem({ icon: "videocam", label: rr ? rr.name : "?", sub: t("ui.sidebar.outgoing.sub", { screen: captureName(cap), fps: d.fps, kbps: allocated(d) }), onclick: done(() => rr && A.selectRoom(rr, () => A.openModal({ kind: "webcam", room: rr.id }))) }));
    } else {
      out.push(menuItem({ icon: "cast", label: rr ? rr.name : "?", sub: t("ui.sidebar.outgoing.sub", { screen: captureName(cap), fps: d.fps, kbps: allocated(d) }), onclick: done(() => rr && A.selectRoom(rr, () => A.openStreamModal(cap.id, rr.id))) }));
    }
  }
  if (ds.length) {
    out.push(sep());
    out.push(menuItem({ icon: "stop_screen_share", label: t("ui.sidebar.outgoing.stop_all"), cls: "danger", onclick: done(() => { stopAll(); toast(t("ui.toast.stopped_all"), "stop_circle"); }) }));
  }
  return out;
}

/** "Also send to…" / "Move to…" of the stream modal. */
function targetsMenu(kind) {
  const r = roomById(S.menu.roomId);
  const c = capById(S.menu.cap);
  if (!r || !c) return [];
  const targets = S.rooms.filter((x) => x.id !== r.id && x.state === "joined");
  const out = [label(kind === "alsosend" ? t("ui.popup.also_send_heading") : t("ui.popup.move_heading"))];
  if (!targets.length) out.push(h("div", { class: "mrow cap", text: t("ui.popup.no_other_rooms") }));
  for (const x of targets) {
    const has = c.dests.has(x.id);
    if (kind === "alsosend") {
      out.push(menuItem({ iconEl: h("span", { class: "cbx bare" + (has ? " on" : "") }, h("span", { class: "box" }, ic("check"))), label: x.name, sub: has ? t("ui.popup.live_there") : t("ui.popup.people", { count: x.members.length }), role: "menuitemcheckbox", onclick: done(() => (has ? stopDest(c, x.id) : addDest(c, x))) }));
    } else {
      out.push(menuItem({ icon: "meeting_room", label: x.name, sub: has ? t("ui.popup.already_live") : t("ui.popup.move_sub"), disabled: has, onclick: done(() => moveDest(c, r, x)) }));
    }
  }
  return out;
}

/** Settings > Profile > Avatar: the picker. The 40 emoji of the replica's grid plus a "Paste any emoji" field (a browser has system emoji fonts; native draws any one-glyph emoji from its embedded Noto font). */
function emojiMenu() {
  const cur = S.settings.emoji;
  const set = (e) => { A.setEmoji(e); };
  // the hint lives in the menu state: every render() rebuilds the menu, and a hint that vanished 100 ms after it appeared would be no hint
  const hint = h("div", { class: "emoji-hint", id: "emoji-hint", text: S.menu.hint || "" });
  const say = (text) => {
    S.menu.hint = text;
    hint.textContent = text;
  };
  const inp = h("input", { class: "field", id: "emoji-in", value: cur, placeholder: t("ui.settings.avatar_paste"), "aria-label": t("ui.settings.avatar_paste"), autocomplete: "off" });
  inp.addEventListener("input", () => {
    const g = pickEmoji(inp.value);
    if (!g) return say(inp.value.trim() ? t("ui.settings.avatar_not_emoji") : "");
    if (new TextEncoder().encode(g).length > 16) return say(t("ui.settings.avatar_too_long"));
    say("");
    set(g);
  });
  inp.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      closeMenu();
    }
  });
  return [
    head(avatarEl(displayName(), cur), t("ui.settings.avatar_title"), t("ui.settings.avatar_hint")),
    h("div", { class: "emoji-grid" }, EMOJI.map((e) => h("button", { type: "button", class: e === cur ? "on" : "", "aria-label": e, onclick: done(() => set(e)) }, e))),
    h("div", { class: "mrow" }, inp),
    hint,
    menuItem({ icon: "text_fields", label: t("ui.settings.avatar_clear"), onclick: done(() => set("")) }),
  ];
}

/** A dropdown field of the app's own style: opens the menu under the field, as wide as it. Options: `[value, label, sub?, icon?]`. */
export function selectField({ id, opts, cur, onPick, title }) {
  const c = opts.find((o) => String(o[0]) === String(cur));
  return h("button", { type: "button", class: "field dd", id, "aria-haspopup": "listbox", title: title || null, ...opener("dd:" + id, "dd", { opts, cur, onPick }) },
    h("span", { class: "txt", text: c ? c[1] : String(cur ?? "") }), ic("expand_more"));
}
function ddMenu() {
  const M = S.menu;
  return M.opts.map(([v, text, sub, icon]) => menuItem({ icon, label: text, sub, on: String(v) === String(M.cur), role: "option", onclick: done(() => M.onPick(v)) }));
}

const BODIES = { share: shareMenu, audio: audioMenu, cam: camMenu, layout: layoutMenu, room: roomMenu, tile: tileMenu, outgoing: outgoingMenu, alsosend: () => targetsMenu("alsosend"), move: () => targetsMenu("move"), emoji: emojiMenu, dd: ddMenu };

/** The open menu as an element for host `hostId` ("main" | "pop"), or null. */
export function renderMenu(hostId) {
  const M = S.menu;
  if (!M || M.hostId !== hostId) return null;
  let view = null;
  if (M.viewKey) view = viewOf(M.viewKey);
  if (M.viewKey && !view) {
    S.menu = null;
    return null;
  }
  const kids = (BODIES[M.kind] || (() => []))(view);
  if (!kids.length) {
    S.menu = null;
    return null;
  }
  return h("div", { class: `menu m-${M.kind}`, id: "menu", role: "menu", "data-mk": M.mk }, kids);
}

/** Place the open menu (call after the render): above its anchor centred on it, below (right-aligned) when there is no room above; a point menu (context) at the pointer; a dropdown
 *  under its field, as wide as it. Always inside the window. */
export function positionMenu(win) {
  const M = S.menu;
  const doc = win.document;
  const m = doc.querySelector(".menu#menu");
  if (!m || !M) return;
  const W = win.innerWidth;
  const Hh = win.innerHeight;
  let x, y;
  if (M.kind === "dd") m.style.width = M.anchor.width + "px";
  const mw = m.offsetWidth;
  const mh = m.offsetHeight;
  const a = M.anchor;
  if (M.pos) {
    x = M.pos.x;
    y = M.pos.y;
    if (y + mh > Hh - 8) y = Math.max(8, y - mh - (M.pos.h || 0));
  } else if (M.kind === "dd") {
    x = a.left;
    y = a.bottom + 4;
    if (y + mh > Hh - 8) y = a.top - mh - 4;
  } else if (M.kind === "outgoing" || M.kind === "alsosend" || M.kind === "move") {
    x = a.left;
    y = a.top - mh - 6;
    if (y < 8) y = a.bottom + 6;
  } else {
    x = a.left + a.width / 2 - mw / 2;
    y = a.top - mh - 10;
    if (y < 8) {
      y = a.bottom + 8;
      x = a.right - mw;
    }
  }
  m.style.left = Math.max(8, Math.min(x, W - mw - 8)) + "px";
  m.style.top = Math.max(8, Math.min(y, Hh - mh - 8)) + "px";
}

/** Open the stream menu of tile `id` of view `viewKey` at a point (a right click) or under a button. */
export function openTileMenu(viewKeyStr, itemId, anchorEl, pos) {
  const mk = "tile:" + itemId;
  if (S.menu && S.menu.mk === mk && !pos) {
    S.menu = null;
    return A.render();
  }
  S.menu = { kind: "tile", mk, hostId: hostIdOf(anchorEl), anchor: rectOf(anchorEl), viewKey: viewKeyStr, itemId, pos: pos || null };
  A.render();
}
