// The stage's control bar (native `stage/bar.rs`, the replica's `.ctlrow`): `[hide sidebar][Copy invite link]` left, two button
// groups and Leave in the middle, `[pop out][fullscreen]` right. Share screen, Share audio, Webcam, Layout and Room settings are SPLIT
// buttons that always show their dropdown arrow, cut off from the button by a separator line; grouped buttons sit on a backdrop, the
// freestanding ones at the sides have none; a live Share/Audio button is green, a staged one accent; Leave is as tall as a group.
// The people button shows and hides the strip (no people list, no arrow above the avatars). Watched has no Invite, Room settings or
// Leave and shows Share, audio and webcam only while you have such a share to act on, in the room you share in.
// Share audio and Webcam are shares of their own (task I10, native I7/I8): with a screen share that came with audio the audio button is its mute
// switch, else a click starts an audio-only share (a tab's or the system's audio, as the browser offers it) and the next one stops it; the webcam
// button starts the camera (`getUserMedia`) and the next click stops it, from another room it moves, its arrow only offers the cameras and
// "All webcam settings...".
// Narrow windows degrade by CSS container queries on the stage: the Copy-invite label goes first, then the bar takes two rows.

import { capabilities } from "../core/index.js";
import { S, PHONE, persistSettings } from "./store.js";
import { applyLevelLoudness } from "./rooms.js";
import { t } from "./i18n.js";
import { h, ic } from "./dom.js";
import { A } from "./actions.js";
import { capInRoom, screens, stagedFor, setAudioMuted, webcamInRoom, webcamCap, audioInRoom, startWebcam, stopWebcam, stopDest, camSupported, camsRequested, camsListing, refreshCameras, camChoice, cameras } from "./capture.js";
import { viewKey, layoutOf, LAYOUTS } from "./stage-state.js";
import { opener, openMenu, shareRoomOf } from "./menus.js";
import { toast } from "./store.js";
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
  const othersLive = screens().some((c) => c.state === "live" && !(shareRoom && c.dests.has(shareRoom.id)));
  const shareBtn = !r && !cap ? null : split({
    main: {
      id: "btn-share", title: caps.canShare ? shareTitle : t("ui.err.cannot_share"), icon: "screen_share",
      // Idle: the picker, then live at once with the remembered settings, no modal (or, when your share is live in another room, its menu: add / move here stay reachable). Staged or live: the stream settings.
      onclick: (e) => (cap ? A.openStreamModal(cap.id, shareRoom.id) : othersLive ? openMenu("share", e.currentTarget.nextElementSibling, { mk: "share", viewKey: key }) : A.startStage(shareRoom, true)),
    },
    menu: { mk: "share", kind: "share", extra: { viewKey: key } }, state, disabled: !canShare || picking, arrowTitle: t("ui.bar.share_caret"), arrowDisabled: !canShare,
  });
  // Share audio. With a screen share whose audio came along (the browser decides what) the button is its mute switch. Without one it is an audio-only share
  // of its own: a click picks a tab or the system audio, the next click stops it (native: Desktop audio on / off). Its arrow is the audio menu.
  const aud = shareRoom ? audioInRoom(shareRoom.id) : null;
  const hasAudio = !!cap && !!cap.audio;
  const joined = !!shareRoom && shareRoom.state === "joined";
  const audioOnClick = () => {
    if (hasAudio) return setAudioMuted(cap, !cap.audioMuted);
    if (aud) {
      stopDest(aud, shareRoom.id);
      return toast(t("ui.toast.stopped_in", { room: shareRoom.name }), "stop_circle");
    }
    return A.startAudio(shareRoom);
  };
  const audioTitle = hasAudio ? (cap.audioMuted ? t("ui.bar.unmute_audio") : t("ui.bar.mute_audio")) : aud ? t("ui.bar.stop_audio_tip", { source: aud.label }) : cap ? t("ui.bar.share_audio_only_tip") : t("ui.bar.share_audio_tip");
  const audioOn = hasAudio ? !cap.audioMuted : !!aud && !aud.audioMuted;
  const audioBtn = !r && !cap && !aud ? null : split({
    main: { id: "btn-audio", title: audioTitle, icon: audioOn ? "music_note" : "music_off", onclick: audioOnClick },
    menu: { mk: "audio", kind: "audio", extra: { viewKey: key } }, state: audioOn ? (hasAudio && !live ? "ready" : "on") : "", disabled: picking || !joined || !caps.canShare, arrowTitle: t("ui.bar.audio_caret"), arrowDisabled: !joined || !caps.canShare,
  });
  // Webcam: a share of its own, no screen needed (`getUserMedia`, `audio: false`). One click starts the saved camera in its saved mode, into the room the bar
  // acts on (from another room it MOVES); the next click stops it. The arrow only offers the cameras and "All webcam settings...".
  const wc = shareRoom ? webcamInRoom(shareRoom.id) : null;
  const wcElsewhere = !wc && !!webcamCap();
  const canCam = camSupported() && caps.webCodecsEncode && caps.trackProcessor && caps.canWatch && joined;
  if (canCam && !camsRequested()) setTimeout(refreshCameras, 0);
  const camName = (camChoice() && camChoice().name) || "";
  const noCam = !wc && !cameras().length;
  const camTitle = !camSupported() ? t("ui.err.cannot_share") : wc ? t("ui.bar.stop_webcam_tip", { camera: camName }) : noCam ? (!camsRequested() || camsListing() ? t("ui.bar.webcam_looking") : t("ui.bar.webcam_none")) : wcElsewhere ? t("ui.bar.move_webcam_tip", { camera: camName }) : t("ui.bar.share_webcam_tip", { camera: camName });
  const camBtn = !r && !wc ? null : split({
    main: {
      id: "btn-webcam", title: camTitle, icon: wc ? "videocam" : "videocam_off",
      onclick: () => (wc ? (stopWebcam(), toast(t("ui.toast.stopped_in", { room: shareRoom.name }), "videocam_off")) : startWebcam(shareRoom)),
    },
    menu: { mk: "cam", kind: "cam", extra: { viewKey: key } }, state: wc ? "on" : "", disabled: !canCam || noCam, arrowTitle: t("ui.bar.webcam_caret"), arrowDisabled: !canCam,
  });

  const L = LAYOUTS.find((l) => l[0] === layoutOf(key));
  const layoutTitle = t("ui.bar.layout_tip", { layout: t(L[2]) });
  const layoutBtn = split({ main: { id: "btn-layout", title: layoutTitle, icon: L[1] }, menu: { mk: "layout", kind: "layout", extra: { viewKey: key } }, mainOpensMenu: true, arrowTitle: t("ui.bar.layout_caret") });
  const roomBtn = !r ? null : split({ main: { id: "btn-room", title: t("ui.bar.room_settings"), icon: "tune" }, menu: { mk: "room", kind: "room", extra: { roomId: r.id, viewKey: key } }, mainOpensMenu: true });
  const strip = !S.stage.stripHidden;
  const peopleBtn = h("button", { type: "button", class: "cb" + (strip ? " sel" : ""), id: "btn-people", title: strip ? t("ui.bar.hide_people") : t("ui.bar.show_people"), "aria-label": strip ? t("ui.bar.hide_people") : t("ui.bar.show_people"), "aria-pressed": String(strip),
    onclick: () => { S.stage.stripHidden = !S.stage.stripHidden; A.render(); } }, ic("group"));

  // Level loudness is a global viewer setting, so it sits left of the people button in a room and in Watched alike.
  const lvl = !!S.settings.levelLoudness;
  const lvlTitle = t(lvl ? "ui.bar.level_loudness_on" : "ui.bar.level_loudness_off") + "\n" + t("ui.settings.level_loudness_tip");
  const levelBtn = h("button", { type: "button", class: "cb" + (lvl ? " sel" : ""), id: "btn-level", title: lvlTitle, "aria-label": lvlTitle, "aria-pressed": String(lvl),
    onclick: () => { S.settings.levelLoudness = !S.settings.levelLoudness; persistSettings(); applyLevelLoudness(); A.render(); } }, ic("graphic_eq"));

  const side = host === "main" && !fullscreen
    ? h("button", { type: "button", class: "cb dim", id: "btn-sidebar", title: S.stage.sidebarHidden ? t("ui.bar.show_sidebar") : t("ui.bar.hide_sidebar"), "aria-label": S.stage.sidebarHidden ? t("ui.bar.show_sidebar") : t("ui.bar.hide_sidebar"),
      onclick: () => { if (PHONE.matches) S.drawer = true; else S.stage.sidebarHidden = !S.stage.sidebarHidden; A.render(); } }, ic(S.stage.sidebarHidden ? "left_panel_open" : "left_panel_close"))
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
  // No getDisplayMedia (every phone browser): the two split buttons would only be dead weight in a narrow bar, so one dimmed button says why on tap.
  const noScreen = caps.canWatch && !caps.getDisplayMedia && !!shareRoom;
  const noScreenBtn = !noScreen ? null : h("button", { type: "button", class: "cb dim", id: "btn-noscreen", title: t("ui.err.no_screen_share"), "aria-label": t("ui.err.no_screen_share"), onclick: () => toast(t("ui.err.no_screen_share"), "screen_share", { err: true }) }, ic("screen_share"));
  const g1 = [noScreen ? noScreenBtn : shareBtn, noScreen ? null : audioBtn, camBtn].filter(Boolean);
  return h("div", { class: "ctlrow" },
    h("div", { class: "side" }, side, invite),
    h("div", { class: "mid" }, g1.length ? h("div", { class: "cgroup" }, g1) : null, h("div", { class: "cgroup" }, layoutBtn, roomBtn, levelBtn, peopleBtn), leave),
    h("div", { class: "side r" }, pop, fs));
}
