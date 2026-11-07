// The stage (task I6): the centre of a joined room and the whole "Watched streams" view are one black stage that replaces the old
// tiles, share cards and People list: the streams as Split / Crop / Floating, the people strip with its hover card, and the control
// bar (bar.js). A port of the replica's stage (`superdoc/ui/replica/index.html`) with the behaviour of the native client
// (`client/src/ui/stage/`), whose geometry (stage-geom.js) and fullscreen rule (chrome.js) are line-for-line ports with the same tests.
//
// How it lives with a full re-render: `render()` rebuilds the DOM from `S`, so everything here is a pure function of state plus the
// per-view layout (stage-state.js). Positions are applied AFTER the tree is inserted (`afterRender` -> `layoutArena`), a drag updates
// styles directly and holds back re-renders while it lasts (`isDragging`), and the canvases of the watches are re-adopted by the new
// tree, so video never restarts. The people strip's hover card and the viewers dropdown are decided by pointer geometry on the
// document (the replaced elements never get a mouseleave).

import { S, invalidate, PHONE } from "./store.js";
import { t } from "./i18n.js";
import { h, ic, T, avatarEl, lp } from "./dom.js";
import { A } from "./actions.js";
import { capabilities } from "../core/index.js";
import { closePopout, popoutMode } from "./popout.js";
import { previewFor } from "./capture.js";
import { pipEl, placePip, pipPointerMove, pipPointerUp, pipDragging } from "./pip.js";
import * as G from "./stage-geom.js";
import { chromeStart, chromeStep, chromeRemaining } from "./chrome.js";
import { vg, viewKey, viewOf, layoutOf, collect, visibleItems, itemAspect, audibleFor, peopleOf, itemNumbers, watchFps, watchKbps, watchRes } from "./stage-state.js";
import { renderBar } from "./bar.js";
import { openTileMenu, renderMenu, positionMenu, closeMenu } from "./menus.js";
import { watchStream, unwatchStream } from "./rooms.js";

const caps = capabilities();
/** The pop-out button: a real window (Document PiP) anywhere; where it would only be the tile's fullscreen again (no PiP) a phone has the fullscreen button already. */
const popOutBtn = () => !!popoutMode() && !(PHONE.matches && popoutMode() === "fullscreen");
export { watchFps, watchKbps, watchRes };

const arenas = new Set(); // every arena built by the last render (main window and the pop-out window)
let drag = null; // a pointer drag in progress: { kind: "bar" | "tile" | "rz", ... }
export const isDragging = () => !!drag || pipDragging();

/* ============================== the stage ============================== */

const note = (icon, title, text, extra) => h("div", { class: "arena-note" }, ic(icon, "big"), h("b", { text: title }), text ? h("span", { text }) : null, extra || null);

/** The stage of `view` for host `host` ({ id: "main" | "pop", win, doc }). In fullscreen the strip and bar lie over the streams. */
export function renderStage(view, host, { fullscreen = false } = {}) {
  const key = viewKey(view);
  const items = collect(view);
  const vis = visibleItems(view, items);
  const layout = layoutOf(key);
  const arena = h("div", { class: `arena ${layout}`, "data-view": key });
  arena._m = { key, hostId: host.id };
  if (!items.length) {
    arena.appendChild(view.kind === "room"
      ? note("tv_off", t("ui.room.nobody_title"), t("ui.room.nobody_text"))
      : note("visibility_off", t("ui.room.watching_empty_title"), t("ui.room.watching_empty_text")));
  } else if (!vis.length) {
    const ownOnly = S.stage.hideOwn && items.some((i) => i.mine) && !items.some((i) => i.popped && !i.mine);
    arena.appendChild(ownOnly ? note("visibility_off", t("ui.stage.own_hidden_title"), t("ui.stage.own_hidden_text")) : note("open_in_new", t("ui.stage.all_popped_title"), t("ui.stage.all_popped_text")));
  } else {
    for (const it of vis) arena.appendChild(tileEl(it, view, key, arena));
    arenas.add(arena);
  }
  const people = peopleOf(view, items);
  const strip = S.stage.stripHidden || !people.length ? null : stripEl(view, items, people, host);
  const sec = h("section", { class: "vb g0 eq stg minh0", "data-view": key }, arena, strip, renderBar(view, host.id, { fullscreen }));
  if (!fullscreen) return sec;
  const wrap = h("div", { class: "stagefs" + (chromeShown ? "" : " chrome-off") }, sec);
  return wrap;
}

/** The note the main window shows while the whole stage is open in its own window. */
export function renderPoppedNote(view) {
  const key = viewKey(view);
  return h("section", { class: "vb g0 eq stg minh0 popped-stage" },
    h("div", { class: "arena" }, note("open_in_new", t("ui.stage.popped_title"), t("ui.stage.popped_text"),
      h("button", { type: "button", class: "btn btn-secondary", id: "btn-stage-back", onclick: () => A.popStage(key, true) }, ic("close_fullscreen"), t("ui.stage.bring_back")))));
}

