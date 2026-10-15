// One WebTransport connection to the relay (spec §11): connect, control stream, datagrams, preview
// uni-streams. No room logic here — RoomClient owns reconnects, crypto and subscriptions.

import {
  ControlFrameReader, MAX_MEDIA_DATAGRAM_LEN, WEBTRANSPORT_PATH, decodeRelayToClient, encodeClientToRelay, frameControl, randomConnId,
} from "./protocol.js";
import { NonceSequence } from "./crypto.js";
import { parseServerAddr } from "./joinlink.js";

/**
 * Connect failure. `code`: `handshake-failed`, `no-webtransport`, `datagram-too-small`, `join-no-reply`, `relay-shutdown`.
 *
 * For `handshake-failed`, `reason` says what the browser lets us conclude. Chromium reports EVERY handshake failure as the
 * same `WebTransportError: Opening handshake failed.` (source "session", no code), whether the relay is down, the host does
 * not resolve, or the pinned certificate hash does not match (measured, see README), so the core infers from history:
 *  - `"unreachable"`: this server+hash pair completed a handshake earlier in this page, so the hash is known good and
 *    the relay is simply not answering (down, restarting, network). `mayBeExpiredLink` is false.
 *  - `"unreachable-or-expired-link"`: the link carries a hash that has never been seen to work here; the relay may be down
 *    or the hash may be stale. `mayBeExpiredLink` is true (only then).
 *  - `"no-hash"`: the link has no hash (CA-certificate relay or native-only link).
 */
export class ConnectError extends Error {
  constructor(message, { code, cause, mayBeExpiredLink = false, reason = null } = {}) {
    super(message);
    this.name = "ConnectError";
    this.code = code;
    this.reason = reason;
    this.cause = cause;
    this.mayBeExpiredLink = mayBeExpiredLink;
  }
}

/**
 * Application close codes the relay uses (server/src/relay.rs). Chromium 153 does NOT expose them: a QUIC
 * CONNECTION_CLOSE with an application code surfaces as `WebTransportError: Connection lost.` with no close code, so
 * these are mapped only if a browser ever reports them. The JoinRejected frame on the control stream is the reliable signal.
 */
export const CLOSE_CODES = { MALFORMED: 0x101, ROOM_FULL: 0x102, SHUTDOWN: 0x200 };

/** What a WebTransport close info says, as far as the browser tells us (null = nothing useful). */
export function classifyClose(info) {
  const code = info && typeof info.closeCode === "number" ? info.closeCode : null;
  if (code === CLOSE_CODES.ROOM_FULL) return "room-full";
  if (code === CLOSE_CODES.MALFORMED) return "malformed";
  if (code === CLOSE_CODES.SHUTDOWN) return "relay-shutdown";
  return null;
}

/** server+hash pairs that completed a WebTransport handshake in this page: their hash is known good. */
const verifiedHashes = new Set();
const hashKey = (server, certHash) => `${server}|${Array.from(certHash, (b) => b.toString(16).padStart(2, "0")).join("")}`;
export const isHashVerified = (server, certHash) => !!certHash && verifiedHashes.has(hashKey(server, certHash));

export class JoinRejectedError extends Error {
  constructor(reason) {
    super(
      reason === "RoomFull"
        ? "The room is full (8 members maximum)."
        : reason === "Malformed"
          ? "The relay rejected the join request as malformed."
          : `The relay rejected the join: ${reason}`,
    );
    this.name = "JoinRejectedError";
    this.code = reason;
    this.reason = reason;
  }
}

export const EXPIRED_LINK_HINT =
  "Could not connect to the relay. It may be offline, or this invite link may have expired: the relay rotates its certificate about every 11 days, which invalidates the certificate hash inside older links. Ask for a fresh invite link.";

export function supportsWebTransport() {
  return typeof WebTransport !== "undefined";
}

const HANDSHAKE_TIMEOUT_MS = 8000;
const MAX_PREVIEW_STREAM_BYTES = 4 * 1024 * 1024;

async function readAll(readable, limit) {
  const reader = readable.getReader();
  const chunks = [];
  let total = 0;
  for (;;) {
    const { value, done } = await reader.read();
    if (done) break;
    total += value.length;
    if (total > limit) {
      reader.cancel().catch(() => {});
      throw new Error("uni stream exceeds size limit");
    }
    chunks.push(value);
  }
  if (chunks.length === 1) return chunks[0];
  const out = new Uint8Array(total);
  let o = 0;
  for (const c of chunks) {
    out.set(c, o);
    o += c.length;
  }
  return out;
}

