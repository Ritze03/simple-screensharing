// RoomClient: one connection to one room. See README.md for the full API and event list.

import { AudioReceiver, AudioSender, MAX_VOLUME } from "./audio.js";
import { AvSync } from "./avsync.js";
import { NonceSpaceExhausted, decryptFrame, decryptPreview, encryptFrame, encryptPreview, importRoomKey } from "./crypto.js";
import { Reassembler, fragmentFrame, pacePlan } from "./fragment.js";
import { HELLO_DATAGRAM, HELLO_INTERVAL_MS, HELLO_MAX_TRIES, SendRing, nackEnabledByDefault } from "./nack.js";
import { JoinLinkError, parseJoinLink } from "./joinlink.js";
import { PREVIEW_FIRST_DELAY_MS, PREVIEW_INTERVAL_MS, PreviewSampler } from "./preview.js";
import {
  STREAM_TYPE_PREVIEW, WIRE_VERSION, clampUtf8, connIdFromHex, connIdToHex, packPreviewHeader, unpackMediaHeader, unpackPreviewHeader,
  MEDIA_FLAG_KEYFRAME, MEDIA_FLAG_STALLED, NACK_TAG_HELLO_ACK, NACK_TAG_MISS, NACK_TAG_REQUEST, isNackDatagram, packNackRange, unpackNackRange,
  AVATAR_TAG_ACK, AVATAR_TAG_HELLO, AVATAR_TAG_LIST, packAvatarSet, unpackAvatars,
} from "./protocol.js";
import { ConnectError, JoinRejectedError, RelaySession, classifyClose, supportsWebTransport } from "./session.js";
import { CanvasRenderer, VideoReceiver } from "./video-rx.js";
import { ENCODER_PRESETS, MediaClock, VideoSender, probeEncoders } from "./video-tx.js";

const STATS_INTERVAL_MS = 1000;
const MAX_TX_QUEUE = 8; // frames waiting for encrypt+send per stream before we drop
const MAX_PENDING_DATAGRAMS = 400; // ~400 KB queued inside WebTransport; more than that is latency, not throughput
const MAX_RX_QUEUE = 64;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const RESUBSCRIBE_MIN_INTERVAL_MS = 1500;
// With keyframes this close together the next one arrives by itself. Asking anyway costs an extra IDR on the wire, a moment off
// the relay's route and, on the sharer, a "left" and "joined" sound (the viewer list changes twice). `waiting` still asks.
const KEYFRAME_SOON_MS = 500;
const FIRST_RECONNECT_MS = 50;
const BACKOFF_MS = [500, 1000, 2000, 4000, 8000, 10000];
// Viewer lists (spec section 9.1): 1-byte hello datagram to the relay, its 1-byte ack and the per-stream list it sends our way.
const VIEWERS_TAG_HELLO = 0xe6;
const VIEWERS_TAG_ACK = 0xe7;
const VIEWERS_TAG_LIST = 0xe8;
const VIEWERS_STALE_MS = 7000; // the relay repeats a non-empty list every 2 s
const AVATAR_HELLO_MS = 1000; // emoji avatars (spec 9.2): Hello once a second, at most 10 times, until the relay's Ack
const AVATAR_SET_MS = 250; // after the Ack: re-send Set until the relay's list shows our own entry (the Set's only ack)
const NACK_TICK_MS = 5; // poll the reassembler this often while a frame is incomplete (native: 5 ms too)
const NACK_DEFAULT_RTT_MS = 150; // when the browser does not report a smoothed RTT (WebTransport getStats)

export const streamKey = (ownerHex, streamId) => `${ownerHex}:${streamId}`;

/** A webcam is an ordinary video stream whose label starts with "Webcam" (plan D7; native `capture::webcam::is_webcam_label`). */
export const WEBCAM_LABEL = "Webcam";
export const isWebcamLabel = (label) => String(label ?? "").startsWith(WEBCAM_LABEL);
const parseKey = (key) => {
  const [owner, sid] = String(key).split(":");
  return { owner, streamId: Number(sid) };
};

/** What this browser can do, for the UI to gate buttons. */
export function capabilities() {
  const c = {
    webTransport: supportsWebTransport(),
    webCodecsDecode: typeof VideoDecoder !== "undefined",
    webCodecsEncode: typeof VideoEncoder !== "undefined",
    audioCodecs: typeof AudioDecoder !== "undefined" && typeof AudioEncoder !== "undefined",
    trackProcessor: typeof MediaStreamTrackProcessor !== "undefined",
    getDisplayMedia: !!(navigator.mediaDevices && navigator.mediaDevices.getDisplayMedia),
    secureContext: typeof isSecureContext === "undefined" ? true : isSecureContext,
  };
  c.canWatch = c.webTransport && c.webCodecsDecode && c.secureContext;
  c.canShare = c.canWatch && c.webCodecsEncode && c.trackProcessor && c.getDisplayMedia;
  return c;
}

export { probeEncoders };

function emitError(client, code, error, { fatal = false } = {}) {
  client.dispatchEvent(new CustomEvent("error", { detail: { code, message: error && error.message ? error.message : String(error), fatal, error } }));
}

class RxStream {
  constructor(client, key, info, impl) {
    this.client = client;
    this.key = key;
    this.info = info; // { owner (hex), ownerId (bigint), streamId, kind }
    this.impl = impl; // VideoReceiver | AudioReceiver
    this.queue = [];
    this.busy = false;
    this.decryptFailures = 0;
    this.stalled = false;
    this.lastResubscribe = -Infinity;
  }
  enqueue(op) {
    if (this.queue.length >= MAX_RX_QUEUE) {
      this.queue.length = 0;
      this.queue.push({ type: "loss" });
    }
    this.queue.push(op);
    if (!this.busy) this.#drain();
  }
  async #drain() {
    this.busy = true;
    try {
      while (this.queue.length) {
        const op = this.queue.shift();
        if (op.type === "loss") {
          if (this.impl.markLoss) this.impl.markLoss();
          continue;
        }
        try {
          const { ptsUs, payload } = await decryptFrame(this.client._key, this.info.ownerId, op.frameId, this.info.streamId, op.ciphertext);
          this.stalled = op.stalled;
          if (this.info.kind === "video") this.impl.pushFrame({ ptsUs, keyframe: op.keyframe, payload, stalled: op.stalled, frameId: op.frameId });
          else this.impl.pushPacket({ ptsUs, payload });
        } catch (e) {
          this.decryptFailures++;
          if (this.impl.markLoss) this.impl.markLoss();
        }
      }
    } finally {
      this.busy = false;
    }
  }
}

/** Handle returned by `watch()`. */
export class WatchHandle {
  constructor(client, key, audioKey) {
    this.client = client;
    this.key = key;
    this.audioKey = audioKey; // sibling audio stream key being auto-played, or null
    this.renderer = null;
    this.onFrame = null;
  }
  /** Render this stream on a canvas (one draw per display refresh, newest frame wins). */
  attachToCanvas(canvas) {
    this.detach();
    this.renderer = new CanvasRenderer(canvas);
    return this.renderer;
  }
  detach() {
    if (this.renderer) this.renderer.stop();
    this.renderer = null;
    if (this._trackWriter) {
      this._trackWriter.close().catch(() => {});
      this._trackWriter = null;
      this._track = null;
      this.onFrame = null;
    }
  }
  /**
   * Optional alternative to the canvas: a live video `MediaStreamTrack` of the decoded frames (for a `<video>` element,
   * Picture-in-Picture, recording). It goes through the browser's own video pipeline, which adds latency and frame
   * pacing of its own: prefer `attachToCanvas` for latency-critical viewing. Chromium-only (`MediaStreamTrackGenerator`).
   */
  videoTrack() {
    if (this._track) return this._track;
    if (typeof MediaStreamTrackGenerator === "undefined") throw new Error("MediaStreamTrackGenerator is not available in this browser");
    if (this.renderer) {
      this.renderer.stop();
      this.renderer = null;
    }
    const gen = new MediaStreamTrackGenerator({ kind: "video" });
    const writer = gen.writable.getWriter();
    this._trackWriter = writer;
    this._track = gen;
    this.onFrame = (frame) => writer.write(frame).catch(() => frame.close()); // write() takes ownership and closes the frame
    return gen;
  }
  /** Receive raw VideoFrames instead. The callback OWNS each frame and must frame.close() it. */
  setFrameCallback(fn) {
    this.onFrame = fn;
  }
  _deliver(frame) {
    if (this.renderer) this.renderer.present(frame);
    else if (this.onFrame) this.onFrame(frame);
    else frame.close();
  }
  setVolume(v) {
    this.client.setVolume(this.key, v);
  }
  setMuted(m) {
    this.client.setMuted(this.key, m);
  }
  stats() {
    return this.client.watchStats(this.key);
  }
  stop() {
    this.client.unwatch(this.key);
  }
}