/* ============================== tiles ============================== */

function tileEl(it, view, key, arena) {
  const { r, w, mine, kind } = it;
  const cross = view.kind === "watched";
  const audioOnly = kind === "audio";
  const hasAudio = !mine && !!w && !it.cam && r.hasAudio(it.s.owner); // a webcam brings no audio
  const audible = (hasAudio || (audioOnly && !!w)) && !w.muted;
  const stats = !!w && !audioOnly && S.stage.showStats;
  const open = S.menu && S.menu.kind === "tile" && S.menu.itemId === it.id;
  const label = cross ? `${it.label} · ${r.name}` : it.label;
  const tile = h("div", {
    class: "st-tile" + (kind === "idle" ? " idle" : "") + (audioOnly ? " audio-only" : "") + (kind === "mine" ? " mine" : "") + (it.cam ? " cam" : "") + (audible ? " audible" : "") + (S.flash === it.s?.key ? " flash" : "") + (open ? " menu-open" : ""),
    "data-id": it.id, style: `--ar:${it.ar.toFixed(4)}`, tabindex: "0", "aria-label": mine ? t("ui.stage.your_stream") : t("ui.tile.aria", { name: it.owner }),
  });
  tile._it = it;
  const vid = h("div", { class: "vid" });
  if (kind === "video") {
    vid.appendChild(w.canvas);
    if (w.error) tile.append(vid, h("div", { class: "wait err" }, ic("error"), h("span", { text: w.error })));
    else if (!w.firstFrame) tile.append(vid, h("div", { class: "wait" }, ic("hourglass_empty"), h("span", { text: t("ui.tile.waiting") })));
    else tile.append(vid);
  } else if (kind === "mine") {
    const live = it.d.state === "live";
    const what = it.cam ? t("ui.room.my_webcam") : t("ui.room.my_screen");
    vid.append(...[previewFor(it.c), h("div", { class: "mine-pic" }, ic(it.cam ? "videocam" : "cast"), h("span", { text: `${what} · ${live ? t("ui.room.live_chip") : t("ui.room.chip_starting")}` }))].filter(Boolean));
    tile.appendChild(vid);
  } else if (audioOnly) {
    // an audio-only sender: no picture, a Listen button (Stop listening while it plays)
    tile.append(vid, h("div", { class: "invite" },
      avatarEl(it.owner, r.emojiOf(it.conn)),
      h("b", { text: t("ui.stage.sharing_audio", { name: it.owner }) }),
      h("span", { text: t("ui.room.audio_only") }),
      h("div", { class: "hb g2" }, w
        ? h("button", { type: "button", class: "btn btn-secondary", "data-listen": it.s.key, onclick: () => unwatchStream(r, it.s.key) }, ic("volume_off"), t("ui.room.stop_listening"))
        : h("button", { type: "button", class: "btn btn-primary", "data-listen": it.s.key, disabled: !caps.canWatch || r.state !== "joined" || null, onclick: () => watchStream(r, it.s.key) }, ic("volume_up"), t("ui.room.listen")))));
  } else {
    const prev = r.previews.get(it.s.key);
    if (prev) vid.appendChild(h("img", { src: prev, alt: "" }));
    tile.append(vid, h("div", { class: "invite" },
      avatarEl(it.owner, r.emojiOf(it.conn)),
      h("b", { text: t("ui.stage.sharing", { name: it.owner, label: it.label }) }),
      it.s.width ? h("span", { text: `${it.s.width}×${it.s.height}` }) : null,
      h("div", { class: "hb g2" },
        h("button", { type: "button", class: "btn btn-primary", "data-watch": it.s.key, disabled: !caps.canWatch || r.state !== "joined" || null, onclick: () => watchStream(r, it.s.key) }, ic("play_arrow"), t("ui.stage.watch_stream")),
        popOutBtn() ? h("button", { type: "button", class: "btn btn-secondary", title: t("ui.room.pop_out_card_tip"), disabled: !caps.canWatch || r.state !== "joined" || null, onclick: () => A.watchAndPop(r, it.s.key) }, ic("open_in_new"), t("ui.room.pop_out")) : null)));
  }

  // the name tag (top left): audio status icon, name, source; with "Show stats" also the numbers in slots reserved for 9999x9999,
  // 999 fps and 99.9 Mbps so the pill is as wide as its content and never jitters; they show while the tile is hovered
  const tagIcon = it.cam && kind === "video" ? ic("videocam") : kind === "video" ? (!hasAudio ? ic("volume_off") : w.muted ? ic("volume_off", "off") : ic("volume_up"))
    : audioOnly ? (w ? (w.muted ? ic("volume_off", "off") : ic("volume_up")) : ic("volume_off"))
      : mine ? ic(it.cam ? "videocam" : "cast", it.d.state === "live" ? "live" : "warn") : ic("visibility_off");
  const tag = h("span", { class: "tag" + (S.stage.alwaysNames ? "" : " hover-ui") + (stats ? " stats" : ""), title: `${mine ? t("ui.sidebar.you") : it.owner} · ${label}` },
    tagIcon, h("span", { class: "nm", text: mine ? t("ui.sidebar.you") : it.owner }), h("span", { class: "lb", text: "· " + label }));
  if (stats) {
    tile.title = t("ui.popout.stats_tip");
    tag.append(
      A.liveText(h("span", { class: "v res" }), () => watchRes(w, it.s)),
      A.liveText(h("span", { class: "v fps" }), () => (watchFps(w) ? watchFps(w) + " fps" : "")),
      A.liveText(h("span", { class: "v kbps" }), () => watchKbps(w)));
  }
  tile.appendChild(tag);

  // the buttons (top right), only while the tile is hovered
  const btn = (icon, title, onclick, cls = "") => h("button", { type: "button", class: "round-btn " + cls, title, "aria-label": title, onclick }, ic(icon));
  const tools = h("div", { class: "tools hover-ui" });
  if (kind === "mine") {
    const chip = viewersChip(it.c, r);
    tools.append(...[h("span", { class: "chip-dark pchip", title: t("ui.room.preview_tip"), text: t("ui.room.preview_chip") }), chip,
      it.cam ? btn("tune", t("ui.room.webcam_settings"), () => A.openModal({ kind: "webcam", room: r.id })) : btn("tune", t("ui.room.stream_settings"), () => A.openStreamModal(it.c.id, r.id))].filter(Boolean));
  } else if (kind === "video") {
    if (popOutBtn()) tools.append(btn("open_in_new", t("ui.room.pop_out_tip"), () => A.doPopOut(r, w)));
    tools.append(btn("fullscreen", t("ui.stage.fullscreen"), () => A.doFullscreen(r, w))); // no element fullscreen (iPhone): popout.js falls back to a fixed full-viewport layer
  }
  tools.append(h("button", { type: "button", class: "round-btn", title: t("ui.stage.stream_menu"), "aria-label": t("ui.stage.stream_menu"), "data-mk": "tile:" + it.id, onclick: (e) => openTileMenu(key, it.id, e.currentTarget) }, ic("more_horiz")));
  tile.appendChild(tools);
  tile.appendChild(h("span", { class: "rz hover-ui", title: t("ui.stage.resize_tip") }));

  // Webcam in picture: the webcam of this stream's owner sits inside its picture (stage-state.js `pipPairs`); a press on it stops there (pip.js).
  if (it.pipCam) {
    tile.appendChild(pipEl(it.pipCam, {
      key,
      geom: () => ({ tw: tile.clientWidth, th: tile.clientHeight, ar: parseFloat(tile.style.getPropertyValue("--ar")) || 16 / 9, crop: layoutOf(key) === "crop" }),
      onPress: () => {
        if (layoutOf(key) === "float") {
          raise(vg(key), it.id); // a floating parent comes to the front when its webcam is pressed
          layoutArena(arena);
        }
      },
    }));
  }
  tile.addEventListener("pointerdown", (e) => tilePointerDown(e, tile, arena));
  if (kind === "video") tile.addEventListener("dblclick", (e) => { if (!e.target.closest("button, input")) A.doFullscreen(r, w); });
  tile.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    openTileMenu(key, it.id, tile, { x: e.clientX, y: e.clientY });
  });
  return tile;
}

