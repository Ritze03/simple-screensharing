// One RoomEntry per saved room: its RoomClient (one WebTransport connection), connection state, roster, streams,
// previews and the streams we watch there. Turns core events into UI state and asks for a re-render.

import { RoomClient, parseJoinLink, formatJoinLink, stripLinkName, ConnectError, JoinRejectedError, JoinLinkError } from "../core/index.js";
import { S, invalidate, toast, persistRooms, displayName } from "./store.js";
import { t, DICT } from "./i18n.js";
import { onShareEvent, onShareStats, stopDestsInRoom } from "./capture.js";
import { closePopout } from "./popout.js";

export class RoomEntry {
  constructor({ id, name, link, auto = true }) {
    const p = parseJoinLink(link);
    this.id = id || `${p.roomIdHex}@${p.server}`;
    this.name = name || t("ui.room.unnamed", { id: p.roomIdHex.slice(0, 8) });
    // The name is a local label, never part of the stored link (like native's saved join string).
    this.link = stripLinkName(link);
    this.server = p.server;
    this.auto = auto;
    this.client = null;
    this.state = "disconnected"; // disconnected | connecting | joined | reconnecting
    this.attempt = 0;
    this.error = null; // last connect error text (shown in the disconnected view)
    this.issue = null; // why the last reconnect attempt failed: "unreachable" | "full" | null
    this.members = [];
    this.streams = [];
    this.previews = new Map(); // key -> object URL
    this.watches = new Map(); // key -> Watch
    this.resume = []; // watches that ended recently, resumed when the same sharer's stream reappears
  }
  /** The link to send to friends: the stored link plus this room's name (5th field), unless it is only the "Room abcd1234" placeholder. */
  inviteLink() {
    const p = parseJoinLink(this.link);
    const placeholder = Object.values(DICT).some((d) => d["ui.room.unnamed"].replace("%{id}", p.roomIdHex.slice(0, 8)) === this.name);
    return formatJoinLink(p.server, p.roomId, p.roomKey, p.certHash, placeholder ? null : this.name);
  }
  get selfId() {
    return this.client ? this.client.selfId : null;
  }
  videoStreams() {
    return this.streams.filter((s) => s.kind === "video" && !s.self);
  }
  hasAudio(ownerId) {
    return this.streams.some((s) => s.owner === ownerId && s.kind === "audio");
  }
  memberName(id) {
    const m = this.members.find((x) => x.id === id);
    return m ? m.name || "?" : "?";
  }
}

export const roomById = (id) => S.rooms.find((r) => r.id === id) || null;

/** Human text for a failed connect. */
export function connectErrorText(e) {
  if (e instanceof JoinRejectedError) return e.reason === "RoomFull" ? t("ui.err.room_full") : e.message;
  if (e instanceof ConnectError) {
    if (e.mayBeExpiredLink) return t("ui.err.unreachable_or_expired");
    if (e.reason === "unreachable") return t("ui.err.unreachable");
    if (e.code === "join-no-reply") return t("ui.err.join_no_reply");
    if (e.code === "no-webtransport") return t("ui.err.no_webtransport");
    return e.message;
  }
  if (e instanceof JoinLinkError) return t("ui.err.bad_link", { error: e.message });
  return String((e && e.message) || e);
}

export async function connectRoom(r, { quiet = false } = {}) {
  if (r.client && r.state !== "disconnected") return;
  const st = S.settings;
  const client = new RoomClient({ displayName: displayName(), maxJitterBufferMs: st.jitter, catchUpSpeedPercent: st.catchup });
  r.client = client;
  r.state = "connecting";
  r.error = null;
  r.issue = null;
  wire(r, client);
  invalidate();
  try {
    await client.connect(r.link);
    if (r.client !== client) return; // left meanwhile
    if (!quiet) toast(t("ui.toast.connected", { room: r.name }), "check_circle");
  } catch (e) {
    if (r.client !== client) return;
    r.client = null;
    r.state = "disconnected";
    r.error = connectErrorText(e);
    if (!quiet) toast(t("ui.toast.connect_failed", { room: r.name }), "error", { err: true });
  }
  invalidate();
}