/** Handle returned by `startShare()`. */
export class ShareHandle {
  constructor(client, init) {
    this.client = client;
    Object.assign(this, init);
    this.subscribers = 0; // video stream's viewer count (from the relay); encoding pauses at 0
    this.audioSubscribers = 0;
    this.viewerIds = []; // hex ConnIds of the video stream's viewers (relay list); expires after VIEWERS_STALE_MS
    this.ended = false;
  }
  /** Display names of the people watching this share right now, or `null` when the relay does not report viewers (an older relay). */
  viewers() {
    return this.client.viewersSupported ? this.client._viewerNames(this) : null;
  }
  muteAudio(muted) {
    if (this.audioTrack) this.audioTrack.enabled = !muted;
    this.audioMuted = !!muted;
  }
  requestKeyframe() {
    this.sender.requestKeyframe();
  }
  setBitrate(kbps) {
    this.sender.setBitrate(kbps);
  }
  stats() {
    return this.client._shareStats(this);
  }
  stop() {
    return this.client.stopShare(this);
  }
}

export class RoomClient extends EventTarget {
  /**
   * @param {object} [o]
   * @param {string} [o.displayName]
   * @param {number} [o.maxJitterBufferMs=2000]  0..5000 (native setting `max_jitter_buffer_ms`)
   * @param {boolean} [o.levelLoudness=true]  viewer-side loudness levelling of watched audio (leveller.js)
   * @param {number} [o.catchUpSpeedPercent=5]   5..10  (native setting `catch_up_speed_percent`)
   * @param {boolean} [o.autoReconnect=true]
   * @param {"resubscribe"|"wait"} [o.lossRecovery="resubscribe"]
   * @param {"no-preference"|"prefer-hardware"|"prefer-software"} [o.hardwareAcceleration]
   * @param {number} [o.audioLatencyMs=80]
   * @param {AudioContext} [o.audioContext]
   * @param {boolean} [o.nack]  selective retransmission (spec §6.6); default on unless `?nack=0` / `localStorage["sss-nack"]="0"`
   */
  constructor({
    displayName = "Browser", maxJitterBufferMs = 2000, catchUpSpeedPercent = 5, levelLoudness = true, autoReconnect = true, lossRecovery = "resubscribe",
    hardwareAcceleration = "no-preference", audioLatencyMs = 80, audioContext = null, nack = nackEnabledByDefault(),
  } = {}) {
    super();
    this.displayName = displayName;
    this.maxJitterBufferMs = maxJitterBufferMs;
    this.catchUpSpeedPercent = catchUpSpeedPercent;
    this.levelLoudness = levelLoudness;
    this.autoReconnect = autoReconnect;
    this.lossRecovery = lossRecovery;
    this.hardwareAcceleration = hardwareAcceleration;
    this.nackEnabled = !!nack;
    this.nackAcked = false; // the relay answered our Hello on this session: it caches fragments and serves requests
    this.nackTimer = null;
    this.sendRing = new SendRing();
    this.audioLatencyMs = audioLatencyMs;
    this._audioContext = audioContext;

    this.state = "idle";
    this.link = null;
    this._key = null;
    this.session = null;
    this.selfId = null;
    this.members = new Map(); // hex -> { id, name, streamCount, self }
    this.viewersSupported = false; // the relay acked our viewers hello (reset per connection)
    this.avatarsSupported = false; // the relay acked our avatar hello (reset per connection)
    this.avatars = new Map(); // hex ConnId -> emoji: the relay's last whole list (spec 9.2), replaced by every list
    this._avatar = ""; // the emoji we want to show ("" = none), set by setAvatar() and kept across reconnects
    this._avatarTimer = null;
    this.streams = new Map(); // key -> info
    this.previews = new Map(); // key -> { blob, receivedAt }
    this.watches = new Map(); // key -> WatchHandle
    this.subs = new Map(); // key -> RxStream (every subscribed concrete stream)
    this.avSyncs = new Map(); // owner hex -> AvSync (shared by that owner's video and audio receivers)
    this.subRefs = new Map(); // key -> Set of reasons ("watch:<key>", "follow:<key>")
    this.volumes = new Map(); // audio key -> 0..5
    this.muted = new Map(); // audio key -> bool
    this.shares = [];
    this.reassembler = new Reassembler({ onDrop: (c, s) => this.#onFrameDropped(c, s) });
    this.tx = new Map(); // streamId -> { chain, queued, dropped, sentFrames, sentBytes }
    this.userClosed = false;
    this.attempt = 0;
    this.statsTimer = null;
    this.reconnectTimer = null;
    this.lastTransportStats = null;
  }

  // -------------------------------------------------------------------------------- connection

  /**
   * Join a room. Resolves once joined; rejects with JoinLinkError / ConnectError (check `.mayBeExpiredLink`)
   * / JoinRejectedError. After a successful first connect the client reconnects by itself.
   */
  async connect(link, displayName) {
    if (this.state !== "idle" && this.state !== "closed") throw new Error("already connected; call disconnect() first");
    if (displayName != null) this.displayName = displayName;
    this.link = typeof link === "string" ? parseJoinLink(link) : link;
    this._key = await importRoomKey(this.link.roomKey);
    this.userClosed = false;
    this.#setState("connecting");
    try {
      await this.#establish();
    } catch (e) {
      this.#setState("closed", { error: e });
      throw e;
    }
    this.attempt = 0;
    this.#setState("connected");
    this.#startStats();
  }

  async disconnect() {
    this.userClosed = true;
    clearTimeout(this.reconnectTimer);
    for (const s of [...this.shares]) this.stopShare(s);
    for (const key of [...this.watches.keys()]) this.unwatch(key);
    this.#stopStats();
    clearInterval(this._avatarTimer);
    this._avatarTimer = null;
    const sess = this.session;
    this.session = null;
    if (sess) await sess.close({ graceful: true });
    this.members.clear();
    this.streams.clear();
    this.#setState("closed", { reason: "disconnected" });
  }

  get connected() {
    return this.state === "connected";
  }

  /** The link for inviting others (same as the one joined). */
  get joinLink() {
    return this.link;
  }

  #setState(state, extra = {}) {
    this.state = state;
    this.dispatchEvent(new CustomEvent("state", { detail: { state, selfId: this.selfId, attempt: this.attempt, ...extra } }));
  }

  #openAbort = null;
  async #establish() {
    const link = this.link;
    const ac = (this.#openAbort = new AbortController());
    let sess;
    try {
      sess = await RelaySession.open({ server: link.server, certHash: link.certHash, signal: ac.signal });
    } finally {
      if (this.#openAbort === ac) this.#openAbort = null;
    }
    let snapshot;
    try {
      snapshot = await sess.join({ roomId: link.roomId, displayName: clampUtf8(this.displayName) });
    } catch (e) {
      sess.close({ graceful: false });
      throw e;
    }
    this.session = sess;
    this.selfId = connIdToHex(sess.connId);
    this.reassembler.clear();
    this.reassembler.setNack(false);
    this.nackAcked = false;
    this.sendRing.clear();
    this.tx.clear();
    this.#startHello(sess);
    sess.onControl = (m) => sess === this.session && this.#onControl(m);
    sess.onDatagram = (d) => sess === this.session && this.#onDatagram(d);
    sess.onPreviewStream = (b) => sess === this.session && this.#onPreviewStream(b);
    sess.onClosed = (info) => this.#onSessionClosed(sess, info);
    const before = new Map(this.streams); // infos of the streams we knew, for watch-ended after a reconnect
    this.#applySnapshot(snapshot);
    this.#startViewersHello(sess);
    this.#startAvatars(sess);
    // Re-establish what we had before a reconnect: subscriptions to streams that still exist (the receivers
    // restart from the next keyframe: datagrams were lost meanwhile), and forget the ones that are gone.
    for (const [key, reasons] of [...this.subRefs]) {
      if (!this.streams.has(key)) {
        this.#streamGone(key, before.get(key), "gone-after-reconnect");
      } else if (reasons.size) {
        const rx = this.subs.get(key);
        if (rx) rx.enqueue({ type: "loss" });
        this.#sendSubscribe(key);
      }
    }
    for (const share of this.shares) this.#announceShare(share);
    this.#startPreviewTimers();
  }

  /** Say Hello once per second (at most 10 times) until the relay acknowledges (spec §6.6). An old relay drops the 1-byte datagram. */
  #startHello(sess) {
    if (!this.nackEnabled) return;
    let tries = 0;
    const tick = () => {
      if (sess !== this.session || sess.closedFlag || this.nackAcked || tries >= HELLO_MAX_TRIES) return;
      tries++;
      sess.sendDatagram(HELLO_DATAGRAM);
      setTimeout(tick, HELLO_INTERVAL_MS);
    };
    tick();
  }

  #onSessionClosed(sess, info) {
    if (sess !== this.session) return;
    this.session = null;
    for (const rx of this.subs.values()) rx.queue.length = 0;
    if (this.userClosed) return;
    const reason = classifyClose(info) === "relay-shutdown" ? "relay shutting down" : info && info.error ? String(info.error.message || info.error) : (info && info.reason) || "connection lost";
    if (!this.autoReconnect) {
      this.members.clear();
      this.streams.clear();
      this.#setState("closed", { reason, closeCode: info && info.closeCode });
      emitError(this, "connection-lost", new Error(`Connection to the relay lost: ${reason}`), { fatal: true });
      return;
    }
    this.#setState("reconnecting", { reason, closeCode: info && info.closeCode });
    this.#scheduleReconnect();
  }