/* ---------- who watches my share: eye + count, a dropdown with avatar and name on hover (task #116) ---------- */

let vpop = null; // { cap, room }: the open dropdown; it is rebuilt by every render, so it follows live changes
const VIEWER_ROWS_MAX = 8;
/** The members watching my share `c` in room `r`, sorted by name; `null` when the relay does not report viewers. Members, not names: two viewers of one name are two rows,
 *  each with their own emoji. */
function viewersOf(c, r) {
  const share = c.dests.get(r.id)?.share;
  if (share?.viewers?.() == null) return null;
  const list = share.viewerIds.map((id) => r.members.find((m) => m.id === id)).filter(Boolean);
  return list.sort((a, b) => (a.name || "").toLowerCase().localeCompare((b.name || "").toLowerCase()));
}

/** The viewer count chip of my own tile; `null` when the relay does not report viewers (an older relay): no count rather than a wrong "0". */
function viewersChip(c, r) {
  const viewers = viewersOf(c, r);
  if (!viewers) return null;
  const chip = h("span", { class: "chip-dark viewers", "data-cap": c.id, "data-room": r.id }, ic("visibility"), String(viewers.length));
  if (!viewers.length) {
    chip.title = t("ui.room.viewers_none");
    return chip;
  }
  chip.addEventListener("mouseenter", () => {
    if (!vpop || vpop.cap !== c.id || vpop.room !== r.id) {
      vpop = { cap: c.id, room: r.id, hostId: chip.ownerDocument === document ? "main" : "pop" };
      A.render();
    }
  });
  return chip;
}
function viewersPopEl() {
  const c = S.captures.find((x) => x.id === vpop.cap);
  const r = S.rooms.find((x) => x.id === vpop.room);
  const viewers = c && r ? viewersOf(c, r) : null;
  if (!viewers || !viewers.length) return null;
  const rows = viewers.slice(0, VIEWER_ROWS_MAX).map((m) => h("div", { class: "hb g2 vrow" }, h("div", { class: "sl", style: "width:22px;flex-basis:22px" }, avatarEl(m.name, r.emojiOf(m.id), { sm: true })), h("div", { class: "fl" }, T(m.name))));
  if (viewers.length > VIEWER_ROWS_MAX) rows.push(h("div", { class: "hb g0 vrow more" }, T(`+${viewers.length - VIEWER_ROWS_MAX}`)));
  return h("div", { class: "vb g0 vpop" }, h("div", { class: "hb g0 ph" }, T(`${t("ui.room.viewers_title").toUpperCase()} · ${viewers.length}`)), rows);
}
function positionViewersPop(win) {
  const doc = win.document;
  const p = doc.querySelector(".vpop");
  const chip = vpop && doc.querySelector(`.viewers[data-cap="${vpop.cap}"][data-room="${vpop.room}"]`);
  if (!p) return;
  if (!chip) {
    vpop = null;
    p.remove();
    return;
  }
  const cr = chip.getBoundingClientRect();
  const pr = p.getBoundingClientRect();
  p.style.left = Math.max(8, Math.min(cr.right - pr.width, win.innerWidth - pr.width - 8)) + "px";
  const below = cr.bottom - 2; // overlaps the chip by 2 px so the pointer never crosses a gap
  p.style.top = (below + pr.height > win.innerHeight - 8 ? cr.top + 2 - pr.height : below) + "px";
}

