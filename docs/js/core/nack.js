// Selective retransmission (NACK), the browser side of spec §6.6 (task #111, the follow-up of the native/relay #104):
// the capability probe's bytes, the kill switch and the sender's ring of recently sent video datagrams. The viewer half
// (detecting holes, asking, holding frames in order) is `Reassembler` in fragment.js; `RoomClient` connects the two to the wire.
// Mirrors `client/src/nack.rs`.

import { MEDIA_FLAG_RETRANSMIT, NACK_TAG_HELLO } from "./protocol.js";

/** The 1-byte Hello datagram (spec §6.6): "I send NACKs for what I watch and re-send fragments of what I share". */
export const HELLO_DATAGRAM = Uint8Array.of(NACK_TAG_HELLO);
/** Hello goes out once per second, at most this many times, until the relay's HelloAck arrives (an old relay never answers). */
export const HELLO_MAX_TRIES = 10;
export const HELLO_INTERVAL_MS = 1000;

/** What the sender keeps: the last 2 s / 8 MiB of video datagrams per stream (the same as the relay's cache). */
export const SEND_RING_MS = 2000;
export const SEND_RING_BYTES = 8 * 1024 * 1024;
/** Most fragments one request is answered with (mirrors the relay's cap). */
export const MAX_RESEND_FRAGMENTS = 256;

const OFF = new Set(["0", "off", "false", "no"]);

/**
 * The kill switch, mirroring native `SSS_DISABLE_NACK=1`: NACK is off when the page URL has `?nack=0` (or `off`/`false`),
 * or `localStorage["sss-nack"]` is `0`. Then the client never says Hello, so the relay never forwards requests to it and,
 * as a viewer, it never sees a HelloAck and never asks: exactly the pre-#111 behaviour.
 */
export function nackEnabledByDefault() {
  try {
    const q = new URLSearchParams(globalThis.location?.search ?? "").get("nack");
    if (q != null) return !OFF.has(q.toLowerCase());
  } catch {}
  try {
    const v = globalThis.localStorage?.getItem("sss-nack");
    if (v != null) return !OFF.has(v.toLowerCase());
  } catch {}
  return true;
}

/**
 * The video datagrams this client sent recently, per stream, so the relay can ask for the ones that never reached it.
 * Holds references to the `Uint8Array`s that went to the transport (no copy until a re-send, which flips one flag bit).
 */
export class SendRing {
  constructor({ now = () => performance.now() } = {}) {
    this.now = now;
    this.streams = new Map(); // streamId -> { frames: Map(frameId -> {at, datagrams, bytes}), bytes }
    this.resent = 0; // fragments re-sent on request (stats)
    this.requests = 0; // requests received for one of our streams
    this.missed = 0; // requests (fragments) we could no longer answer
  }

  /** Remember one frame's datagrams (in fragment order). */
  record(streamId, frameId, datagrams) {
    const t = this.now();
    let s = this.streams.get(streamId);
    if (!s) this.streams.set(streamId, (s = { frames: new Map(), bytes: 0 }));
    let bytes = 0;
    for (const d of datagrams) bytes += d.length;
    s.frames.set(frameId, { at: t, datagrams, bytes });
    s.bytes += bytes;
    for (const [id, f] of s.frames) {
      if (t - f.at <= SEND_RING_MS && s.bytes <= SEND_RING_BYTES) break; // Map order = send order: the rest is younger
      s.frames.delete(id);
      s.bytes -= f.bytes;
    }
  }

  /** The stream ended (or the connection did): drop what it cached. */
  forget(streamId) {
    this.streams.delete(streamId);
  }

  clear() {
    this.streams.clear();
  }

  /**
   * The datagrams answering a relay-forwarded request: the requested fragments that are still cached, byte for byte
   * except for the retransmit flag (spec §6.6). Nothing is re-encrypted: a re-sent slice is the same slice.
   * @param {{streamId:number, frameId:number, first:number, count:number}} range
   * @returns {Uint8Array[]}
   */
  resends({ streamId, frameId, first, count }) {
    this.requests++;
    const f = this.streams.get(streamId)?.frames.get(frameId);
    const want = Math.min(count, MAX_RESEND_FRAGMENTS);
    const out = [];
    if (f) {
      for (let i = first; i < first + want && i < f.datagrams.length; i++) {
        const d = f.datagrams[i];
        if (!d || d.length < 2) continue;
        const copy = d.slice();
        copy[1] |= MEDIA_FLAG_RETRANSMIT;
        out.push(copy);
      }
    }
    this.missed += want - out.length;
    return out;
  }
}