  /**
   * The first retry after a loss is immediate (a relay that closed our connection on purpose, e.g. a restart, answers
   * again within a second; a dead one costs one cheap failed attempt), then exponential backoff.
   */
  #scheduleReconnect() {
    if (this.userClosed) return;
    const delay = this.attempt === 0 ? FIRST_RECONNECT_MS : BACKOFF_MS[Math.min(this.attempt - 1, BACKOFF_MS.length - 1)] * (0.75 + Math.random() * 0.5);
    this.attempt++;
    clearTimeout(this.reconnectTimer);
    this.reconnectTimer = setTimeout(() => this.#attemptReconnect(), delay);
  }

  #reconnecting = null;
  /** One reconnect attempt (shared if one is already running). Resolves to the client state afterwards. */
  #attemptReconnect() {
    if (this.#reconnecting) return this.#reconnecting;
    clearTimeout(this.reconnectTimer);
    this.#reconnecting = (async () => {
      if (this.userClosed) return this.state;
      try {
        await this.#establish();
        this.attempt = 0;
        this.#setState("connected", { reconnected: true });
      } catch (e) {
        if (this.userClosed || (e instanceof ConnectError && e.code === "aborted")) return this.state; // cancelled by reconnectNow()
        // Codes: relay-unreachable (handshake failed, hash known good), link-expired (handshake failed, hash never seen to
        // work: only possible if the page never connected with it; kept for completeness), room-full / malformed
        // (JoinRejected), join-no-reply, relay-shutdown, reconnect-failed.
        let code = "reconnect-failed";
        if (e instanceof JoinRejectedError) code = e.reason === "RoomFull" ? "room-full" : "join-rejected";
        else if (e instanceof ConnectError) code = e.mayBeExpiredLink ? "link-expired" : e.reason === "unreachable" ? "relay-unreachable" : e.code || code;
        emitError(this, code, e, { fatal: false });
        this.#setState("reconnecting", { reason: e && e.message, attempt: this.attempt, error: e });
        this.#scheduleReconnect(); // a full room keeps being retried: someone may leave
      }
      return this.state;
    })().finally(() => {
      this.#reconnecting = null;
    });
    return this.#reconnecting;
  }

  /**
   * Reconnect right now, keeping everything: shares stay registered and are re-announced, watched streams are re-subscribed
   * (those whose owner is still in the room keep their key). While `reconnecting` this skips the backoff wait; while
   * `connected` it drops the connection and joins again (a fresh ConnId, so OTHER viewers of your shares see them restart).
   * Resolves with the client state after the attempt (`connected` on success, `reconnecting` if it failed and retries go on).
   */
  async reconnectNow() {
    if (this.userClosed || this.state === "idle" || this.state === "closed") throw new Error("not connected to a room; call connect() first");
    if (this.state === "connected") {
      this.#restartConnection("reconnect requested");
    } else if (this.#reconnecting) {
      // An attempt is already in flight (maybe stuck in a slow handshake): cancel it and start over right now.
      if (this.#openAbort) this.#openAbort.abort();
      await this.#reconnecting.catch(() => {});
      if (this.state === "connected") return this.state;
    }
    return this.#attemptReconnect();
  }

  /** Force a fresh connection (fresh ConnId, fresh nonce space). */
  #restartConnection(reason) {
    const sess = this.session;
    if (!sess) return;
    sess.close({ graceful: false }).then(() => {});
    this.#onSessionClosed(sess, { reason });
  }

  // -------------------------------------------------------------------------------- presence

  #applySnapshot(snap) {
    this.members.clear();
    this.streams.clear();
    for (const m of snap.members) this.#putMember(m);
    for (const s of snap.streams) this.#putStream(s);
    this.#emitRoster();
    this.#emitStreams();
    this.#reconcileFollowers();
  }

  #putMember(m) {
    const id = connIdToHex(m.connId);
    this.members.set(id, { id, name: m.displayName, streamCount: m.streamCount, self: id === this.selfId });
  }

  #putStream(s) {
    const owner = connIdToHex(s.owner);
    const key = streamKey(owner, s.streamId);
    this.streams.set(key, {
      key, owner, ownerId: s.owner, streamId: s.streamId, kind: s.kind, label: s.label, width: s.width, height: s.height, self: owner === this.selfId,
      ownerName: this.members.get(owner)?.name ?? "",
    });
  }

  /**
   * Identity of a stream that survives its owner's reconnect (a reconnect gives the owner a new ConnId, so `key` changes):
   * room + owner's display name + kind + label. Two members with the same name sharing the same label collide; the
   * caller then picks the first unwatched match. See README "Stable stream identity".
   */
  #stableKey(info) {
    const name = info.ownerName || this.members.get(info.owner)?.name || "";
    return `${this.link ? this.link.roomIdHex : ""}/${name}/${info.kind}/${info.label}`;
  }

  /** The stream currently in the room with this `stableKey` (and not `exceptKeys`), or null. */
  findStreamByStableKey(stableKey, { exceptKeys = [] } = {}) {
    for (const s of this.streams.values()) {
      if (!exceptKeys.includes(s.key) && this.#stableKey(s) === stableKey) return { ...s, stableKey };
    }
    return null;
  }

  #emitRoster() {
    this.dispatchEvent(new CustomEvent("roster", { detail: { members: [...this.members.values()] } }));
  }

  #emitStreams() {
    const members = this.members;
    const streams = [...this.streams.values()].map((s) => {
      s.ownerName = members.get(s.owner)?.name ?? s.ownerName;
      s.stableKey = this.#stableKey(s);
      return { ...s };
    });
    this.dispatchEvent(new CustomEvent("streams", { detail: { streams } }));
  }

  #onControl(msg) {
    switch (msg.type) {
      case "PresenceDelta": {
        const e = msg.event;
        if (e.type === "MemberJoined") {
          this.#putMember(e.member);
          this.#emitRoster();
        } else if (e.type === "MemberLeft") {
          const id = connIdToHex(e.connId);
          for (const [key, s] of [...this.streams]) if (s.owner === id) this.#streamGone(key, null, "member-left");
          this.members.delete(id);
          this.avatars.delete(id);
          this.#emitRoster();
          this.#emitStreams();
        } else if (e.type === "StreamStarted") {
          this.#putStream(e.stream);
          const owner = connIdToHex(e.stream.owner);
          const m = this.members.get(owner);
          if (m) m.streamCount = Math.max(m.streamCount, 1);
          this.#emitRoster();
          this.#emitStreams();
          this.#reconcileFollowers();
        } else if (e.type === "StreamStopped") {
          const owner = connIdToHex(e.connId);
          this.#streamGone(streamKey(owner, e.streamId), null, "stream-stopped");
          const m = this.members.get(owner);
          if (m) m.streamCount = [...this.streams.values()].filter((s) => s.owner === owner).length;
          this.#emitRoster();
          this.#emitStreams();
        }
        break;
      }
      case "SubscriberCount": {
        const share = this.shares.find((s) => s.videoStreamId === msg.streamId || s.audioStreamId === msg.streamId);
        if (share) {
          if (share.videoStreamId === msg.streamId) share.subscribers = msg.count;
          else share.audioSubscribers = msg.count;
          this.dispatchEvent(new CustomEvent("subscribers", { detail: { streamId: msg.streamId, count: msg.count } }));
        }
        break;
      }
      case "KeyframeRequest": {
        const share = this.shares.find((s) => s.videoStreamId === msg.streamId);
        if (share) share.sender.requestKeyframe();
        break;
      }
      case "JoinRejected":
        emitError(this, "join-rejected", new JoinRejectedError(msg.reason), { fatal: true });
        break;
      default:
        break;
    }
  }

  /** `cause`: why it is gone, for the UI's connection log: "stream-stopped" (the owner stopped it), "member-left" (the owner's session ended), "gone-after-reconnect". */
  #streamGone(key, known = null, cause = "stream-stopped") {
    const info = this.streams.get(key) || known;
    this.dispatchEvent(new CustomEvent("stream-gone", { detail: { key, cause, ownerName: info ? info.ownerName : "", label: info ? info.label : "", kind: info ? info.kind : "video" } }));
    this.streams.delete(key);
    this.previews.delete(key);
    if (this.subRefs.has(key)) {
      this.#dropSub(key);
      this.subRefs.delete(key);
    }
    const w = this.watches.get(key);
    if (w) {
      this.watches.delete(key);
      w.detach();
      this.dispatchEvent(new CustomEvent("watch-ended", {
        detail: { key, reason: "stream-stopped", cause, stableKey: info ? this.#stableKey(info) : null, ownerName: info ? info.ownerName : "", label: info ? info.label : "", kind: info ? info.kind : "video" },
      }));
    }
    this.#reconcileFollowers();
    return info;
  }

  // -------------------------------------------------------------------------------- watching

  /** Streams of `owner` that are audio (the sibling the native client auto-subscribes). */
  #siblingAudioKey(videoKey) {
    const v = this.streams.get(videoKey);
    if (!v || v.kind !== "video" || isWebcamLabel(v.label)) return null; // a webcam is no video for the audio that follows video (native `owner_has_video`)
    for (const s of this.streams.values()) if (s.owner === v.owner && s.kind === "audio") return s.key;
    return null;
  }

  /**
   * Watch a stream. `key` is `"<ownerHex>:<streamId>"` as given in the `streams` event.
   * Watching a video stream also plays the owner's audio stream when it has one ("audio follows video").
   * @param {string|{owner:string, streamId:number}} key
   * @param {{canvas?:HTMLCanvasElement, onFrame?:(f:VideoFrame)=>void, audio?:boolean}} [opts]
   * @returns {WatchHandle}
   */
  watch(key, { canvas = null, onFrame = null, audio = true } = {}) {
    if (typeof key === "object") key = streamKey(key.owner, key.streamId);
    const info = this.streams.get(key);
    if (!info) throw new Error(`unknown stream ${key}`);
    if (info.self) throw new Error("cannot watch your own stream");
    if (this.watches.has(key)) return this.watches.get(key);
    if (info.kind === "video" && !capabilities().webCodecsDecode) throw new Error("This browser has no WebCodecs VideoDecoder.");
    const handle = new WatchHandle(this, key, null);
    handle.followAudio = info.kind === "video" && audio && !isWebcamLabel(info.label);
    this.watches.set(key, handle);
    if (canvas) handle.attachToCanvas(canvas);
    if (onFrame) handle.setFrameCallback(onFrame);
    this.#addRef(key, `watch:${key}`);
    this.#reconcileFollowers();
    return handle;
  }

  unwatch(key) {
    if (typeof key === "object") key = streamKey(key.owner, key.streamId);
    const handle = this.watches.get(key);
    if (!handle) return;
    this.watches.delete(key);
    handle.detach();
    this.#removeRef(key, `watch:${key}`);
    if (handle.audioKey) this.#removeRef(handle.audioKey, `follow:${key}`);
  }

  /** Make every video watch with `audio` follow its owner's audio stream, and drop followers whose audio ended. */
  #reconcileFollowers() {
    for (const [key, handle] of this.watches) {
      if (!handle.followAudio) continue;
      const want = this.#siblingAudioKey(key);
      if (handle.audioKey && handle.audioKey !== want) {
        this.#removeRef(handle.audioKey, `follow:${key}`);
        handle.audioKey = null;
      }
      if (want && handle.audioKey !== want) {
        handle.audioKey = want;
        this.#addRef(want, `follow:${key}`);
      }
    }
  }

  #addRef(key, reason) {
    let set = this.subRefs.get(key);
    if (!set) this.subRefs.set(key, (set = new Set()));
    const first = set.size === 0;
    set.add(reason);
    if (first) this.#startSub(key);
  }

  #removeRef(key, reason) {
    const set = this.subRefs.get(key);
    if (!set) return;
    set.delete(reason);
    if (set.size === 0) {
      this.subRefs.delete(key);
      this.#dropSub(key);
      const s = this.session;
      const info = this.streams.get(key);
      if (s && info) s.send({ type: "Unsubscribe", target: info.ownerId, streamId: info.streamId }).catch(() => {});
    }
  }

  #startSub(key) {
    const info = this.streams.get(key);
    if (!info) return;
    let impl;
    if (info.kind === "video") {
      impl = new VideoReceiver({
        announcedSize: () => {
          const i = this.streams.get(key) || info;
          return i.width > 0 && i.height > 0 ? { width: i.width, height: i.height } : null;
        },
        onFrame: (frame) => {
          const h = this.watches.get(key);
          if (h) h._deliver(frame);
          else frame.close();
        },
        onEvent: (type, detail) => this.#onVideoEvent(key, type, detail),
        maxJitterBufferMs: this.maxJitterBufferMs,
        catchUpSpeedPercent: this.catchUpSpeedPercent,
        hardwareAcceleration: this.hardwareAcceleration,
        avSync: this.#avSyncFor(info.owner),
      });
    } else {
      const ctx = this.#audioContext();
      impl = new AudioReceiver({
        context: ctx,
        latencyMs: this.audioLatencyMs,
        volume: this.volumes.get(key) ?? 1,
        muted: this.muted.get(key) ?? false,
        level: this.levelLoudness,
        avSync: this.#avSyncFor(info.owner),
        onEvent: (type, detail) => this.dispatchEvent(new CustomEvent("media", { detail: { key, type, ...detail } })),
      });
    }
    const rx = new RxStream(this, key, info, impl);
    this.subs.set(key, rx);
    this.#sendSubscribe(key);
  }

  /** The owner's shared A/V sync state; one per owner, so their video and audio receivers find each other. */
  #avSyncFor(owner) {
    let s = this.avSyncs.get(owner);
    if (!s) this.avSyncs.set(owner, (s = new AvSync()));
    return s;
  }

  #dropSub(key) {
    const rx = this.subs.get(key);
    if (!rx) return;
    this.subs.delete(key);
    rx.impl.close();
    this.reassembler.clearStream(rx.info.ownerId, rx.info.streamId);
  }

  #sendSubscribe(key) {
    const info = this.streams.get(key);
    const s = this.session;
    if (!info || !s) return;
    if (!this.subs.has(key)) return;
    s.send({ type: "Subscribe", target: info.ownerId, streamId: info.streamId }).catch(() => {});
  }

  #onVideoEvent(key, type, detail) {
    this.dispatchEvent(new CustomEvent("media", { detail: { key, type, ...detail } }));
    if (type === "need-keyframe" && this.lossRecovery === "resubscribe") {
      const rx = this.subs.get(key);
      const info = this.streams.get(key);
      const s = this.session;
      if (!rx || !info || !s) return;
      if (detail.reason !== "waiting" && detail.gopMs !== null && detail.gopMs <= KEYFRAME_SOON_MS) return;
      const now = performance.now();
      if (now - rx.lastResubscribe < RESUBSCRIBE_MIN_INTERVAL_MS) return;
      rx.lastResubscribe = now;
      // Unsubscribe+Subscribe is the only way a viewer can make the relay ask the sender for a keyframe (spec §9).
      s.send({ type: "Unsubscribe", target: info.ownerId, streamId: info.streamId }).catch(() => {});
      s.send({ type: "Subscribe", target: info.ownerId, streamId: info.streamId }).catch(() => {});
    }
  }

  #onFrameDropped(connId, streamId) {
    const rx = this.subs.get(streamKey(connIdToHex(connId), streamId));
    if (rx) rx.enqueue({ type: "loss" });
  }

  #audioContext() {
    if (!this._audioContext) {
      const AC = globalThis.AudioContext || globalThis.webkitAudioContext;
      this._audioContext = new AC({ latencyHint: "interactive", sampleRate: 48000 });
    }
    if (this._audioContext.state === "suspended") this._audioContext.resume().catch(() => {});
    return this._audioContext;
  }

  /** Call from a user gesture if the browser's autoplay policy kept audio suspended. */
  resumeAudio() {
    return this._audioContext ? this._audioContext.resume() : Promise.resolve();
  }

  #audioTarget(key) {
    const info = this.streams.get(key);
    if (info && info.kind === "video") return this.#siblingAudioKey(key);
    return key;
  }

  /** Playback volume, 0..5 (0-500 %). `key` may be a video stream (applies to its audio) or an audio stream. */
  setVolume(key, volume) {
    const a = this.#audioTarget(key);
    if (!a) return;
    const v = Math.max(0, Math.min(MAX_VOLUME, Number(volume) || 0));
    this.volumes.set(a, v);
    const rx = this.subs.get(a);
    if (rx) rx.impl.setVolume(v);
  }

  setMuted(key, muted) {
    const a = this.#audioTarget(key);
    if (!a) return;
    this.muted.set(a, !!muted);
    const rx = this.subs.get(a);
    if (rx) rx.impl.setMuted(!!muted);
  }

  /** Live settings (the native app's Settings > Jitter buffer). */
  /** "Level loudness" (#119): slow AGC + instant peak limiter on every watched audio stream, live. On by default. */
  setLevelLoudness(on) {
    this.levelLoudness = !!on;
    for (const rx of this.subs.values()) if (rx.impl.setLevelLoudness) rx.impl.setLevelLoudness(this.levelLoudness);
  }

  setJitterSettings({ maxJitterBufferMs, catchUpSpeedPercent } = {}) {
    if (maxJitterBufferMs != null) this.maxJitterBufferMs = maxJitterBufferMs;
    if (catchUpSpeedPercent != null) this.catchUpSpeedPercent = catchUpSpeedPercent;
    for (const rx of this.subs.values()) {
      if (rx.info.kind !== "video") continue;
      if (maxJitterBufferMs != null) rx.impl.setMaxJitterBufferMs(maxJitterBufferMs);
      if (catchUpSpeedPercent != null) rx.impl.setCatchUpSpeedPercent(catchUpSpeedPercent);
    }
  }

  watchStats(key) {
    const rx = this.subs.get(key);
    if (!rx) return null;
    if (rx.info.kind === "video") {
      return { kind: "video", ...rx.impl.getStats(), decryptFailures: rx.decryptFailures, stalled: rx.stalled, droppedFrames: this.reassembler.droppedFrames, nack: this.nackStats() };
    }
    return { kind: "audio", ...rx.impl.stats, level: rx.impl.level, volume: rx.impl.volume, muted: rx.impl.muted, decryptFailures: rx.decryptFailures };
  }

  /** Selective retransmission counters (spec §6.6): viewer side (requests, repairs) and sender side (re-sends). */
  nackStats() {
    return {
      enabled: this.nackEnabled, active: this.nackAcked, rttMs: this.#rttMs(), rttReported: Number.isFinite(this.lastTransportStats?.smoothedRtt), ...this.reassembler.nackStats,
      served: { requests: this.sendRing.requests, resent: this.sendRing.resent, missed: this.sendRing.missed },
    };
  }

  // -------------------------------------------------------------------------------- receive path

  /** Ask the relay once a second (at most 10 times) to report who watches our streams; an older relay drops it and never answers. */
  #startViewersHello(sess) {
    this.viewersSupported = false;
    let n = 0;
    const tick = () => {
      if (sess !== this.session || this.viewersSupported || n++ >= 10) return;
      sess.sendDatagram(new Uint8Array([VIEWERS_TAG_HELLO]));
      setTimeout(tick, 1000);
    };
    tick();
  }

  #onViewersDatagram(bytes) {
    if (bytes.length === 1 && bytes[0] === VIEWERS_TAG_ACK) {
      this.viewersSupported = true;
      this.dispatchEvent(new CustomEvent("viewers", { detail: {} }));
      return;
    }
    if (bytes[0] !== VIEWERS_TAG_LIST || bytes.length < 3 || bytes.length !== 3 + 8 * bytes[2]) return;
    const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    const ids = [];
    for (let i = 0; i < bytes[2]; i++) ids.push(connIdToHex(dv.getBigUint64(3 + 8 * i, false)));
    const share = this.shares.find((s) => s.videoStreamId === bytes[1]);
    if (!share) return;
    this.viewersSupported = true;
    share.viewerIds = ids;
    clearTimeout(share._viewersTimer);
    if (ids.length) {
      share._viewersTimer = setTimeout(() => {
        share.viewerIds = [];
        this.dispatchEvent(new CustomEvent("viewers", { detail: {} }));
      }, VIEWERS_STALE_MS);
    }
    this.dispatchEvent(new CustomEvent("viewers", { detail: { streamId: bytes[1], ids } }));
  }

  // -------------------------------------------------------------------------------- emoji avatars (spec 9.2)

  /** Hello once a second (at most 10 times) until the relay acks; an older relay never answers and nobody else's emoji shows. */
  #startAvatars(sess) {
    this.avatarsSupported = false;
    this.avatars = new Map();
    clearInterval(this._avatarTimer);
    this._avatarTimer = null;
    let n = 0;
    const tick = () => {
      if (sess !== this.session || sess.closedFlag || this.avatarsSupported || n++ >= 10) return;
      sess.sendDatagram(new Uint8Array([AVATAR_TAG_HELLO]));
      setTimeout(tick, AVATAR_HELLO_MS);
    };
    tick();
  }

  #onAvatarDatagram(bytes) {
    if (bytes.length === 1) {
      this.avatarsSupported = true; // a list follows at once
      this.dispatchEvent(new CustomEvent("avatars", { detail: {} }));
      this.#syncAvatar();
      return;
    }
    const list = unpackAvatars(bytes);
    if (!list) return; // truncated / trailing bytes / a bad entry: the whole list is distrusted
    this.avatarsSupported = true;
    const next = new Map(list.map(([id, emoji]) => [connIdToHex(id), emoji]));
    // the relay repeats the list every 3 s: tell the UI only when something changed (a re-render per repeat would be churn)
    const changed = next.size !== this.avatars.size || [...next].some(([id, e]) => this.avatars.get(id) !== e);
    this.avatars = next;
    if (changed) this.dispatchEvent(new CustomEvent("avatars", { detail: {} }));
    this.#syncAvatar();
  }

  /** Keep sending AvatarSet every 250 ms until the relay's list shows our entry as we want it (that entry is the Set's ack). */
  #syncAvatar() {
    const sess = this.session;
    const done = () => !sess || sess !== this.session || sess.closedFlag || !this.avatarsSupported || (this.avatars.get(this.selfId) || "") === this._avatar;
    if (done()) {
      clearInterval(this._avatarTimer);
      this._avatarTimer = null;
      return;
    }
    if (this._avatarTimer) return;
    const send = () => {
      if (done()) return this.#syncAvatar();
      const d = packAvatarSet(this._avatar);
      if (d) sess.sendDatagram(d);
    };
    send();
    this._avatarTimer = setInterval(send, AVATAR_SET_MS);
  }

  /** Our emoji avatar ("" = none; at most 16 UTF-8 bytes, anything longer is ignored). Sent to the relay once it acked, and again after every reconnect. */
  setAvatar(emoji) {
    const e = String(emoji || "").trim();
    if (!packAvatarSet(e)) return;
    this._avatar = e;
    this.#syncAvatar();
  }

  /** The emoji shown for member `idHex` ("" = none): ours from our own setting (at once, also on an older relay), the others' from the relay's list. */
  emojiOf(idHex) {
    return idHex === this.selfId ? this._avatar : this.avatars.get(idHex) || "";
  }

  _viewerNames(share) {
    return share.viewerIds
      .map((id) => this.members.get(id)?.name)
      .filter((n) => n != null)
      .sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()));
  }

  #onDatagram(bytes) {
    // Viewer lists (spec 9.1) first: ViewersAck is 1 byte and ViewersList can be >= 19 bytes, so neither is told apart from
    // NACK-family or media datagrams by length. Then NACK-family (< 19 bytes, tag >= 0xE0), then media, untouched.
    if ((bytes[0] === VIEWERS_TAG_ACK && bytes.length === 1) || bytes[0] === VIEWERS_TAG_LIST) {
      this.#onViewersDatagram(bytes);
      return;
    }
    // Avatar lists (spec 9.2) the same way: AvatarAck is 1 byte, AvatarList can be >= 19 bytes.
    if ((bytes[0] === AVATAR_TAG_ACK && bytes.length === 1) || bytes[0] === AVATAR_TAG_LIST) {
      this.#onAvatarDatagram(bytes);
      return;
    }
    if (isNackDatagram(bytes)) {
      this.#onNackDatagram(bytes);
      return;
    }
    let parsed;
    try {
      parsed = unpackMediaHeader(bytes);
    } catch {
      return;
    }
    const { header, payload } = parsed;
    if (header.version !== WIRE_VERSION) return;
    const key = streamKey(connIdToHex(header.connId), header.streamId);
    const rx = this.subs.get(key);
    if (!rx) return;
    const done = this.reassembler.ingest(header, payload);
    if (done.length) this.#deliver(done);
    if (this.reassembler.nack) this.#scheduleNackPoll();
  }

  /** Hand completed frames, in order, to their receivers. */
  #deliver(frames) {
    for (const f of frames) {
      const rx = this.subs.get(streamKey(connIdToHex(f.connId), f.streamId));
      if (rx) rx.enqueue({ type: "frame", frameId: f.frameId, ciphertext: f.ciphertext, keyframe: f.keyframe, stalled: f.stalled });
    }
  }

  /** One NACK-family datagram (spec §6.6): the relay's HelloAck switches the reassembler to NACK mode, a Miss ends a repair, a Request asks us to re-send. */
  #onNackDatagram(bytes) {
    if (!this.nackEnabled) return;
    const sess = this.session;
    switch (bytes[0]) {
      case NACK_TAG_HELLO_ACK:
        this.nackAcked = true;
        this.reassembler.setNack(true);
        break;
      case NACK_TAG_MISS: {
        const r = unpackNackRange(bytes);
        if (r) {
          this.reassembler.nackMiss(r);
          this.#scheduleNackPoll(0);
        }
        break;
      }
      case NACK_TAG_REQUEST: {
        const r = unpackNackRange(bytes);
        // only requests for our own streams; the relay forwards them only after our Hello was acknowledged
        if (!r || !sess || !sess.joined || r.connId !== sess.connId) break;
        if (sess.pendingDatagrams > MAX_PENDING_DATAGRAMS) break; // backed up: a re-send would arrive too late to matter
        for (const d of this.sendRing.resends(r)) {
          sess.sendDatagram(d);
          this.sendRing.resent++;
        }
        break;
      }
      default:
        break;
    }
  }

  #scheduleNackPoll(delay = NACK_TICK_MS) {
    if (this.nackTimer != null) return;
    this.nackTimer = setTimeout(() => {
      this.nackTimer = null;
      this.#nackPoll();
    }, delay);
  }

  /** The viewer's connection RTT: WebTransport `getStats().smoothedRtt` where the browser has it, else a fixed allowance. */
  #rttMs() {
    const r = this.lastTransportStats?.smoothedRtt;
    return Number.isFinite(r) && r > 0 ? r : NACK_DEFAULT_RTT_MS;
  }

  #nackPoll() {
    const sess = this.session;
    if (!sess || !this.reassembler.nack) return;
    const { frames, requests } = this.reassembler.poll(this.#rttMs());
    if (frames.length) this.#deliver(frames);
    for (const r of requests) sess.sendDatagram(packNackRange(NACK_TAG_REQUEST, r));
    if (this.reassembler.hasPending()) this.#scheduleNackPoll();
  }

  async #onPreviewStream(bytes) {
    try {
      const { header, payload } = unpackPreviewHeader(bytes);
      if (header.streamType !== STREAM_TYPE_PREVIEW || header.version !== WIRE_VERSION) return;
      const owner = connIdToHex(header.connId);
      if (owner === this.selfId) return;
      const jpeg = await decryptPreview(this._key, header.connId, header.counter, header.streamId, payload);
      const key = streamKey(owner, header.streamId);
      const blob = new Blob([jpeg], { type: "image/jpeg" });
      this.previews.set(key, { blob, receivedAt: Date.now() });
      this.dispatchEvent(new CustomEvent("preview", { detail: { key, owner, streamId: header.streamId, blob } }));
    } catch (e) {
      // wrong key / corrupt: ignore like the native client does
    }
  }

  // -------------------------------------------------------------------------------- send path

  #txState(streamId) {
    let st = this.tx.get(streamId);
    if (!st) this.tx.set(streamId, (st = { chain: Promise.resolve(), queued: 0, dropped: 0, sentFrames: 0, sentBytes: 0 }));
    return st;
  }

  /**
   * Encrypt (one counter value per frame), fragment to <= 1000 bytes and send as datagrams, strictly in order per stream.
   * Returns false when the frame was dropped (not connected / backlog), so the caller can ask for a keyframe.
   */
  #sendMedia(streamId, { ptsUs, keyframe, payload, stalled = false }, fps = 60, bitrateKbps = 8000, video = true) {
    const sess = this.session;
    if (!sess || !sess.joined) return false;
    const st = this.#txState(streamId);
    if (st.queued >= MAX_TX_QUEUE || sess.pendingDatagrams > MAX_PENDING_DATAGRAMS) {
      st.dropped++;
      return false;
    }
    st.queued++;
    st.chain = st.chain.then(async () => {
      try {
        if (sess !== this.session) return;
        const { nonce, counter } = sess.nonce.take();
        const ct = await encryptFrame(this._key, streamId, nonce, ptsUs, payload);
        const datagrams = fragmentFrame({
          connId: sess.connId, streamId, frameId: counter, ciphertext: ct, maxDatagramSize: sess.maxDatagramSize,
          flags: (keyframe ? MEDIA_FLAG_KEYFRAME : 0) | (stalled ? MEDIA_FLAG_STALLED : 0),
        });
        // Video only (audio is never NACKed): keep the datagrams so a relay-forwarded Request can be answered (spec §6.6).
        if (video && this.nackEnabled) this.sendRing.record(streamId, counter, datagrams);
        const t0 = performance.now();
        const plan = pacePlan(datagrams.length, ct.length, keyframe, fps, bitrateKbps);
        if (!plan) {
          for (const d of datagrams) sess.sendDatagram(d);
        } else {
          // Spread the frame over time (see pacePlan). Chunks are due by the clock, so a throttled timer (background tab)
          // sends what is due in one go instead of stretching the frame. A frame that has started is sent whole: the
          // pending-datagram limit is only checked before a frame, and here by waiting, never by skipping a fragment.
          // Stop/reconnect end the loop at the next tick (tx entry gone / session replaced / closed).
          const alive = () => sess === this.session && this.tx.get(streamId) === st && !sess.closedFlag;
          st.pacing = true;
          try {
            for (let i = 0; i < datagrams.length && alive(); ) {
              while (sess.pendingDatagrams > MAX_PENDING_DATAGRAMS && alive()) await sleep(plan.gapMs);
              const due = Math.floor((performance.now() - t0) / plan.gapMs) + 1;
              for (const end = Math.min(datagrams.length, due * plan.perChunk); i < end; i++) sess.sendDatagram(datagrams[i]);
              if (i < datagrams.length) await sleep(plan.gapMs);
            }
          } finally {
            st.pacing = false;
          }
        }
        st.sentFrames++;
        st.sentBytes += ct.length;
        if (keyframe && datagrams.length >= 24) {
          // diagnostics (like native's "sending a keyframe of N datagrams ... paced over ~Y ms"): the last 20 big keyframes
          const log = (st.bigKeyframes ??= []);
          if (log.push({ datagrams: datagrams.length, bytes: ct.length, spreadMs: Math.round(performance.now() - t0) }) > 20) log.shift();
        }
      } catch (e) {
        if (e instanceof NonceSpaceExhausted) {
          emitError(this, "nonce-exhausted", e);
          this.#restartConnection("nonce space exhausted");
        } else {
          emitError(this, "send-failed", e);
        }
      } finally {
        st.queued--;
      }
    });
    return true;
  }

  async #sendPreview(streamId, jpeg) {
    const sess = this.session;
    if (!sess || !sess.joined) return;
    const { nonce, counter } = sess.nonce.take();
    const ct = await encryptPreview(this._key, streamId, nonce, jpeg);
    const head = packPreviewHeader({ connId: sess.connId, streamId, counter });
    const out = new Uint8Array(head.length + ct.length);
    out.set(head, 0);
    out.set(ct, head.length);
    await sess.sendUniStream(out);
  }

  // -------------------------------------------------------------------------------- sharing

  #allocStreamId() {
    const used = new Set(this.shares.flatMap((s) => [s.videoStreamId, s.audioStreamId]));
    for (let i = 0; i < 256; i++) if (!used.has(i)) return i;
    throw new Error("no free stream id");
  }

  #announceShare(share) {
    const s = this.session;
    if (!s) return;
    share.subscribers = 0;
    share.audioSubscribers = 0;
    share.viewerIds = [];
    if (share.videoStreamId != null) {
      s.send({ type: "AnnounceStream", streamId: share.videoStreamId, kind: "video", label: clampUtf8(share.label), width: share.width, height: share.height }).catch(() => {});
    }
    if (share.audioStreamId != null) {
      s.send({ type: "AnnounceStream", streamId: share.audioStreamId, kind: "audio", label: clampUtf8(share.audioLabel), width: 0, height: 0 }).catch(() => {});
    }
    if (share.sender) share.sender.requestKeyframe();
  }

  /**
   * Share a screen/window/tab (and optionally its audio) into the room.
   * @param {object} [o]
   * @param {boolean} [o.audio=false]         also capture and share audio (where the browser offers it)
   * @param {boolean} [o.audioOnly=false]     share ONLY the stream's audio track as an audio-only share (no video stream is announced; a
   *                                          video track in `o.stream` is ignored, the caller stops it). Throws when there is no audio track.
   * @param {"auto"|"h264"|"h265"|"av1"} [o.codec="auto"]  "auto" = H.264 (hardware-decodable everywhere)
   * @param {number} [o.fps=60]
   * @param {number} [o.bitrateKbps=8000]
   * @param {"text"|"quality"|"gaming"} [o.preset="quality"] ("balanced" and "low_latency" are aliases of quality)
   * @param {string} [o.label="Screen"]               the stream label ("Webcam" makes it a webcam, by the label rule of `isWebcamLabel`)
   * @param {number} [o.gopMs]                        keyframe interval (default: the preset's)
   * @param {"quality"|"realtime"} [o.latencyMode]    encoder mode (default: the preset's)
   * @param {MediaStream} [o.stream]          use this stream instead of calling getDisplayMedia (tests, custom pickers)
   * @param {object} [o.displayMediaOptions]  merged into the getDisplayMedia() argument
   * @param {boolean} [o.previews=true]
   * @returns {Promise<ShareHandle>}
   */
  async startShare(o = {}) {
    if (this.state !== "connected") throw new Error("join a room before sharing");
    const caps = capabilities();
    const audioOnly = !!o.audioOnly;
    if (!caps.canShare && !o.stream) throw new Error("Sharing needs WebCodecs, MediaStreamTrackProcessor and getDisplayMedia (a current Chromium-based browser).");
    const fps = o.fps ?? 60;
    const bitrateKbps = o.bitrateKbps ?? 8000;
    const preset = ENCODER_PRESETS[o.preset ?? "quality"] ?? ENCODER_PRESETS.quality;
    let stream = o.stream;
    let ownsStream = false;
    if (!stream) {
      stream = await navigator.mediaDevices.getDisplayMedia({
        // an audio-only share still has to ask for video (browsers refuse audio alone); the video track is stopped below
        video: audioOnly ? true : { frameRate: { ideal: fps, max: fps }, ...(o.width ? { width: { ideal: o.width } } : {}), ...(o.height ? { height: { ideal: o.height } } : {}) },
        audio: o.audio || audioOnly ? { echoCancellation: false, noiseSuppression: false, autoGainControl: false } : false,
        ...(o.displayMediaOptions || {}),
      });
      ownsStream = true;
    }
    if (audioOnly && ownsStream) for (const v of stream.getVideoTracks()) v.stop();
    const videoTrack = audioOnly ? null : stream.getVideoTracks()[0];
    if (!audioOnly && !videoTrack) throw new Error("the selected source has no video track");
    const audioTrack = audioOnly || o.audio !== false ? stream.getAudioTracks()[0] || null : null;
    if (audioOnly && !audioTrack) {
      if (ownsStream) stream.getTracks().forEach((t) => t.stop());
      throw new Error("the selected source has no audio track");
    }
    if (o.audio && !audioTrack && ownsStream) {
      this.dispatchEvent(new CustomEvent("share", { detail: { state: "no-audio", message: "The browser did not offer audio for this source (choose a tab, or enable 'Share system audio' where supported)." } }));
    }

    const codec = !o.codec || o.codec === "auto" ? "h264" : o.codec;
    const clock = new MediaClock();
    const videoStreamId = audioOnly ? null : this.#allocStreamId();
    const settings = videoTrack ? videoTrack.getSettings() : {};
    const share = new ShareHandle(this, {
      stream, ownsStream, videoTrack, audioTrack, codec, fps, bitrateKbps, label: o.label ?? (audioOnly ? "Audio" : "Screen"), audioLabel: audioOnly ? (o.label ?? "Audio") : `${o.label ?? "Screen"} audio`,
      audioOnly, videoStreamId, audioStreamId: null, width: settings.width || 0, height: settings.height || 0, audioMuted: false, previewEnabled: !audioOnly && o.previews !== false,
      sampler: new PreviewSampler(), previewDue: false, previewTimer: null,
    });
    this.shares.push(share); // reserve the id before allocating the audio one
    if (audioTrack) share.audioStreamId = this.#allocStreamId();

    if (!audioOnly) share.sender = new VideoSender({
      track: videoTrack, codec, fps, bitrateKbps, clock,
      gopMs: o.gopMs ?? preset.gopMs, latencyMode: o.latencyMode ?? preset.latencyMode,
      hardwareAcceleration: o.hardwareAcceleration ?? this.hardwareAcceleration,
      isActive: () => share.subscribers > 0,
      // a paced frame is on its way: drop INPUT frames meanwhile (nothing encoded, no reference chain broken) instead of
      // encoding frames that would overflow the tx queue and force yet another keyframe
      isBackedUp: () => !!this.tx.get(share.videoStreamId)?.pacing,
      onCapturedFrame: (frame) => {
        if (share.previewDue && share.previewEnabled) {
          share.previewDue = false;
          if (share.sampler.capture(frame)) {
            share.sampler.toJpeg().then((jpeg) => this.#sendPreview(share.videoStreamId, jpeg)).catch((e) => emitError(this, "preview-failed", e));
          }
        }
      },
      onUnit: (u) => {
        if (!this.#sendMedia(share.videoStreamId, u, share.fps, share.bitrateKbps)) share.sender.forceKeyframe();
      },
      onError: (e) => {
        emitError(this, "encoder-error", e);
        this.stopShare(share);
      },
    });
    try {
      if (share.sender) await share.sender.start();
    } catch (e) {
      this.shares.splice(this.shares.indexOf(share), 1);
      if (ownsStream) stream.getTracks().forEach((t) => t.stop());
      throw e;
    }
    share.codecString = share.sender ? share.sender.stats.codecString : null;

    if (audioTrack) {
      share.audioSender = new AudioSender({
        track: audioTrack, clock,
        onPacket: (p) => {
          if (share.audioSubscribers > 0) this.#sendMedia(share.audioStreamId, { ...p, keyframe: true }, undefined, undefined, false);
        },
        onError: (e) => emitError(this, "audio-encoder-error", e),
      });
      try {
        await share.audioSender.start();
      } catch (e) {
        if (audioOnly) {
          // an audio-only share with no working encoder has nothing left to send
          this.shares.splice(this.shares.indexOf(share), 1);
          if (ownsStream) stream.getTracks().forEach((t) => t.stop());
          throw e;
        }
        emitError(this, "audio-share-unavailable", e);
        share.audioSender = null;
        share.audioStreamId = null;
        share.audioTrack = null;
      }
    }

    (videoTrack || audioTrack).addEventListener("ended", () => {
      this.dispatchEvent(new CustomEvent("share", { detail: { state: "ended-by-browser", share } }));
      this.stopShare(share);
    });

    this.#announceShare(share);
    this.#startPreviewTimer(share);
    // A damage-driven capture of a completely static source can stay silent; tell the UI instead of looking dead.
    if (share.sender) share.noFrameTimer = setTimeout(() => {
      if (!share.ended && share.sender.stats.captured === 0) {
        this.dispatchEvent(new CustomEvent("share", { detail: { state: "no-frames", share, message: "The capture source has not delivered a single frame yet (a static screen, or a capture problem)." } }));
      }
    }, 4000);
    this.dispatchEvent(new CustomEvent("share", { detail: { state: "started", share } }));
    return share;
  }

  #startPreviewTimer(share) {
    clearTimeout(share.previewTimer);
    clearInterval(share.previewTimer);
    if (!share.previewEnabled) return;
    share.previewTimer = setTimeout(() => {
      share.previewDue = true;
      share.previewTimer = setInterval(() => (share.previewDue = true), PREVIEW_INTERVAL_MS);
    }, PREVIEW_FIRST_DELAY_MS);
  }

  #startPreviewTimers() {
    for (const share of this.shares) {
      this.#startPreviewTimer(share);
    }
  }

  stopShare(share) {
    if (!share) {
      for (const s of [...this.shares]) this.stopShare(s);
      return;
    }
    if (share.ended) return;
    share.ended = true;
    const i = this.shares.indexOf(share);
    if (i >= 0) this.shares.splice(i, 1);
    clearTimeout(share.previewTimer);
    clearInterval(share.previewTimer);
    clearTimeout(share.noFrameTimer);
    share.sender && share.sender.stop();
    share.audioSender && share.audioSender.stop();
    if (share.ownsStream) share.stream.getTracks().forEach((t) => t.stop());
    const s = this.session;
    if (s) {
      if (share.videoStreamId != null) s.send({ type: "StopStream", streamId: share.videoStreamId }).catch(() => {});
      if (share.audioStreamId != null) s.send({ type: "StopStream", streamId: share.audioStreamId }).catch(() => {});
    }
    this.tx.delete(share.videoStreamId);
    this.sendRing.forget(share.videoStreamId);
    if (share.audioStreamId != null) this.tx.delete(share.audioStreamId);
    this.dispatchEvent(new CustomEvent("share", { detail: { state: "stopped", share } }));
  }

  _shareStats(share) {
    const v = share.sender ? share.sender.getStats() : {};
    const tx = this.tx.get(share.videoStreamId);
    return {
      ...v,
      subscribers: share.subscribers,
      active: share.subscribers > 0,
      codec: share.codec,
      sentFrames: tx ? tx.sentFrames : 0,
      sentBytes: tx ? tx.sentBytes : 0,
      txDropped: tx ? tx.dropped : 0,
      bigKeyframes: tx?.bigKeyframes ?? [],
      audioPackets: share.audioSender ? share.audioSender.packets : 0,
      audioSubscribers: share.audioSubscribers,
    };
  }

  // -------------------------------------------------------------------------------- stats

  #startStats() {
    this.#stopStats();
    this.statsTimer = setInterval(async () => {
      if (this.state !== "connected" || !this.session) return;
      const watching = {};
      for (const key of this.subs.keys()) watching[key] = this.watchStats(key);
      const sharing = this.shares.map((s) => ({ videoStreamId: s.videoStreamId, audioStreamId: s.audioStreamId, ...this._shareStats(s) }));
      const t = await this.session?.getStats();
      this.lastTransportStats = t;
      this.dispatchEvent(new CustomEvent("stats", { detail: { watching, sharing, transport: t, droppedFrames: this.reassembler.droppedFrames } }));
    }, STATS_INTERVAL_MS);
  }

  #stopStats() {
    clearInterval(this.statsTimer);
    this.statsTimer = null;
  }
}

export { ConnectError, JoinLinkError, JoinRejectedError };