/* ============================== people strip + hover card ============================== */

function stripEl(view, items, people, host) {
  const pv = people.map((p) => {
    const its = p.items;
    const popped = its.some((i) => i.popped);
    const audible = audibleFor(its);
    const badges = [
      its.length ? h("span", { class: "lv", text: t("ui.strip.live") }) : null,
      its.length > 1 ? h("span", { class: "cnt", title: t("ui.strip.sharing_n", { count: its.length }), text: String(its.length) }) : null,
      popped ? h("span", { class: "pop" }, ic("open_in_new")) : null,
    ];
    const props = { class: `pv${its.length ? " streaming" : ""}${popped ? " popped" : ""}${audible ? " audible" : ""}`, "data-user": p.name, "data-person": p.key };
    if (popped) Object.assign(props, { type: "button", "aria-label": t("ui.strip.bring_all_tip", { name: p.name }), onclick: () => bringBack(its.filter((i) => i.popped)) });
    else props.role = "listitem";
    return h(popped ? "button" : "div", props, avatarEl(p.name, p.emoji, { kids: badges }), h("span", { class: "nm", text: p.you ? t("ui.room.you") : p.name }));
  });
  return h("div", { class: "stripwrap" }, h("div", { class: "strip", role: "list" }, pv));
}

/** Bring popped-out streams back to the stage (the strip's avatar and the hover card's Bring back). */
function bringBack(its) {
  for (const i of its) if (i.w && i.w.popped) closePopout(i.w);
  A.render();
}

