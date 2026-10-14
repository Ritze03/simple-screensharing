// Wire protocol: a line-for-line port of `protocol/src/lib.rs` (binding spec:
// superdoc/architecture/protocol-spec.md). Variant ORDER is the postcard
// discriminant, so the order of the tables below is frozen.
//
// Representation: ConnId (u64) is a BigInt here; RoomId is a Uint8Array(16);
// StreamKind is the string "video" | "audio".

import { PostcardReader, PostcardWriter } from "./postcard.js";

export const WIRE_VERSION = 0x01;
export const MAX_NAME_LEN = 64;
export const MAX_ROOM_MEMBERS = 8;
export const MAX_CONTROL_FRAME_LEN = 64 * 1024;
export const STREAM_TYPE_PREVIEW = 0x01;
export const MEDIA_HEADER_LEN = 19;
export const MEDIA_FLAG_KEYFRAME = 0b01;
export const MEDIA_FLAG_STALLED = 0b10;
export const PREVIEW_HEADER_LEN = 15;
export const MAX_MEDIA_DATAGRAM_LEN = 1000;
export const WEBTRANSPORT_PATH = "/ssshare";
/** GCM tag appended to every ciphertext. */
export const GCM_TAG_LEN = 16;

const KINDS = ["video", "audio"]; // StreamKind: Video = 0, Audio = 1
const REJECT_REASONS = ["RoomFull", "ConnIdCollision", "Malformed"];

const kindToWire = (k) => {
  const i = KINDS.indexOf(k);
  if (i < 0) throw new Error(`unknown stream kind ${k}`);
  return i;
};
const kindFromWire = (i) => {
  if (i >= KINDS.length) throw new Error(`unknown StreamKind ${i}`);
  return KINDS[i];
};
const checkU8 = (v, what) => {
  if (!Number.isInteger(v) || v < 0 || v > 0xff) throw new RangeError(`${what} must be a u8, got ${v}`);
  return v;
};
const checkU16 = (v, what) => {
  if (!Number.isInteger(v) || v < 0 || v > 0xffff) throw new RangeError(`${what} must be a u16, got ${v}`);
  return v;
};

/** Truncate to at most `max` UTF-8 bytes on a char boundary (the relay does the same). */
export function clampUtf8(s, max = MAX_NAME_LEN) {
  const enc = new TextEncoder();
  if (enc.encode(s).length <= max) return s;
  let out = "";
  let used = 0;
  for (const ch of s) {
    const n = enc.encode(ch).length;
    if (used + n > max) break;
    out += ch;
    used += n;
  }
  return out;
}

// ---------------------------------------------------------------------
// ClientToRelay
// ---------------------------------------------------------------------
// { type: "Join", connId, roomId, displayName }
// { type: "Leave" }
// { type: "AnnounceStream", streamId, kind, label, width, height }
// { type: "StopStream", streamId }
// { type: "Subscribe", target, streamId }
// { type: "Unsubscribe", target, streamId }
const C2R = ["Join", "Leave", "AnnounceStream", "StopStream", "Subscribe", "Unsubscribe"];

export function encodeClientToRelay(m) {
  const w = new PostcardWriter();
  const tag = C2R.indexOf(m.type);
  if (tag < 0) throw new Error(`unknown ClientToRelay type ${m.type}`);
  w.varint(tag);
  switch (m.type) {
    case "Join":
      if (!(m.roomId instanceof Uint8Array) || m.roomId.length !== 16) throw new Error("roomId must be 16 bytes");
      w.varint(m.connId).bytesFixed(m.roomId).string(m.displayName);
      break;
    case "Leave":
      break;
    case "AnnounceStream":
      w.u8(checkU8(m.streamId, "streamId")).u8(kindToWire(m.kind)).string(m.label)
        .varint(checkU16(m.width, "width")).varint(checkU16(m.height, "height"));
      break;
    case "StopStream":
      w.u8(checkU8(m.streamId, "streamId"));
      break;
    case "Subscribe":
    case "Unsubscribe":
      w.varint(m.target).u8(checkU8(m.streamId, "streamId"));
      break;
  }
  return w.finish();
}

export function decodeClientToRelay(bytes) {
  const r = new PostcardReader(bytes);
  const type = C2R[r.varint()];
  switch (type) {
    case "Join":
      return { type, connId: r.varintBig(), roomId: r.bytesFixed(16), displayName: r.string() };
    case "Leave":
      return { type };
    case "AnnounceStream":
      return { type, streamId: r.u8(), kind: kindFromWire(r.u8()), label: r.string(), width: r.varint(), height: r.varint() };
    case "StopStream":
      return { type, streamId: r.u8() };
    case "Subscribe":
    case "Unsubscribe":
      return { type, target: r.varintBig(), streamId: r.u8() };
    default:
      throw new Error("unknown ClientToRelay discriminant");
  }
}

