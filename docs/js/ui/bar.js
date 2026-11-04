// The stage's control bar (native `stage/bar.rs`, the replica's `.ctlrow`): `[hide sidebar][Copy invite link]` left, two button
// groups and Leave in the middle, `[pop out][fullscreen]` right. Share screen, Share audio, Webcam, Layout and Room settings are SPLIT
// buttons that always show their dropdown arrow, cut off from the button by a separator line; grouped buttons sit on a backdrop, the
// freestanding ones at the sides have none; a live Share/Audio button is green, a staged one accent; Leave is as tall as a group.
// The people button shows and hides the strip (no people list, no arrow above the avatars). Watched has no Invite, Room settings or
// Leave and shows Share and audio only while you have a share to act on, in the room you share in.
// Narrow windows degrade by CSS container queries on the stage: the Copy-invite label goes first, then the bar takes two rows.

import { capabilities } from "../core/index.js";
import { S } from "./store.js";
import { t } from "./i18n.js";
import { h, ic } from "./dom.js";
import { A } from "./actions.js";
import { capInRoom, stagedFor, setAudioMuted } from "./capture.js";
import { viewKey, layoutOf, LAYOUTS } from "./stage-state.js";
import { opener, openMenu, shareRoomOf } from "./menus.js";
import { pipSupported } from "./popout.js";

const caps = capabilities();

/** A split button `[main][arrow]`: `main` = props of the left button, `menu` = `{ mk, kind, extra }` of the arrow. `state`: "on" | "ready" | "". */
function split({ main, menu, state = "", disabled = false, mainOpensMenu = false, arrowTitle, arrowDisabled }) {
  const open = S.menu && S.menu.mk === menu.mk;
  const o = opener(menu.mk, menu.kind, menu.extra);
  return h("div", { class: "splitb" + (state ? " " + state : "") + (open ? " open" : ""), "data-anchor": menu.mk },
    h("button", { type: "button", class: "cb", id: main.id || null, title: main.title, "aria-label": main.title, "aria-pressed": main.pressed != null ? String(main.pressed) : null, disabled: disabled || null, ...(mainOpensMenu ? o : { onclick: main.onclick }) }, ic(main.icon)),
    h("button", { type: "button", class: "cb caret", title: arrowTitle || main.title, "aria-label": arrowTitle || main.title, disabled: arrowDisabled ?? disabled ?? null, ...o }, ic("expand_more")));
}

/**
 * @param view  the stage's view (`roomView(r)` / `WATCHED`)
 * @param host  "main" | "pop": the pop-out window has no sidebar button and no fullscreen of its own (a Document PiP window cannot go fullscreen), and its
 *              pop-out button brings the stage back
 */