function cardEl() {
  const C = S.card;
  const view = viewOf(C.view);
  if (!view) return null;
  const items = collect(view);
  const p = peopleOf(view, items).find((x) => x.key === C.person);
  if (!p) return null;
  const its = p.items;
  const cross = view.kind === "watched";
  const name = p.you ? t("ui.room.you") : p.name;
  const sub = !its.length ? t("ui.strip.not_sharing") : its.length === 1 ? t("ui.strip.sharing_one") : t("ui.strip.sharing_n", { count: its.length });
  const hd = h("div", { class: "hd" }, avatarEl(p.name, p.emoji), h("div", {}, h("b", { text: name }), h("small", { text: sub })), h("div", { class: "fl" }),
    its.length ? h("span", { class: "badge badge-live" }, h("span", { class: "dot" }), t("ui.pill.live")) : null);
  const rows = its.map((it) => {
    const audio = it.kind === "audio" || (it.mine ? !!it.c.audio : !it.cam && it.r.hasAudio(it.s.owner));
    const status = it.inPip ? [ic("picture_in_picture"), t("ui.strip.in_picture")]
      : it.mine ? (it.cam ? [ic("videocam"), t("ui.strip.your_webcam")] : [ic("cast"), t("ui.strip.your_stream")])
        : it.popped ? [ic("open_in_new"), t("ui.strip.popped")]
          : it.kind === "audio" ? (it.w ? [ic("volume_up"), t("ui.strip.listening")] : [ic("visibility_off"), t("ui.strip.not_watching")])
            : it.w ? [ic("visibility"), t("ui.strip.on_stage")] : [ic("visibility_off"), t("ui.strip.not_watching")];
    const watching = it.mine ? viewersOf(it.c, it.r) : null;
    const extra = [cross ? it.r.name : null, watching ? t("ui.strip.watching_n", { count: watching.length }) : null].filter(Boolean).join(" · ");
    return h("div", { class: "ust" },
      h("div", { class: "l1" }, h("span", { class: "txt", text: it.label }), h("div", { class: "fl" }),
        h("span", { class: "au" + (audio ? " on" : ""), "data-audio": audio ? "1" : "0" }, ic(audio ? "volume_up" : "volume_off"), audio ? t("ui.strip.audio") : t("ui.strip.no_audio")),
        it.popped ? h("button", { type: "button", class: "back", title: t("ui.strip.bring_back_tip"), onclick: () => bringBack([it]) }, ic("open_in_new"), t("ui.strip.bring_back")) : null),
      h("div", { class: "l2 txt", text: itemNumbers(it) }),
      h("div", { class: "l3" }, status, extra ? h("span", { text: "· " + extra }) : null));
  });
  return h("div", { class: "utip" + (its.length ? " multi" : ""), role: "tooltip" }, hd, rows);
}
/** A person's cell in the strip of `doc`. By `data-person` (room + ConnId), never by `data-user`: that is the name, and two people may have one. */
const avatarOf = (doc, person) => [...doc.querySelectorAll(".pv[data-person]")].find((e) => e.dataset.person === person);
function positionCard(win) {
  const doc = win.document;
  const tip = doc.querySelector(".utip");
  if (!tip || !S.card) return;
  const av = avatarOf(doc, S.card.person);
  if (!av) {
    S.card = null;
    tip.remove();
    return;
  }
  const r = av.querySelector(".avatar").getBoundingClientRect();
  const x = Math.max(8, Math.min(r.left + r.width / 2 - tip.offsetWidth / 2, win.innerWidth - tip.offsetWidth - 8));
  tip.style.left = x + "px";
  tip.style.top = Math.max(8, r.top - tip.offsetHeight - 12) + "px";
}

/** Pointer geometry for the hover card (native `card_keeps`): opens on a person's cell; stays while the pointer is on the card, the avatar or the gap straight above it. */
function cardPointer(e, host) {
  if (drag) return;
  const doc = host.doc;
  const over = e.target.closest && e.target.closest(".pv[data-person]");
  const q = { x: e.clientX, y: e.clientY };
  if (S.card) {
    const tip = doc.querySelector(".utip");
    const av = avatarOf(doc, S.card.person);
    const keeps = tip && av && G.cardKeeps(tip.getBoundingClientRect(), av.querySelector(".avatar").getBoundingClientRect(), q);
    if (keeps && !(over && over.dataset.person !== S.card.person)) return;
    if (!over || over.dataset.person !== S.card.person) {
      S.card = null;
      if (!over) return A.render();
    }
  }
  if (over && !S.card && !(chromeOn && !chromeShown)) {
    const stg = over.closest(".stg");
    S.card = { person: over.dataset.person, view: stg ? stg.dataset.view : null, hostId: host.id };
    A.render();
  }
}

/* ============================== layout ============================== */

const sameCounts = (a, b) => a.length === b.length && a.every((x, i) => x === b[i]);
export function layoutArena(ar) {
  const m = ar._m;
  if (!m) return;
  const layout = layoutOf(m.key);
  const crop = layout === "crop";
  const float = layout === "float";
  const g = vg(m.key);
  const tiles = new Map([...ar.querySelectorAll(":scope > .st-tile")].map((x) => [x.dataset.id, x]));
  const ids = g.order.filter((id) => tiles.has(id));
  if (!ids.length) return;
  const W = ar.clientWidth;
  const H = ar.clientHeight;
  if (W < 8 || H < 8) return;
  const ars = ids.map((id) => parseFloat(tiles.get(id).style.getPropertyValue("--ar")) || 16 / 9);
  const counts = G.bestRows(ars, W, H);
  if (!g.grid || !sameCounts(g.grid.counts, counts)) g.grid = G.evenGrid(counts);
  const placed = G.place(g.grid, ars, W, H, crop);
  const split = crop ? G.place(g.grid, ars, W, H, false).pics : placed.pics;
  ar._ars = ars;
  ar._ids = ids;
  ar._W = W;
  ar._H = H;
  ids.forEach((id, i) => {
    const el = tiles.get(id);
    let q;
    if (float) {
      // Floating starts where Split showed the picture; a window always has its stream's aspect ratio, whatever layout or frame size it came from.
      const f = (g.float[id] = G.fitFloat(g.float[id] || { x: split[i].x / W, y: split[i].y / H, w: split[i].w / W, h: split[i].h / H }, ars[i], W, H));
      q = { x: f.x * W, y: f.y * H, w: f.w * W, h: f.h * H };
      el.style.zIndex = String(1 + Math.max(0, g.z.indexOf(id)));
    } else {
      q = placed.pics[i];
      el.style.zIndex = "";
    }
    el.style.left = q.x + "px";
    el.style.top = q.y + "px";
    el.style.width = q.w + "px";
    el.style.height = q.h + "px";
    const pip = el.querySelector(":scope > .pip");
    if (pip) placePip(pip);
  });
  // the resize bars (Split, Crop), reused by key so a drag keeps its element
  const guts = (ar._guts ||= new Map());
  const bars = (ar._bars = new Map());
  if (!float) for (const { bar, rect } of placed.bars) bars.set(G.barKey(bar), { bar, rect });
  for (const [k, el] of guts) if (!bars.has(k)) { el.remove(); guts.delete(k); }
  for (const [k, { bar, rect }] of bars) {
    let el = guts.get(k);
    if (!el) {
      el = h("div", { class: "gut " + bar.t, "data-bar": k, title: t("ui.stage.bar_tip") });
      el.addEventListener("pointerdown", (e) => gutPointerDown(e, ar, k));
      el.addEventListener("dblclick", () => {
        vg(m.key).grid = G.evenGrid(vg(m.key).grid.counts); // double-click a bar: an even grid again
        layoutArena(ar);
      });
      guts.set(k, el);
      ar.appendChild(el);
    }
    el.classList.toggle("on", !!drag && drag.kind === "bar" && drag.key === k);
    el.style.left = rect.x + "px";
    el.style.top = rect.y + "px";
    el.style.width = rect.w + "px";
    el.style.height = rect.h + "px";
  }
  fitTags(ar);
}