// ---------------------------------------------------------------------
// RelayToClient
// ---------------------------------------------------------------------
// Member:     { connId, displayName, streamCount }
// StreamInfo: { owner, streamId, kind, label, width, height }
// { type: "PresenceSnapshot", members: Member[], streams: StreamInfo[] }
// { type: "PresenceDelta", event }
//     event: { type: "MemberJoined", member } | { type: "MemberLeft", connId }
//          | { type: "StreamStarted", stream } | { type: "StreamStopped", connId, streamId }
// { type: "SubscriberCount", streamId, count }
// { type: "KeyframeRequest", streamId }
// { type: "JoinRejected", reason }   reason: "RoomFull" | "ConnIdCollision" | "Malformed"
const R2C = ["PresenceSnapshot", "PresenceDelta", "SubscriberCount", "KeyframeRequest", "JoinRejected"];
const EVENTS = ["MemberJoined", "MemberLeft", "StreamStarted", "StreamStopped"];

const writeMember = (w, m) => w.varint(m.connId).string(m.displayName).u8(checkU8(m.streamCount, "streamCount"));
const readMember = (r) => ({ connId: r.varintBig(), displayName: r.string(), streamCount: r.u8() });
const writeStreamInfo = (w, s) =>
  w.varint(s.owner).u8(checkU8(s.streamId, "streamId")).u8(kindToWire(s.kind)).string(s.label)
    .varint(checkU16(s.width, "width")).varint(checkU16(s.height, "height"));
const readStreamInfo = (r) => ({
  owner: r.varintBig(), streamId: r.u8(), kind: kindFromWire(r.u8()), label: r.string(), width: r.varint(), height: r.varint(),
});

export function encodeRelayToClient(m) {
  const w = new PostcardWriter();
  const tag = R2C.indexOf(m.type);
  if (tag < 0) throw new Error(`unknown RelayToClient type ${m.type}`);
  w.varint(tag);
  switch (m.type) {
    case "PresenceSnapshot":
      w.varint(m.members.length);
      m.members.forEach((x) => writeMember(w, x));
      w.varint(m.streams.length);
      m.streams.forEach((x) => writeStreamInfo(w, x));
      break;
    case "PresenceDelta": {
      const e = m.event;
      const et = EVENTS.indexOf(e.type);
      if (et < 0) throw new Error(`unknown PresenceEvent ${e.type}`);
      w.varint(et);
      if (e.type === "MemberJoined") writeMember(w, e.member);
      else if (e.type === "MemberLeft") w.varint(e.connId);
      else if (e.type === "StreamStarted") writeStreamInfo(w, e.stream);
      else w.varint(e.connId).u8(e.streamId);
      break;
    }
    case "SubscriberCount":
      w.u8(m.streamId).varint(m.count);
      break;
    case "KeyframeRequest":
      w.u8(m.streamId);
      break;
    case "JoinRejected": {
      const i = REJECT_REASONS.indexOf(m.reason);
      if (i < 0) throw new Error(`unknown RejectReason ${m.reason}`);
      w.varint(i);
      break;
    }
  }
  return w.finish();
}

export function decodeRelayToClient(bytes) {
  const r = new PostcardReader(bytes);
  const type = R2C[r.varint()];
  switch (type) {
    case "PresenceSnapshot": {
      const members = Array.from({ length: r.varint() }, () => readMember(r));
      const streams = Array.from({ length: r.varint() }, () => readStreamInfo(r));
      return { type, members, streams };
    }
    case "PresenceDelta": {
      const et = EVENTS[r.varint()];
      let event;
      if (et === "MemberJoined") event = { type: et, member: readMember(r) };
      else if (et === "MemberLeft") event = { type: et, connId: r.varintBig() };
      else if (et === "StreamStarted") event = { type: et, stream: readStreamInfo(r) };
      else if (et === "StreamStopped") event = { type: et, connId: r.varintBig(), streamId: r.u8() };
      else throw new Error("unknown PresenceEvent discriminant");
      return { type, event };
    }
    case "SubscriberCount":
      return { type, streamId: r.u8(), count: r.varint() };
    case "KeyframeRequest":
      return { type, streamId: r.u8() };
    case "JoinRejected": {
      const reason = REJECT_REASONS[r.varint()];
      if (!reason) throw new Error("unknown RejectReason discriminant");
      return { type, reason };
    }
    default:
      throw new Error("unknown RelayToClient discriminant");
  }
}