export class RelaySession {
  /**
   * Open the WebTransport session. Rejects with ConnectError.
   * @param {{server:string, certHash:Uint8Array|null, signal?:AbortSignal, timeoutMs?:number}} o   server = host:port.
   *   `signal` aborts a handshake in flight (ConnectError code "aborted"); a handshake still pending after `timeoutMs`
   *   is given up on (Chromium would otherwise keep retransmitting its Initial packet for many seconds).
   */
  static async open({ server, certHash, signal = null, timeoutMs = HANDSHAKE_TIMEOUT_MS }) {
    if (!supportsWebTransport()) {
      throw new ConnectError("This browser does not support WebTransport (needed to reach the relay). Use a current Chrome, Edge or Firefox.", { code: "no-webtransport" });
    }
    try {
      parseServerAddr(server);
    } catch (e) {
      throw new ConnectError(e.message, { code: e.code === "no-port" ? "server-no-port" : "server-bad-port", cause: e });
    }
    const url = `https://${server}${WEBTRANSPORT_PATH}`;
    const opts = {};
    if (certHash) opts.serverCertificateHashes = [{ algorithm: "sha-256", value: certHash }];
    let wt;
    let aborted = false;
    let timer = null;
    const onAbort = () => {
      aborted = true;
      try {
        wt && wt.close();
      } catch {}
    };
    const cleanup = () => {
      clearTimeout(timer);
      if (signal) signal.removeEventListener("abort", onAbort);
    };
    try {
      if (signal && signal.aborted) throw new Error("aborted");
      wt = new WebTransport(url, opts);
      wt.closed.catch(() => {});
      if (signal) signal.addEventListener("abort", onAbort, { once: true });
      if (timeoutMs) timer = setTimeout(() => wt.close(), timeoutMs);
      await wt.ready;
    } catch (e) {
      cleanup();
      if (aborted || (signal && signal.aborted)) throw new ConnectError("The connection attempt was cancelled.", { code: "aborted", cause: e });
      try {
        wt && wt.close();
      } catch {}
      if (certHash && isHashVerified(server, certHash)) {
        throw new ConnectError(`The relay at ${server} is not answering (it may be down or restarting).`, { code: "handshake-failed", reason: "unreachable", cause: e });
      }
      throw new ConnectError(certHash ? EXPIRED_LINK_HINT : `Could not connect to the relay at ${server}. If it uses a self-signed certificate, the invite link must contain its certificate hash (4th field).`, {
        code: "handshake-failed",
        reason: certHash ? "unreachable-or-expired-link" : "no-hash",
        cause: e,
        mayBeExpiredLink: !!certHash,
      });
    }
    cleanup();
    if (certHash) verifiedHashes.add(hashKey(server, certHash));
    const max = wt.datagrams.maxDatagramSize;
    if (max < MAX_MEDIA_DATAGRAM_LEN) {
      wt.close();
      throw new ConnectError(`This connection only allows ${max}-byte datagrams; ${MAX_MEDIA_DATAGRAM_LEN} are required for media.`, { code: "datagram-too-small" });
    }
    return new RelaySession(wt);
  }

  constructor(wt) {
    this.wt = wt;
    this.maxDatagramSize = wt.datagrams.maxDatagramSize;
    this.connId = randomConnId();
    this.nonce = null; // NonceSequence, set after a successful join
    this.joined = false;
    this.closedFlag = false;
    this.closeInfo = null;
    this.onControl = () => {};
    this.onDatagram = () => {};
    this.onPreviewStream = () => {};
    this.onClosed = () => {};
    this.pendingDatagrams = 0;
    this.ctrlW = null;
    this.ctrlR = null;
    this.ctrlFrames = new ControlFrameReader();
    try {
      wt.datagrams.incomingHighWaterMark = 1 << 14; // keyframes arrive as hundreds of fragments in one burst
    } catch {}
    try {
      // Media older than this is useless (and the receiver drops the frame anyway): let the browser discard stale datagrams
      // instead of sending them late. Without it a congested/CPU-starved sender queued seconds of video (measured: 2.8 s).
      wt.datagrams.outgoingMaxAge = 250;
    } catch {}
    wt.closed.then(
      (info) => this.#closed(info || { closeCode: 0, reason: "" }),
      (err) => this.#closed({ error: err }),
    );
  }