export function renderBar(view, host, { fullscreen = false } = {}) {
  const r = view.kind === "room" ? view.r : null;
  const key = viewKey(view);
  const shareRoom = shareRoomOf(view);
  const live = shareRoom ? capInRoom(shareRoom.id) : null;
  const staged = shareRoom ? stagedFor(shareRoom.id) : null;
  const cap = live || staged;
  const picking = !!staged && staged.state === "picking";
  const state = live ? "on" : staged && staged.state === "ready" ? "ready" : "";
  const roomName = shareRoom ? shareRoom.name : "";

  const canShare = caps.canShare && !!shareRoom && shareRoom.state === "joined";
  const shareTitle = (cap ? (live ? t("ui.bar.stream_settings") : t("ui.stream.title_staged")) : t("ui.bar.share_screen_tip")) + (!r && shareRoom ? ` (${roomName})` : "");
  const othersLive = S.captures.some((c) => c.state === "live" && !(shareRoom && c.dests.has(shareRoom.id)));
  const shareBtn = !r && !cap ? null : split({
    main: {
      id: "btn-share", title: caps.canShare ? shareTitle : t("ui.err.cannot_share"), icon: "screen_share",
      // Idle: the picker (or, when your share is live in another room, its menu: add / move here stay reachable). Staged or live: the stream settings.
      onclick: (e) => (cap ? A.openStreamModal(cap.id, shareRoom.id) : othersLive ? openMenu("share", e.currentTarget.nextElementSibling, { mk: "share", viewKey: key }) : A.startStage(shareRoom)),
    },
    menu: { mk: "share", kind: "share", extra: { viewKey: key } }, state, disabled: !canShare || picking, arrowTitle: t("ui.bar.share_caret"), arrowDisabled: !canShare,
  });
  // Share audio: audio rides along with the screen the browser picked (today's behaviour), so the button is the mute switch and the arrow its menu.
  // Task I9 adds desktop audio and audio-only sharing here: this is where its button and menu body plug in (`menus.js` `audioMenu`).
  const hasAudio = !!cap && !!cap.audio;
  const audioTitle = !cap ? t("ui.bar.share_audio_first") : !cap.audio ? t("ui.bar.no_audio_tip") : cap.audioMuted ? t("ui.bar.unmute_audio") : t("ui.bar.mute_audio");
  const audioBtn = !r && !cap ? null : split({
    main: { id: "btn-audio", title: audioTitle, icon: hasAudio && !cap.audioMuted ? "music_note" : "music_off", onclick: () => cap && setAudioMuted(cap, !cap.audioMuted) },
    menu: { mk: "audio", kind: "audio", extra: { cap: cap && cap.id, viewKey: key } }, state: hasAudio && !cap.audioMuted ? (live ? "on" : "ready") : "", disabled: !hasAudio || picking, arrowTitle: t("ui.bar.audio_caret"),
  });
  // Webcam: present, disabled (task I10 enables its split and adds the camera menu body).
  const camBtn = !r && !cap ? null : split({ main: { id: "btn-webcam", title: t("ui.bar.webcam_soon"), icon: "videocam_off", onclick: () => {} }, menu: { mk: "cam", kind: "cam" }, disabled: true });

  const L = LAYOUTS.find((l) => l[0] === layoutOf(key));
  const layoutTitle = t("ui.bar.layout_tip", { layout: t(L[2]) });
  const layoutBtn = split({ main: { id: "btn-layout", title: layoutTitle, icon: L[1] }, menu: { mk: "layout", kind: "layout", extra: { viewKey: key } }, mainOpensMenu: true, arrowTitle: t("ui.bar.layout_caret") });
  const roomBtn = !r ? null : split({ main: { id: "btn-room", title: t("ui.bar.room_settings"), icon: "tune" }, menu: { mk: "room", kind: "room", extra: { roomId: r.id, viewKey: key } }, mainOpensMenu: true });
  const strip = !S.stage.stripHidden;
  const peopleBtn = h("button", { type: "button", class: "cb" + (strip ? " sel" : ""), id: "btn-people", title: strip ? t("ui.bar.hide_people") : t("ui.bar.show_people"), "aria-label": strip ? t("ui.bar.hide_people") : t("ui.bar.show_people"), "aria-pressed": String(strip),
    onclick: () => { S.stage.stripHidden = !S.stage.stripHidden; A.render(); } }, ic("group"));

  const side = host === "main" && !fullscreen
    ? h("button", { type: "button", class: "cb dim", id: "btn-sidebar", title: S.stage.sidebarHidden ? t("ui.bar.show_sidebar") : t("ui.bar.hide_sidebar"), "aria-label": S.stage.sidebarHidden ? t("ui.bar.show_sidebar") : t("ui.bar.hide_sidebar"),
      onclick: () => { S.stage.sidebarHidden = !S.stage.sidebarHidden; A.render(); } }, ic(S.stage.sidebarHidden ? "left_panel_open" : "left_panel_close"))
    : null;
  const invite = !r ? null : h("button", { type: "button", class: "cb dim", id: "btn-invite", title: t("ui.room.copy_invite_tip"), onclick: () => A.copyLink(r) }, ic("person_add"), h("span", { class: "lbl2", text: t("ui.room.copy_invite") }));
  const leave = !r ? null : h("button", { type: "button", class: "cb leave", id: "btn-leave", title: t("ui.bar.leave_room", { room: r.name }), "aria-label": t("ui.bar.leave_room", { room: r.name }), onclick: () => A.leaveRoom(r) }, ic("call_end"));
  const popped = host === "pop";
  const pop = popped || pipSupported()
    ? h("button", { type: "button", class: "cb dim", id: "btn-popstage", title: popped ? t("ui.bar.unpop_stage") : t("ui.bar.pop_stage"), "aria-label": popped ? t("ui.bar.unpop_stage") : t("ui.bar.pop_stage"), onclick: () => A.popStage(key, popped) }, ic(popped ? "close_fullscreen" : "open_in_new"))
    : null;
  const fs = host === "main"
    ? h("button", { type: "button", class: "cb dim", id: "btn-stagefs", title: S.stage.fullscreen ? t("ui.stage.exit_fullscreen") : t("ui.stage.fullscreen"), "aria-label": S.stage.fullscreen ? t("ui.stage.exit_fullscreen") : t("ui.stage.fullscreen"), onclick: () => A.toggleStageFullscreen() }, ic(S.stage.fullscreen ? "fullscreen_exit" : "fullscreen"))
    : null;
  const g1 = [shareBtn, audioBtn, camBtn].filter(Boolean);
  return h("div", { class: "ctlrow" },
    h("div", { class: "side" }, side, invite),
    h("div", { class: "mid" }, g1.length ? h("div", { class: "cgroup" }, g1) : null, h("div", { class: "cgroup" }, layoutBtn, roomBtn, peopleBtn), leave),
    h("div", { class: "side r" }, pop, fs));
}