const TOOLS_RESERVE_PHONE = 156; // the round buttons are 40 px on a phone
const TOOLS_RESERVE = 119; // 16 px of margins + the three round buttons the hovered tile shows (native `TOOLS_RESERVE` + margins)
/** The stats in a name tag lose what does not fit, like native `stats_fields`: the resolution first (it changes least), then the bitrate
 *  (the frame rate is the health signal); the slots stay reserved for 9999x9999 / 999 fps / 99.9 Mbps so the pill never jitters. Measured
 *  with every slot shown for a moment, then `data-fit` tells the CSS which ones to hide. */
function fitTags(ar) {
  for (const tag of ar.querySelectorAll(".tag.stats")) {
    const tile = tag.closest(".st-tile");
    tag.dataset.fit = "all";
    tag.classList.add("measure");
    const slot = (c) => tag.querySelector(".v." + c).offsetWidth;
    const [res, fps, rate] = [slot("res"), slot("fps"), slot("kbps")];
    const base = tag.scrollWidth - res - fps - rate;
    tag.classList.remove("measure");
    const avail = tile.clientWidth - (PHONE.matches ? TOOLS_RESERVE_PHONE : TOOLS_RESERVE);
    tag.dataset.fit = base + res + fps + rate <= avail ? "all" : base + fps + rate <= avail ? "nores" : base + fps <= avail ? "fps" : "none";
  }
}

/** Run after every render (and by timers): position every arena, the overlays and the fullscreen chrome. */
export function afterRender(wins) {
  for (const a of arenas) {
    if (!a.isConnected) arenas.delete(a);
    else layoutArena(a);
  }
  for (const win of wins) {
    positionMenu(win);
    positionViewersPop(win);
    positionCard(win);
  }
  chromeSync();
}

// A stream's picture can change shape (a frame size change, a window resized by the sharer): re-layout when one does, so a floating
// window keeps its stream's aspect ratio without black bars.
setInterval(() => {
  for (const a of arenas) {
    if (!a.isConnected) continue;
    let changed = false;
    for (const el of a.querySelectorAll(":scope > .st-tile")) {
      if (!el._it) continue;
      const ar = itemAspect(el._it);
      if (Math.abs(ar - (parseFloat(el.style.getPropertyValue("--ar")) || 0)) > 0.002) {
        el.style.setProperty("--ar", ar.toFixed(4));
        changed = true;
      }
    }
    for (const el of a.querySelectorAll(":scope > .st-tile > .pip")) {
      const ar = itemAspect(el._cam); // the webcam's own picture can change shape too
      if (Math.abs(ar - (parseFloat(el.dataset.ar) || 0)) > 0.002) {
        el.dataset.ar = ar.toFixed(4);
        changed = true;
      }
    }
    if (changed && !drag) layoutArena(a);
  }
}, 400);

/* ============================== dragging ============================== */

const raise = (g, id) => {
  g.z = g.z.filter((k) => k !== id).concat(id);
};

function gutPointerDown(e, ar, k) {
  if (e.button !== 0) return;
  const b = ar._bars.get(k);
  if (!b) return;
  const g = vg(ar._m.key);
  drag = { kind: "bar", key: k, bar: b.bar, arena: ar, start: G.cloneGrid(g.grid), startCenter: G.barCenter(b.bar, b.rect), sx: e.clientX, sy: e.clientY };
  ar.classList.add("dragging");
  layoutArena(ar);
  chromeSync();
  e.preventDefault();
}