export async function disconnectRoom(r) {
  const client = r.client;
  stopDestsInRoom(r.id);
  for (const key of [...r.watches.keys()]) unwatchStream(r, key);
  r.client = null;
  r.state = "disconnected";
  r.resume = [];
  r.members = [];
  r.streams = [];
  clearPreviews(r);
  invalidate();
  if (client) await client.disconnect().catch(() => {});
}

/** "Retry now" during a reconnect: the core skips its backoff wait; shares and watches stay registered and come back with it. */
export async function retryNow(r) {
  if (!r.client) return connectRoom(r);
  try {
    await r.client.reconnectNow();
  } catch (e) {
    toast(t("ui.toast.connect_failed", { room: r.name }), "error", { err: true });
  }
}

function clearPreviews(r) {
  for (const url of r.previews.values()) URL.revokeObjectURL(url);
  r.previews.clear();
}

function wire(r, client) {
  const on = (type, fn) => client.addEventListener(type, (e) => r.client === client && fn(e.detail));
  on("state", (d) => {
    const map = { connecting: "connecting", connected: "joined", reconnecting: "reconnecting", closed: "disconnected", idle: "disconnected" };
    r.state = map[d.state] || r.state;
    r.attempt = d.attempt || 0;
    if (d.state === "connected") {
      r.issue = null;
      resumeWatches(r);
    }
    if (d.state === "connected" && d.reconnected) toast(t("ui.toast.reconnected", { room: r.name }), "check_circle");
    invalidate();
  });
  on("roster", (d) => {
    r.members = d.members;
    invalidate();
  });
  on("streams", (d) => {
    r.streams = d.streams;
    for (const key of [...r.previews.keys()]) {
      if (!d.streams.some((s) => s.key === key)) {
        URL.revokeObjectURL(r.previews.get(key));
        r.previews.delete(key);
      }
    }
    resumeWatches(r);
    invalidate();
  });
  on("preview", (d) => {
    const old = r.previews.get(d.key);
    if (old) URL.revokeObjectURL(old);
    r.previews.set(d.key, URL.createObjectURL(d.blob));
    invalidate();
  });
  on("stats", (d) => {
    for (const [key, st] of Object.entries(d.watching || {})) {
      const w = r.watches.get(key);
      if (w && st) w.stats = st;
    }
    onShareStats(r, d.sharing || []);
    window.dispatchEvent(new CustomEvent("sss-stats"));
  });
  on("watch-ended", (d) => {
    const w = r.watches.get(d.key);
    if (!w) return;
    endWatch(r, w);
    // A reconnect (theirs or ours, e.g. a relay restart) gives the sharer a fresh ConnId, so the same share comes back
    // under a new stream key. The core cannot tell that from a stop + restart, so remember the stream's stableKey for
    // a minute and resume watching when the same one reappears (the policy lives here, the identity comes from the core).
    const entry = { stableKey: d.stableKey, ownerName: w.ownerName, label: w.label, vol: w.vol, muted: w.muted, until: Date.now() + RESUME_MS };
    r.resume.push(entry);
    setTimeout(() => {
      if (r.resume.includes(entry)) toast(t("ui.toast.watch_ended", { name: w.ownerName }), "tv_off");
    }, 4000);
    invalidate();
  });
  on("media", (d) => {
    const w = r.watches.get(d.key);
    if (!w) return;
    if (d.type === "codec") w.codec = d.codec || w.codec;
    else if (d.type === "unsupported") w.error = t("ui.tile.unsupported", { codec: String(d.codec || "").toUpperCase() || "?" });
    invalidate();
  });
  on("share", (d) => onShareEvent(r, d));
  on("subscribers", () => invalidate());
  on("error", (d) => {
    if (d.code === "link-expired" || d.code === "relay-unreachable") {
      r.issue = "unreachable";
      invalidate();
    } else if (d.code === "room-full") {
      r.issue = "full";
      invalidate();
    } else if (d.code === "encoder-error") {
      toast(t("ui.toast.encoder_error", { error: d.message }), "error", { err: true });
    } else if (d.code === "audio-share-unavailable" || d.code === "audio-encoder-error") {
      toast(t("ui.toast.audio_failed", { error: d.message }), "error", { err: true });
    } else if (d.code === "join-rejected") {
      r.error = t("ui.err.room_full");
      invalidate();
    }
  });
}