// ---------------------------------------------------------------------
// Control framing: u32 BE length ‖ postcard payload
// ---------------------------------------------------------------------

export function frameControl(payload) {
  if (payload.length > MAX_CONTROL_FRAME_LEN) throw new Error("control message too large");
  const out = new Uint8Array(4 + payload.length);
  new DataView(out.buffer).setUint32(0, payload.length, false);
  out.set(payload, 4);
  return out;
}

/** Incremental de-framer: feed arbitrary chunks, get whole payloads back. */
export class ControlFrameReader {
  constructor() {
    this.chunks = [];
    this.have = 0;
  }
  push(chunk) {
    if (chunk.length) {
      this.chunks.push(chunk);
      this.have += chunk.length;
    }
    const out = [];
    for (;;) {
      if (this.have < 4) break;
      const head = this.#take(4, false);
      const len = new DataView(head.buffer, head.byteOffset, 4).getUint32(0, false);
      if (len > MAX_CONTROL_FRAME_LEN) throw new Error(`control frame claims ${len} bytes, exceeding the ${MAX_CONTROL_FRAME_LEN}-byte cap`);
      if (this.have < 4 + len) break;
      this.#take(4, true);
      out.push(this.#take(len, true));
    }
    return out;
  }
  // Peek (consume=false) or consume `n` bytes from the front.
  #take(n, consume) {
    const out = new Uint8Array(n);
    let got = 0;
    let idx = 0;
    while (got < n) {
      const c = this.chunks[idx];
      const take = Math.min(c.length, n - got);
      out.set(c.subarray(0, take), got);
      got += take;
      if (consume) {
        if (take === c.length) {
          this.chunks.splice(idx, 1);
        } else {
          this.chunks[idx] = c.subarray(take);
        }
      } else {
        idx++;
      }
    }
    if (consume) this.have -= n;
    return out;
  }
}

// ---------------------------------------------------------------------
// Fixed binary headers (all big-endian)
// ---------------------------------------------------------------------

/** Write the 19-byte media header into `out` at offset 0. */
export function packMediaHeader(out, h) {
  const dv = new DataView(out.buffer, out.byteOffset, out.byteLength);
  out[0] = h.version ?? WIRE_VERSION;
  out[1] = h.flags;
  dv.setBigUint64(2, h.connId, false);
  out[10] = h.streamId;
  dv.setUint32(11, h.frameId, false);
  dv.setUint16(15, h.fragmentIndex, false);
  dv.setUint16(17, h.fragmentCount, false);
}

/** Parse a datagram: returns `{ header, payload }` (payload is a view). */
export function unpackMediaHeader(bytes) {
  if (bytes.length < MEDIA_HEADER_LEN) throw new Error(`media datagram too short: ${bytes.length} bytes`);
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return {
    header: {
      version: bytes[0],
      flags: bytes[1],
      connId: dv.getBigUint64(2, false),
      streamId: bytes[10],
      frameId: dv.getUint32(11, false),
      fragmentIndex: dv.getUint16(15, false),
      fragmentCount: dv.getUint16(17, false),
      keyframe: (bytes[1] & MEDIA_FLAG_KEYFRAME) !== 0,
      stalled: (bytes[1] & MEDIA_FLAG_STALLED) !== 0,
    },
    payload: bytes.subarray(MEDIA_HEADER_LEN),
  };
}

export function packPreviewHeader({ connId, streamId, counter }) {
  const out = new Uint8Array(PREVIEW_HEADER_LEN);
  const dv = new DataView(out.buffer);
  out[0] = STREAM_TYPE_PREVIEW;
  out[1] = WIRE_VERSION;
  dv.setBigUint64(2, connId, false);
  out[10] = streamId;
  dv.setUint32(11, counter, false);
  return out;
}

export function unpackPreviewHeader(bytes) {
  if (bytes.length < PREVIEW_HEADER_LEN) throw new Error(`preview stream header too short: ${bytes.length} bytes`);
  const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return {
    header: {
      streamType: bytes[0],
      version: bytes[1],
      connId: dv.getBigUint64(2, false),
      streamId: bytes[10],
      counter: dv.getUint32(11, false),
    },
    payload: bytes.subarray(PREVIEW_HEADER_LEN),
  };
}

// ---------------------------------------------------------------------
// ConnId helpers (hex string form is the public, JSON-friendly id)
// ---------------------------------------------------------------------

export const connIdToHex = (id) => BigInt.asUintN(64, id).toString(16).padStart(16, "0");
export const connIdFromHex = (s) => BigInt("0x" + s);

export function randomConnId() {
  const b = crypto.getRandomValues(new Uint8Array(8));
  return new DataView(b.buffer).getBigUint64(0, false);
}