function tilePointerDown(e, tile, ar) {
  if (e.button !== 0 || e.target.closest("button, input, a, .viewers, .pip")) return;
  const key = ar._m.key;
  const g = vg(key);
  const id = tile.dataset.id;
  const float = layoutOf(key) === "float";
  if (float) {
    raise(g, id);
    ar.classList.add("dragging"); // the windows follow the pointer: no glide (the replica's .nofx)
    layoutArena(ar);
  }
  drag = { kind: e.target.closest(".rz") ? "rz" : "tile", id, tile, arena: ar, sx: e.clientX, sy: e.clientY, moved: false, f: g.float[id] ? { ...g.float[id] } : null, over: null };
  chromeSync();
  e.preventDefault();
}

function dragMove(e) {
  if (!drag) return;
  const ar = drag.arena;
  if (!ar.isConnected) return endDrag();
  const dx = e.clientX - drag.sx;
  const dy = e.clientY - drag.sy;
  const W = ar.clientWidth;
  const H = ar.clientHeight;
  const key = ar._m.key;
  const g = vg(key);
  const crop = layoutOf(key) === "crop";
  if (drag.kind === "bar") {
    // The bar sits midway between the pictures, which move slower than the cell edge; search the split that puts it under the pointer.
    const want = drag.startCenter + (drag.bar.t === "v" ? dx : dy);
    g.grid = G.dragBar(drag.start, drag.bar, want, ar._ars, W, H, crop);
    layoutArena(ar);
    return;
  }
  if (!drag.moved && Math.hypot(dx, dy) < 4) return;
  drag.moved = true;
  if (layoutOf(key) === "float") {
    const o = drag.f;
    const i = ar._ids.indexOf(drag.id);
    if (!o || i < 0) return;
    g.float[drag.id] = drag.kind === "rz" ? G.resizeFloat(o, dx, dy, ar._ars[i], W, H) : G.moveFloat(o, dx, dy, W, H);
    layoutArena(ar);
    return;
  }
  if (drag.kind === "rz") return;
  // Reorder (Split, Crop): the tile follows the pointer; the one under it gets an accent ring.
  drag.dx = dx;
  drag.dy = dy;
  drag.tile.classList.add("dragging");
  drag.tile.style.transform = `translate(${dx}px, ${dy}px)`;
  const doc = ar.ownerDocument;
  const over = doc.elementsFromPoint(e.clientX, e.clientY).map((el) => el.closest(".arena > .st-tile")).find((el) => el && el !== drag.tile) || null;
  ar.querySelectorAll(".st-tile.drop").forEach((x) => x !== over && x.classList.remove("drop"));
  if (over) over.classList.add("drop");
  drag.over = over;
}

function endDrag() {
  if (!drag) return;
  const d = drag;
  drag = null;
  const ar = d.arena;
  if (ar.isConnected) {
    ar.classList.remove("dragging");
    if (d.kind === "tile" && d.tile) {
      d.tile.classList.remove("dragging");
      d.tile.style.transform = "";
      if (d.over && layoutOf(ar._m.key) !== "float") {
        d.over.classList.remove("drop");
        const g = vg(ar._m.key);
        const a = g.order.indexOf(d.id);
        const b = g.order.indexOf(d.over.dataset.id);
        if (a >= 0 && b >= 0) [g.order[a], g.order[b]] = [g.order[b], g.order[a]];
      }
    }
    layoutArena(ar);
  }
  chromeSync();
  A.flush();
}

/* ============================== fullscreen of the stage + its auto-hiding chrome ============================== */
// The rule is chrome.js (a port of the native `stage/chrome.rs`): in fullscreen the strip and bar lie over the streams (so streams never
// jump when they fade) and hide when the pointer leaves the stage and after 3 s without movement while on it; movement brings them
// back; an open menu, modal or a drag holds them up, and when that ends they get a full 3 s again.

let cs = null; // chrome state; null outside fullscreen
let chromeOn = false;
let chromeShown = true;
let chromeTimer = 0;
let pointerInside = true;

function chromeApply(doc) {
  const w = doc.querySelector(".stagefs");
  if (w) w.classList.toggle("chrome-off", !chromeShown);
  if (!chromeShown && S.card) {
    S.card = null;
    doc.querySelectorAll(".utip").forEach((x) => x.remove());
  }
}
/** Step the rule: `active` = the pointer moved or pressed, the wheel turned or a key was pressed since the last step. */
export function chromeSync(active = false) {
  const fs = S.stage.fullscreen;
  clearTimeout(chromeTimer);
  if (!fs) {
    cs = null;
    chromeOn = false;
    chromeShown = true;
    return;
  }
  const now = performance.now();
  if (!cs) cs = chromeStart(now);
  chromeOn = true;
  const held = !!(S.menu || S.modal || drag);
  let shown;
  [cs, shown] = chromeStep(cs, now, pointerInside, active, held);
  chromeShown = shown;
  chromeApply(document);
  const rest = chromeRemaining(cs, now);
  if (rest != null) chromeTimer = setTimeout(() => chromeSync(false), rest + 20);
}
/** Ask for fullscreen of the app root (needs the click's user gesture); the tree renders only the stage meanwhile. `Why #app and not the stage:` render() rebuilds
 *  the stage on every state change, and a fullscreen element removed from the DOM drops fullscreen, while #app itself persists. */