/* ---------------- watching ---------------- */

const RESUME_MS = 60_000;

function resumeWatches(r) {
  const now = Date.now();
  r.resume = r.resume.filter((e) => e.until > now);
  if (!r.resume.length || r.state !== "joined") return;
  for (const e of [...r.resume]) {
    const s = r.streams.find((x) => x.kind === "video" && !x.self && !r.watches.has(x.key) && (e.stableKey ? x.stableKey === e.stableKey : (x.ownerName || r.memberName(x.owner)) === e.ownerName && x.label === e.label));
    if (!s) continue;
    r.resume.splice(r.resume.indexOf(e), 1);
    const w = watchStream(r, s.key);
    if (w) {
      if (e.vol !== 1) setWatchVolume(r, w, e.vol);
      if (e.muted) setWatchMuted(r, w, true);
    }
  }
}

export function watchStream(r, key) {
  if (!r.client || r.watches.has(key)) return r.watches.get(key);
  const info = r.streams.find((s) => s.key === key);
  if (!info) return null;
  const canvas = document.createElement("canvas");
  canvas.width = 16;
  canvas.height = 9;
  const w = {
    key, roomId: r.id, owner: info.owner, ownerName: info.ownerName || r.memberName(info.owner), label: info.label,
    canvas, handle: null, vol: 1, muted: false, popped: null, stats: null, codec: null, error: null, firstFrame: false,
  };
  try {
    r.client.resumeAudio();
    w.handle = r.client.watch(key, { canvas });
    if (w.handle.renderer) {
      w.handle.renderer.onDraw = () => {
        if (!w.firstFrame) {
          w.firstFrame = true;
          w.handle.renderer.onDraw = null;
          invalidate();
        }
      };
    }
  } catch (e) {
    toast(t("ui.toast.watch_failed", { error: e.message }), "error", { err: true });
    return null;
  }
  r.watches.set(key, w);
  invalidate();
  return w;
}

function endWatch(r, w) {
  r.watches.delete(w.key);
  if (w.popped) closePopout(w, { silent: true });
  w.popped = null;
}

export function unwatchStream(r, key, { silent = false } = {}) {
  const w = r.watches.get(key);
  if (!w) return;
  endWatch(r, w);
  if (r.client) r.client.unwatch(key);
  if (!silent) invalidate();
}

export function setWatchVolume(r, w, vol) {
  w.vol = vol;
  if (r.client) r.client.setVolume(w.key, vol);
}
export function setWatchMuted(r, w, muted) {
  w.muted = muted;
  if (r.client) r.client.setMuted(w.key, muted);
}

/** Every watch across every room, in room order. */
export function allWatches() {
  const out = [];
  for (const r of S.rooms) for (const w of r.watches.values()) out.push({ r, w });
  return out;
}

/* ---------------- room list ---------------- */

export function addRoom({ name, link, auto = true }) {
  const entry = new RoomEntry({ name, link, auto });
  const existing = roomById(entry.id);
  if (existing) return { entry: existing, existed: true };
  S.rooms.push(entry);
  persistRooms();
  return { entry, existed: false };
}

export async function forgetRoom(r) {
  await disconnectRoom(r);
  S.rooms = S.rooms.filter((x) => x !== r);
  persistRooms();
}

export function applyJitterSettings() {
  for (const r of S.rooms) if (r.client) r.client.setJitterSettings({ maxJitterBufferMs: S.settings.jitter, catchUpSpeedPercent: S.settings.catchup });
}
