// AES-256-GCM frame/preview encryption via WebCrypto (spec §5, §6.4, §7.1),
// mirroring `client/src/net.rs`:
//
//   nonce      = ConnId (u64 BE) ‖ counter (u32 BE)               12 bytes
//   aad        = [WIRE_VERSION, stream_id]                         2 bytes
//   frame      = AES-GCM(pts_us (u64 BE) ‖ codec bytes)            + 16-byte tag
//   preview    = AES-GCM(jpeg bytes)                               + 16-byte tag
//
// ONE NonceSequence per connection is shared by video, audio and previews.

import { GCM_TAG_LEN, WIRE_VERSION } from "./protocol.js";

export class NonceSpaceExhausted extends Error {
  constructor() {
    super("connection's nonce counter space is exhausted; a new connection (fresh ConnId) is required");
    this.name = "NonceSpaceExhausted";
  }
}

/** Single shared, never-wrapping u32 counter for a connection (spec §5.1/§5.2). */
export class NonceSequence {
  /** @param {bigint} connId  @param {number} [start=0] (tests only) */
  constructor(connId, start = 0) {
    this.connId = connId;
    this.next = start;
  }
  /** Consume the next counter value: returns `{ nonce, counter }`. u32::MAX is the last legal value. */
  take() {
    if (this.next > 0xffffffff) throw new NonceSpaceExhausted();
    const counter = this.next++;
    return { nonce: buildNonce(this.connId, counter), counter };
  }
  get exhausted() {
    return this.next > 0xffffffff;
  }
}

export function buildNonce(connId, counter) {
  const n = new Uint8Array(12);
  const dv = new DataView(n.buffer);
  dv.setBigUint64(0, connId, false);
  dv.setUint32(8, counter, false);
  return n;
}

export function buildAad(streamId) {
  return new Uint8Array([WIRE_VERSION, streamId]);
}

export function importRoomKey(keyBytes) {
  if (keyBytes.length !== 32) throw new Error("room key must be 32 bytes");
  return crypto.subtle.importKey("raw", keyBytes, { name: "AES-GCM" }, false, ["encrypt", "decrypt"]);
}

async function aeadEncrypt(key, streamId, nonce, plaintext) {
  const ct = await crypto.subtle.encrypt(
    { name: "AES-GCM", iv: nonce, additionalData: buildAad(streamId), tagLength: GCM_TAG_LEN * 8 },
    key,
    plaintext,
  );
  return new Uint8Array(ct);
}

async function aeadDecrypt(key, streamId, nonce, ciphertext) {
  const pt = await crypto.subtle.decrypt(
    { name: "AES-GCM", iv: nonce, additionalData: buildAad(streamId), tagLength: GCM_TAG_LEN * 8 },
    key,
    ciphertext,
  );
  return new Uint8Array(pt);
}

/** Encrypt one whole media frame: `ptsUs` (Number, µs) is prefixed inside the plaintext. */
export function encryptFrame(key, streamId, nonce, ptsUs, payload) {
  const plain = new Uint8Array(8 + payload.length);
  new DataView(plain.buffer).setBigUint64(0, BigInt(Math.max(0, Math.round(ptsUs))), false);
  plain.set(payload, 8);
  return aeadEncrypt(key, streamId, nonce, plain);
}

/** Decrypt a reassembled frame. Resolves `{ ptsUs: number, payload: Uint8Array }`; rejects on tag failure. */
export async function decryptFrame(key, connId, frameId, streamId, ciphertext) {
  const plain = await aeadDecrypt(key, streamId, buildNonce(connId, frameId), ciphertext);
  if (plain.length < 8) throw new Error("decrypted frame shorter than the mandatory 8-byte PTS prefix");
  const ptsUs = Number(new DataView(plain.buffer, plain.byteOffset, 8).getBigUint64(0, false));
  return { ptsUs, payload: plain.subarray(8) };
}

export function encryptPreview(key, streamId, nonce, jpeg) {
  return aeadEncrypt(key, streamId, nonce, jpeg);
}

export function decryptPreview(key, connId, counter, streamId, ciphertext) {
  return aeadDecrypt(key, streamId, buildNonce(connId, counter), ciphertext);
}