  #closed(info) {
    if (this.closedFlag) return;
    this.closedFlag = true;
    this.closeInfo = info;
    this.onClosed(info);
  }

  /**
   * Open the control stream and join. Resolves with the PresenceSnapshot; rejects with JoinRejectedError.
   * ConnIdCollision is retried with a fresh ConnId on the same connection (spec §3.1).
   */
  async join({ roomId, displayName }) {
    const bidi = await this.wt.createBidirectionalStream();
    this.ctrlW = bidi.writable.getWriter();
    this.ctrlR = bidi.readable.getReader();
    let snapshot = null;
    for (let attempt = 0; attempt < 5 && !snapshot; attempt++) {
      // The relay only notices the stream once data arrives, so Join goes out immediately.
      let msg;
      try {
        await this.#write(encodeClientToRelay({ type: "Join", connId: this.connId, roomId, displayName }));
        msg = await this.#readOne();
      } catch (e) {
        // The relay delivers JoinRejected on the control stream before it closes, so a normal refusal never lands here.
        // We get here when the connection died without any reply: an undecodable first frame (the relay just drops
        // those), a relay going down mid-join, or an older relay. Let closeInfo arrive and map a close code if the browser
        // reports one (Chromium does not, see CLOSE_CODES); otherwise don't pretend to know why.
        await Promise.race([this.wt.closed.catch(() => null), new Promise((r) => setTimeout(r, 150))]);
        const kind = classifyClose(this.closeInfo);
        if (kind === "room-full") throw new JoinRejectedError("RoomFull");
        if (kind === "malformed") throw new JoinRejectedError("Malformed");
        const err =
          kind === "relay-shutdown"
            ? new ConnectError("The relay is shutting down.", { code: "relay-shutdown", cause: e })
            : new ConnectError("The relay closed the connection right after the join request without answering. The room may be full (8 members maximum), or the relay refused the link.", { code: "join-no-reply", cause: e });
        err.closeInfo = this.closeInfo;
        throw err;
      }
      if (msg.type === "PresenceSnapshot") snapshot = msg;
      else if (msg.type === "JoinRejected" && msg.reason === "ConnIdCollision") this.connId = randomConnId();
      else if (msg.type === "JoinRejected") throw new JoinRejectedError(msg.reason);
      else throw new Error(`unexpected message before join acknowledgement: ${msg.type}`);
    }
    if (!snapshot) throw new JoinRejectedError("ConnIdCollision");
    this.nonce = new NonceSequence(this.connId);
    this.joined = true;
    this.#controlLoop();
    this.#datagramLoop();
    this.#uniLoop();
    return snapshot;
  }

  #pendingMsgs = [];
  async #readOne() {
    while (!this.#pendingMsgs.length) {
      const { value, done } = await this.ctrlR.read();
      if (done) throw new Error("control stream ended before the join completed");
      for (const f of this.ctrlFrames.push(value)) this.#pendingMsgs.push(decodeRelayToClient(f));
    }
    return this.#pendingMsgs.shift();
  }

  async #controlLoop() {
    try {
      for (;;) {
        while (this.#pendingMsgs.length) this.onControl(this.#pendingMsgs.shift());
        const { value, done } = await this.ctrlR.read();
        if (done) break;
        for (const f of this.ctrlFrames.push(value)) {
          try {
            this.onControl(decodeRelayToClient(f));
          } catch (e) {
            console.warn("[ssshare] bad control message", e);
          }
        }
      }
    } catch (e) {
      // surfaced through wt.closed
    }
    this.#closed(this.closeInfo || { reason: "control stream ended" });
  }

  async #datagramLoop() {
    try {
      const reader = this.wt.datagrams.readable.getReader();
      for (;;) {
        const { value, done } = await reader.read();
        if (done) break;
        this.onDatagram(value);
      }
    } catch (e) {
      // closed
    }
  }

  async #uniLoop() {
    try {
      const reader = this.wt.incomingUnidirectionalStreams.getReader();
      for (;;) {
        const { value: stream, done } = await reader.read();
        if (done) break;
        readAll(stream, MAX_PREVIEW_STREAM_BYTES).then((bytes) => this.onPreviewStream(bytes), () => {});
      }
    } catch (e) {
      // closed
    }
  }

  #write(bytes) {
    return this.ctrlW.write(frameControl(bytes));
  }

  /** Send a control message (ordered with every earlier call). */
  send(msg) {
    return this.#write(encodeClientToRelay(msg));
  }

  #dgWriter = null;
  /** Queue one datagram. Never throws; check `pendingDatagrams` for backpressure. */
  sendDatagram(bytes) {
    if (this.closedFlag) return;
    if (!this.#dgWriter) {
      const dg = this.wt.datagrams;
      const writable = typeof dg.createWritable === "function" ? dg.createWritable() : dg.writable;
      this.#dgWriter = writable.getWriter();
    }
    this.pendingDatagrams++;
    this.#dgWriter.write(bytes).then(
      () => this.pendingDatagrams--,
      () => this.pendingDatagrams--,
    );
  }

  /** One uni stream per preview blob (spec §7.1). */
  async sendUniStream(bytes) {
    const s = await this.wt.createUnidirectionalStream();
    const w = s.getWriter();
    await w.write(bytes);
    await w.close();
  }

  async getStats() {
    try {
      return await this.wt.getStats();
    } catch {
      return null;
    }
  }

  /**
   * Leave and close. Datagrams written just before a close can be lost, so give the transport a moment
   * to flush first (the relay smoke test saw exactly that).
   */
  async close({ graceful = true, flushMs = 150 } = {}) {
    if (this.closedFlag) return;
    if (graceful) {
      try {
        if (this.joined) await this.send({ type: "Leave" });
        await new Promise((r) => setTimeout(r, flushMs));
      } catch {}
    }
    try {
      this.wt.close({ closeCode: 0, reason: "bye" });
    } catch {}
    this.#closed({ closeCode: 0, reason: "closed locally" });
  }
}