export function toggleStageFullscreen() {
  const root = document.getElementById("app");
  if (S.stage.fullscreen) {
    S.stage.fullscreen = false;
    if (document.fullscreenElement) document.exitFullscreen().catch(() => {});
    chromeSync();
    return A.render();
  }
  S.stage.fullscreen = true;
  cs = null;
  A.render();
  if (!root.requestFullscreen) return; // iPhone Safari: no element fullscreen; the stage-only layout is all there is (the bar's button leaves it)
  root.requestFullscreen().catch(() => {}); // refused (iPhone browsers reject too): the stage-only layout stays as the fullscreen, the bar's button leaves it
}
function onFullscreenChange() {
  const fe = document.fullscreenElement;
  if (fe && fe.classList && fe.classList.contains("pip-stage")) return; // a stream's own fullscreen took over
  if (S.stage.fullscreen && fe !== document.getElementById("app")) {
    S.stage.fullscreen = false;
    chromeSync();
    A.render();
  }
}

/* ============================== window listeners ============================== */

const bound = new WeakSet();
/** Attach the stage's document-level listeners to a window (the main one at boot, the pop-out window when it opens). */
export function bindWindow(win, hostId) {
  if (bound.has(win)) return;
  bound.add(win);
  const host = { id: hostId, win, doc: win.document };
  const mv = (e) => {
    pointerInside = true;
    if (!pipPointerMove(e)) dragMove(e);
    cardPointer(e, host);
    vpopPointer(e, host);
    if (S.stage.fullscreen && hostId === "main") chromeSync(true);
  };
  win.addEventListener("pointermove", mv);
  const up = (e) => {
    if (!pipPointerUp(e)) endDrag();
  };
  win.addEventListener("pointerup", up);
  // Touch has no hover: a tap on a person opens their card (and a tap elsewhere closes it), a tap on the viewers chip opens its list.
  win.addEventListener("pointerdown", (e) => {
    if (e.pointerType === "mouse") return;
    cardPointer(e, host);
    vpopPointer(e, host);
  }, true);
  win.addEventListener("pointercancel", up);
  win.addEventListener("pointerdown", () => S.stage.fullscreen && hostId === "main" && chromeSync(true), true);
  win.addEventListener("wheel", () => S.stage.fullscreen && hostId === "main" && chromeSync(true), { passive: true, capture: true });
  win.document.documentElement.addEventListener("mouseleave", () => {
    pointerInside = false;
    if (S.card) {
      S.card = null;
      A.render();
    }
    if (S.stage.fullscreen && hostId === "main") chromeSync();
  });
  // A menu closes when anything outside it is clicked; a second click on the control that opened it closes it too (and must not re-open it).
  win.document.addEventListener("click", (e) => {
    if (!S.menu || S.menu.hostId !== hostId || performance.now() < lp.until) return;
    if (e.target.closest && e.target.closest(".menu")) return;
    const op = e.target.closest && e.target.closest("[data-mk]");
    if (op && op.dataset.mk === S.menu.mk) {
      e.stopPropagation();
      e.preventDefault();
    }
    S.menu = null;
    setTimeout(() => A.render(), 0);
  }, true);
  win.document.addEventListener("keydown", (e) => {
    if (S.stage.fullscreen && hostId === "main") chromeSync(true);
    if (e.key !== "Escape") return;
    if (S.menu && S.menu.hostId === hostId) {
      // Escape in a dropdown closes the dropdown alone, never the modal under it (found in the native client, D1)
      e.stopPropagation();
      e.preventDefault();
      closeMenu();
    }
  }, true);
  if (hostId === "main") document.addEventListener("fullscreenchange", onFullscreenChange);
  else win.addEventListener("resize", () => invalidate()); // the stage window: the arena follows its size (the main window's resize is handled by app.js)
}

/** The overlays of host `hostId`, built after the tree: the menu, the hover card, the viewers dropdown. */
export function overlays(hostId) {
  const out = [renderMenu(hostId)];
  if (S.card && S.card.hostId === hostId) {
    const c = cardEl();
    if (c) out.push(c);
    else S.card = null;
  }
  if (vpop && vpop.hostId === hostId) {
    const v = viewersPopEl();
    if (v) out.push(v);
    else vpop = null;
  }
  return out.filter(Boolean);
}

// Closes the viewers dropdown as soon as the pointer is over neither the chip nor the dropdown (a document-level check: every render()
// replaces both, and a replaced element never gets its mouseleave).
function vpopPointer(e) {
  if (vpop && !(e.target.closest && e.target.closest(".vpop, .viewers"))) {
    vpop = null;
    A.render();
  }
}
