// Unit tests for docs/js/core, run in the browser by dev/tests.html (headless: `node dev/headless.mjs run dev/tests.html`).
// Cross-language vectors (dev/vectors.json) are generated from the REAL Rust crates (protocol/ + client lib) by a
// throwaway generator; see dev/README.md. Reverse vectors (JS-made, to be decrypted/parsed by Rust) are returned in
// the page result under `reverse`.

import { PostcardReader, PostcardWriter } from "../js/core/postcard.js";
import * as P from "../js/core/protocol.js";
import { base64urlDecode, bytesToHex, createRoom, formatJoinLink, hexToBytes, JoinLinkError, parseJoinLink, splitHostPort } from "../js/core/joinlink.js";
import { NonceSequence, NonceSpaceExhausted, buildAad, buildNonce, decryptFrame, decryptPreview, encryptFrame, encryptPreview, importRoomKey } from "../js/core/crypto.js";
import { Reassembler, fragmentFrame } from "../js/core/fragment.js";
import { deriveCodecString, sniffCodec } from "../js/core/codec.js";
import { DECODE_QUEUE_CATCHUP_THRESHOLD, JitterBuffer, MAX_DECODE_QUEUE, clampCatchUpSpeedPercent, clampMaxJitterBufferMs } from "../js/core/jitter.js";
import { VideoReceiver } from "../js/core/video-rx.js";
import { ENCODER_PRESETS } from "../js/core/video-tx.js";

const tests = [];
export const test = (name, fn) => tests.push({ name, fn });

class AssertionError extends Error {}
const fail = (m) => {
  throw new AssertionError(m);
};
const eq = (a, b, what = "") => {
  if (!deepEqual(a, b)) fail(`${what} expected ${show(b)} got ${show(a)}`);
};
const ok = (c, what = "assertion failed") => {
  if (!c) fail(what);
};
const show = (v) => JSON.stringify(v, (_, x) => (typeof x === "bigint" ? `${x}n` : x instanceof Uint8Array ? `u8[${bytesToHex(x)}]` : x));
function deepEqual(a, b) {
  if (a instanceof Uint8Array || b instanceof Uint8Array) {
    return a instanceof Uint8Array && b instanceof Uint8Array && a.length === b.length && a.every((x, i) => x === b[i]);
  }
  if (typeof a === "bigint" || typeof b === "bigint") return a === b;
  if (a && b && typeof a === "object" && typeof b === "object") {
    const ka = Object.keys(a);
    const kb = Object.keys(b);
    return ka.length === kb.length && ka.every((k) => deepEqual(a[k], b[k]));
  }
  return a === b;
}
async function throwsAsync(fn, what) {
  try {
    await fn();
  } catch (e) {
    return e;
  }
  fail(`${what}: expected a rejection`);
}
function throws(fn, what, cls) {
  try {
    fn();
  } catch (e) {
    if (cls && !(e instanceof cls)) fail(`${what}: wrong error type ${e && e.name}`);
    return e;
  }
  fail(`${what}: expected a throw`);
}

// Vector JSON uses hex strings for u64 / room ids; convert to the JS shapes.
function revive(v) {
  if (Array.isArray(v)) return v.map(revive);
  if (v && typeof v === "object") {
    const out = {};
    for (const [k, x] of Object.entries(v)) {
      if (["connId", "owner", "target"].includes(k) && typeof x === "string") out[k] = BigInt("0x" + x);
      else if (k === "roomId" && typeof x === "string") out[k] = hexToBytes(x);
      else out[k] = revive(x);
    }
    return out;
  }
  return v;
}

let V = null;
const vec = async () => V ?? (V = await (await fetch(new URL("./vectors.json", import.meta.url))).json());
function randomBytes(n) {
  const out = new Uint8Array(n);
  for (let i = 0; i < n; i += 65536) crypto.getRandomValues(out.subarray(i, Math.min(n, i + 65536)));
  return out;
}

// ------------------------------------------------------------------------------------------------ postcard

test("postcard: varint encoding and u64 limits", () => {
  const cases = [[0n, "00"], [127n, "7f"], [128n, "8001"], [300n, "ac02"], [(1n << 64n) - 1n, "ffffffffffffffffff01"]];
  for (const [n, h] of cases) {
    eq(bytesToHex(new PostcardWriter().varint(n).finish()), h, `varint(${n})`);
    eq(new PostcardReader(hexToBytes(h)).varintBig(), n, `read varint ${h}`);
  }
  throws(() => new PostcardReader(hexToBytes("80")).varint(), "truncated varint");
  throws(() => new PostcardWriter().varint(-1), "negative varint");
});

test("postcard: ClientToRelay vectors (from the Rust types) encode and decode byte-exact", async () => {
  const v = await vec();
  ok(v.postcard.c2r.length >= 8);
  for (const { msg, hex, framedHex } of v.postcard.c2r) {
    const m = revive(msg);
    eq(bytesToHex(P.encodeClientToRelay(m)), hex, `encode ${msg.type}`);
    eq(P.decodeClientToRelay(hexToBytes(hex)), m, `decode ${msg.type}`);
    eq(bytesToHex(P.frameControl(P.encodeClientToRelay(m))), framedHex, `frame ${msg.type}`);
  }
});

test("postcard: RelayToClient vectors encode and decode byte-exact", async () => {
  const v = await vec();
  ok(v.postcard.r2c.length >= 10);
  for (const { msg, hex } of v.postcard.r2c) {
    const m = revive(msg);
    eq(bytesToHex(P.encodeRelayToClient(m)), hex, `encode ${msg.type}`);
    eq(P.decodeRelayToClient(hexToBytes(hex)), m, `decode ${msg.type}`);
  }
});

test("postcard: literal hex from protocol/src/lib.rs wire_bytes_unchanged", () => {
  const join = P.encodeClientToRelay({ type: "Join", connId: 0x0102030405060708n, roomId: new Uint8Array(16).fill(7), displayName: "alice" });
  eq(bytesToHex(join), "00888e98a8c0e08081010707070707070707070707070707070705616c696365");
  eq(bytesToHex(P.encodeClientToRelay({ type: "Leave" })), "01");
  eq(bytesToHex(P.encodeClientToRelay({ type: "AnnounceStream", streamId: 3, kind: "video", label: "Screen 1", width: 1920, height: 1080 })), "0203000853637265656e2031800fb808");
  eq(bytesToHex(P.encodeClientToRelay({ type: "StopStream", streamId: 3 })), "0303");
  eq(bytesToHex(P.encodeClientToRelay({ type: "Subscribe", target: 0x1112131415161718n, streamId: 4 })), "0498aed8a8c1e284891104");
  eq(bytesToHex(P.encodeClientToRelay({ type: "Unsubscribe", target: 0x1112131415161718n, streamId: 4 })), "0598aed8a8c1e284891104");
  const m = { connId: 0x0102030405060708n, displayName: "bob", streamCount: 2 };
  const si = { owner: 0x1112131415161718n, streamId: 5, kind: "audio", label: "Screen 1", width: 1920, height: 1080 };
  eq(bytesToHex(P.encodeRelayToClient({ type: "PresenceSnapshot", members: [m], streams: [si] })), "0001888e98a8c0e080810103626f62020198aed8a8c1e284891105010853637265656e2031800fb808");
  eq(bytesToHex(P.encodeRelayToClient({ type: "SubscriberCount", streamId: 3, count: 300 })), "0203ac02");
  eq(bytesToHex(P.encodeRelayToClient({ type: "KeyframeRequest", streamId: 3 })), "0303");
  eq(bytesToHex(P.encodeRelayToClient({ type: "JoinRejected", reason: "RoomFull" })), "0400");
});

test("postcard: malformed input is rejected, not misparsed", () => {
  throws(() => P.decodeRelayToClient(hexToBytes("09")), "unknown RelayToClient discriminant");
  throws(() => P.decodeRelayToClient(hexToBytes("0408")), "unknown RejectReason");
  throws(() => P.decodeRelayToClient(hexToBytes("0103")), "unknown PresenceEvent");
  throws(() => P.decodeRelayToClient(hexToBytes("0001")), "truncated snapshot");
  throws(() => P.decodeClientToRelay(hexToBytes("09")), "unknown ClientToRelay discriminant");
  throws(() => P.encodeClientToRelay({ type: "StopStream", streamId: 256 }), "streamId out of range");
  throws(() => P.encodeClientToRelay({ type: "AnnounceStream", streamId: 1, kind: "video", label: "", width: 70000, height: 1 }), "width out of range");
});

test("protocol: clampUtf8 truncates on a char boundary like the relay", () => {
  eq(P.clampUtf8("abc", 64), "abc");
  eq(P.clampUtf8("a".repeat(70)).length, 64);
  const s = "é".repeat(40); // 80 bytes
  const c = P.clampUtf8(s);
  eq(new TextEncoder().encode(c).length, 64);
  eq(c, "é".repeat(32));
  eq(P.clampUtf8("😀".repeat(20)), "😀".repeat(16)); // 4-byte chars
});

// ------------------------------------------------------------------------------------------------ framing / headers

test("control frames: de-framer handles split, coalesced and byte-wise delivery; enforces the 64 KiB cap", () => {
  const msgs = [{ type: "Leave" }, { type: "StopStream", streamId: 9 }, { type: "Join", connId: 5n, roomId: new Uint8Array(16), displayName: "x".repeat(60) }];
  const wire = [];
  for (const m of msgs) wire.push(...P.frameControl(P.encodeClientToRelay(m)));
  const bytes = Uint8Array.from(wire);
  for (const chunk of [1, 2, 3, 5, 7, 1000]) {
    const r = new P.ControlFrameReader();
    const got = [];
    for (let i = 0; i < bytes.length; i += chunk) got.push(...r.push(bytes.subarray(i, i + chunk)));
    eq(got.map((p) => P.decodeClientToRelay(p)), msgs, `chunk=${chunk}`);
  }
  const big = new Uint8Array(4);
  new DataView(big.buffer).setUint32(0, P.MAX_CONTROL_FRAME_LEN + 1);
  throws(() => new P.ControlFrameReader().push(big), "oversized frame");
  throws(() => P.frameControl(new Uint8Array(P.MAX_CONTROL_FRAME_LEN + 1)), "oversized outgoing frame");
  eq(P.frameControl(new Uint8Array(P.MAX_CONTROL_FRAME_LEN)).length, P.MAX_CONTROL_FRAME_LEN + 4);
});

test("headers: media and preview headers match the Rust pack()", async () => {
  const v = await vec();
  for (const h of v.headers.media) {
    const buf = new Uint8Array(P.MEDIA_HEADER_LEN);
    P.packMediaHeader(buf, { version: h.version, flags: h.flags, connId: BigInt("0x" + h.connId), streamId: h.streamId, frameId: h.frameId, fragmentIndex: h.fragmentIndex, fragmentCount: h.fragmentCount });
    eq(bytesToHex(buf), h.hex, "pack");
    const { header, payload } = P.unpackMediaHeader(hexToBytes(h.hex + "abcd"));
    eq(header.connId, BigInt("0x" + h.connId));
    eq([header.version, header.flags, header.streamId, header.frameId, header.fragmentIndex, header.fragmentCount], [h.version, h.flags, h.streamId, h.frameId, h.fragmentIndex, h.fragmentCount]);
    eq(header.keyframe, (h.flags & 1) === 1);
    eq(header.stalled, (h.flags & 2) === 2);
    eq(bytesToHex(payload), "abcd");
  }
  for (const h of v.headers.preview) {
    eq(bytesToHex(P.packPreviewHeader({ connId: BigInt("0x" + h.connId), streamId: h.streamId, counter: h.counter })), h.hex, "preview pack");
    const { header, payload } = P.unpackPreviewHeader(hexToBytes(h.hex + "ff"));
    eq([header.streamType, header.version, header.streamId, header.counter, header.connId], [1, 1, h.streamId, h.counter, BigInt("0x" + h.connId)]);
    eq(payload.length, 1);
  }
  throws(() => P.unpackMediaHeader(new Uint8Array(18)), "short media header");
  throws(() => P.unpackPreviewHeader(new Uint8Array(14)), "short preview header");
});

// ------------------------------------------------------------------------------------------------ join links

test("join links: every Rust-classified case parses (or fails) identically", async () => {
  const { links } = await vec();
  let accepted = 0;
  let rejected = 0;
  for (const c of links.cases) {
    if (c.ok) {
      const p = parseJoinLink(c.link);
      eq(p.server, c.server, `server of ${c.link}`);
      eq(bytesToHex(p.roomId), c.roomIdHex);
      eq(bytesToHex(p.roomKey), c.roomKeyHex);
      eq(p.certHash ? bytesToHex(p.certHash) : null, c.certHashHex);
      accepted++;
    } else {
      throws(() => parseJoinLink(c.link), `reject ${JSON.stringify(c.link)} (rust: ${c.error})`, JoinLinkError);
      rejected++;
    }
  }
  ok(accepted >= 8 && rejected >= 10, `covered ${accepted} accepted / ${rejected} rejected`);
});

test("join links: format matches Rust, round-trips, hosts/ports split", async () => {
  const { links } = await vec();
  const room = hexToBytes(links.roomIdHex);
  const key = hexToBytes(links.keyHex);
  const hash = hexToBytes(links.certHashHex);
  eq(formatJoinLink("relay.example.com:4433", room, key), links.good3);
  eq(formatJoinLink("127.0.0.1:4433", room, key, hash), links.good4);
  const p = parseJoinLink(links.good4);
  eq([p.host, p.port], ["127.0.0.1", 4433]);
  eq(p.certHash.length, 32);
  eq(parseJoinLink(links.good3).certHash, null);
  eq(splitHostPort("[::1]:4433"), { host: "::1", port: 4433 });
  eq(splitHostPort("example.org"), { host: "example.org", port: null });
  eq(base64urlDecode("AAAA").length, 3);
  throws(() => base64urlDecode("AAA="), "padding rejected");
  throws(() => base64urlDecode("AB"), "non-canonical trailing bits rejected");
});

test("createRoom: fresh random id + key, valid link", () => {
  const a = createRoom("relay.example:4433", { name: " Movie Night " });
  const b = createRoom("relay.example:4433");
  eq(a.name, "Movie Night");
  ok(a.link !== b.link && bytesToHex(a.roomId) !== bytesToHex(b.roomId) && bytesToHex(a.roomKey) !== bytesToHex(b.roomKey), "unique");
  const p = parseJoinLink(a.link);
  eq([bytesToHex(p.roomId), bytesToHex(p.roomKey), p.server], [bytesToHex(a.roomId), bytesToHex(a.roomKey), "relay.example:4433"]);
  throws(() => createRoom("  "), "empty server", JoinLinkError);
  ok(parseJoinLink(createRoom("h:1", { certHash: new Uint8Array(32).fill(9) }).link).certHash.length === 32);
});

// ------------------------------------------------------------------------------------------------ nonce / aad / crypto

test("nonce: ConnId BE || counter BE, one shared counter, never wraps", () => {
  const n = buildNonce(0x0102030405060708n, 0x0a0b0c0d);
  eq(bytesToHex(n), "01020304050607080a0b0c0d");
  const seq = new NonceSequence(0xa1b2c3d4e5f60718n);
  eq(seq.take().counter, 0);
  eq(seq.take().counter, 1);
  eq(bytesToHex(seq.take().nonce), "a1b2c3d4e5f6071800000002");
  const end = new NonceSequence(1n, 0xfffffffe);
  eq(end.take().counter, 0xfffffffe);
  ok(!end.exhausted);
  const last = end.take();
  eq(last.counter, 0xffffffff, "u32::MAX is the last legal counter");
  eq(bytesToHex(last.nonce), "0000000000000001ffffffff");
  ok(end.exhausted);
  throws(() => end.take(), "counter must not wrap", NonceSpaceExhausted);
  throws(() => end.take(), "still exhausted", NonceSpaceExhausted);
});

test("aad: [version, stream_id]", () => {
  eq(bytesToHex(buildAad(0)), "0100");
  eq(bytesToHex(buildAad(255)), "01ff");
});

test("crypto: JS reproduces Rust ciphertext byte-exact and decrypts it (all frame cases)", async () => {
  const { crypto: c } = await vec();
  const key = await importRoomKey(hexToBytes(c.keyHex));
  const connId = BigInt("0x" + c.connIdHex);
  for (const f of c.frames) {
    const nonce = buildNonce(connId, f.counter);
    const ct = await encryptFrame(key, f.streamId, nonce, f.ptsUs, hexToBytes(f.payloadHex));
    eq(bytesToHex(ct), f.ciphertextHex, `encrypt ${f.name}`);
    const back = await decryptFrame(key, connId, f.counter, f.streamId, hexToBytes(f.ciphertextHex));
    eq(back.ptsUs, f.ptsUs, `pts ${f.name}`);
    eq(bytesToHex(back.payload), f.payloadHex, `payload ${f.name}`);
  }
});

test("crypto: tag failures (tamper, wrong stream id in AAD, wrong counter, wrong conn, wrong key)", async () => {
  const { crypto: c } = await vec();
  const key = await importRoomKey(hexToBytes(c.keyHex));
  const connId = BigInt("0x" + c.connIdHex);
  const f = c.frames[2];
  const ct = hexToBytes(f.ciphertextHex);
  await decryptFrame(key, connId, f.counter, f.streamId, ct); // control: passes
  const flipped = ct.slice();
  flipped[10] ^= 1;
  await throwsAsync(() => decryptFrame(key, connId, f.counter, f.streamId, flipped), "tampered ciphertext");
  await throwsAsync(() => decryptFrame(key, connId, f.counter, f.streamId + 1, ct), "relabelled stream (AAD)");
  await throwsAsync(() => decryptFrame(key, connId, f.counter + 1, f.streamId, ct), "wrong counter");
  await throwsAsync(() => decryptFrame(key, connId + 1n, f.counter, f.streamId, ct), "wrong conn id");
  const other = await importRoomKey(new Uint8Array(32).fill(1));
  await throwsAsync(() => decryptFrame(other, connId, f.counter, f.streamId, ct), "wrong key");
  await throwsAsync(() => decryptFrame(key, connId, f.counter, f.streamId, ct.subarray(0, 10)), "truncated");
});

test("crypto: previews match Rust (encrypt, header, decrypt)", async () => {
  const { crypto: c } = await vec();
  const key = await importRoomKey(hexToBytes(c.keyHex));
  const connId = BigInt("0x" + c.connIdHex);
  for (const p of c.previews) {
    const ct = await encryptPreview(key, p.streamId, buildNonce(connId, p.counter), hexToBytes(p.jpegHex));
    eq(bytesToHex(ct), p.ciphertextHex);
    const head = P.packPreviewHeader({ connId, streamId: p.streamId, counter: p.counter });
    eq(bytesToHex(head) + bytesToHex(ct), p.streamHex);
    const { header, payload } = P.unpackPreviewHeader(hexToBytes(p.streamHex));
    const jpeg = await decryptPreview(key, header.connId, header.counter, header.streamId, payload);
    eq(bytesToHex(jpeg), p.jpegHex);
  }
});

// ------------------------------------------------------------------------------------------------ fragmentation

test("fragment: JS fragments exactly like Rust (headers, split points, flags)", async () => {
  const { crypto: c } = await vec();
  const connId = BigInt("0x" + c.connIdHex);
  for (const f of c.frames) {
    const dgs = fragmentFrame({
      connId, streamId: f.streamId, frameId: f.counter, ciphertext: hexToBytes(f.ciphertextHex),
      flags: (f.keyframe ? 1 : 0) | (f.stalled ? 2 : 0), maxDatagramSize: f.maxDatagramSize ?? 1024,
    });
    eq(dgs.map(bytesToHex), f.datagramsHex, `fragments of ${f.name}`);
    ok(dgs.every((d) => d.length <= Math.min(1000, f.maxDatagramSize ?? 1000)), "datagram size cap");
  }
});

test("fragment: never exceeds 1000 bytes even if the transport allows more; rejects absurd sizes", () => {
  const ct = new Uint8Array(10_000);
  for (const max of [1024, 1200, 65535, 1000]) {
    const d = fragmentFrame({ connId: 1n, streamId: 0, frameId: 0, ciphertext: ct, flags: 0, maxDatagramSize: max });
    ok(d.every((x) => x.length <= 1000), `max=${max}`);
    eq(d.length, Math.ceil(10_000 / 981));
  }
  throws(() => fragmentFrame({ connId: 1n, streamId: 0, frameId: 0, ciphertext: ct, flags: 0, maxDatagramSize: 19 }), "no payload room");
  eq(fragmentFrame({ connId: 1n, streamId: 0, frameId: 0, ciphertext: new Uint8Array(0), flags: 0 }).length, 1, "empty frame is still one datagram");
});

const parseDg = (hex) => P.unpackMediaHeader(hexToBytes(hex));

test("reassembly: Rust datagrams in any order give back the ciphertext; AEAD then decrypts", async () => {
  const { crypto: c } = await vec();
  const key = await importRoomKey(hexToBytes(c.keyHex));
  const connId = BigInt("0x" + c.connIdHex);
  for (const f of c.frames) {
    const order = f.datagramsHex.map((_, i) => i);
    for (let i = order.length - 1; i > 0; i--) {
      const j = (i * 7 + 3) % (i + 1);
      [order[i], order[j]] = [order[j], order[i]];
    }
    const r = new Reassembler();
    let done = null;
    for (const i of order) {
      const { header, payload } = parseDg(f.datagramsHex[i]);
      const out = r.insert(header, payload);
      if (out) {
        ok(done === null, "completed twice");
        done = out;
      }
    }
    ok(done, `${f.name} did not complete`);
    eq(bytesToHex(done.ciphertext), f.ciphertextHex);
    eq(done.keyframe, f.keyframe);
    eq(done.stalled, f.stalled);
    const dec = await decryptFrame(key, connId, f.counter, f.streamId, done.ciphertext);
    eq(bytesToHex(dec.payload), f.payloadHex);
  }
});

function makeFrame(frameId, size, { streamId = 0, connId = 1n, keyframe = false } = {}) {
  return fragmentFrame({ connId, streamId, frameId, ciphertext: new Uint8Array(size).fill(frameId & 0xff), flags: keyframe ? 1 : 0, maxDatagramSize: 1000 });
}
const feed = (r, dgs) => {
  let out = null;
  for (const d of dgs) {
    const { header, payload } = P.unpackMediaHeader(d);
    out = r.insert(header, payload) || out;
  }
  return out;
};

test("reassembly: a frame missing any fragment is dropped, reported once, never delivered", () => {
  let t = 0;
  const drops = [];
  const r = new Reassembler({ now: () => t, onDrop: (c, s, f) => drops.push([c, s, f]) });
  const f1 = makeFrame(1, 3000);
  const f2 = makeFrame(2, 3000);
  const f3 = makeFrame(3, 3000);
  feed(r, [f1[0], f1[2]]); // fragment 1 lost
  ok(feed(r, f2), "newer complete frame still delivered");
  eq(drops, [[1n, 0, 1]], "frame 1 reported dropped when frame 2 completed");
  ok(feed(r, [f1[1]]) === null, "straggler of an abandoned frame is ignored");
  eq(drops.length, 1, "no double report for stragglers");
  ok(feed(r, f3), "stream recovers");
  eq(r.droppedFrames, 1);
});

test("reassembly: in-flight cap, stale timeout, duplicates, bad headers", () => {
  let t = 0;
  const drops = [];
  const r = new Reassembler({ now: () => t, onDrop: (c, s, f) => drops.push(f) });
  // three incomplete frames: the oldest is evicted (cap = 2)
  for (const id of [10, 11, 12]) feed(r, [makeFrame(id, 3000)[0]]);
  eq(drops, [10], "oldest evicted beyond the in-flight cap");
  // stale timeout
  t += 250;
  feed(r, [makeFrame(20, 3000)[0]]);
  eq(drops.sort(), [10, 11, 12], "buffers untouched for 200 ms are dropped");
  // duplicate fragments don't double-count
  const d = makeFrame(30, 1500);
  eq(feed(r, [d[0], d[0], d[0]]), null);
  ok(feed(r, [d[1]]), "completes after the real second fragment");
  // malformed: index >= count, count 0
  const bad = makeFrame(40, 3000)[0].slice();
  new DataView(bad.buffer).setUint16(15, 99);
  eq(feed(r, [bad]), null);
  const zero = makeFrame(41, 100)[0].slice();
  new DataView(zero.buffer).setUint16(17, 0);
  eq(feed(r, [zero]), null);
  // streams are independent
  const a = makeFrame(1, 1500, { streamId: 1 });
  const b = makeFrame(1, 1500, { streamId: 2 });
  ok(feed(r, [a[0], b[0]]) === null);
  ok(feed(r, [b[1]]), "stream 2 completes on its own fragments");
  ok(feed(r, [a[1]]), "stream 1 completes on its own fragments");
});

test("reassembly: keyframe flag survives when only a later fragment carries it", () => {
  const r = new Reassembler();
  const dgs = makeFrame(5, 2500, { keyframe: true });
  const out = feed(r, dgs);
  eq(out.keyframe, true);
  eq(feed(r, makeFrame(6, 100)).keyframe, false);
});

// ------------------------------------------------------------------------------------------------ codec

test("codec: sniffing and codec strings match the Rust implementations on real encoder output", async () => {
  const { codec } = await vec();
  const names = { "H.264": "h264", "H.265": "h265", AV1: "av1" };
  let positives = 0;
  for (const c of codec) {
    const au = hexToBytes(c.auHex);
    eq(sniffCodec(au), c.sniff ? names[c.sniff] : null, `sniff ${c.name}`);
    const d = deriveCodecString(au);
    eq(d ? d.string : null, c.codecString, `codec string ${c.name}`);
    if (c.codecString) positives++;
  }
  ok(positives >= 7, `positives=${positives}`);
});

test("codec: hand-checked strings", async () => {
  const { codec } = await vec();
  const by = Object.fromEntries(codec.map((c) => [c.name, deriveCodecString(hexToBytes(c.auHex))?.string]));
  eq(by["vah264enc"], "avc1.64000d"); // SPS 27 64 00 0d: High, constraint 0, level 13
  eq(by["vah265enc"], "hev1.1.6.L60.90");
  eq(by["vaav1enc"], "av01.0.00M.08");
});

test("codec: 3-byte start codes, SPS split into a later NAL, trailing zero bytes", async () => {
  const { codec } = await vec();
  const au = hexToBytes(codec.find((c) => c.name === "vah264enc").auHex);
  eq(sniffCodec(au.subarray(1)), "h264");
  eq(deriveCodecString(au.subarray(1)).string, "avc1.64000d");
  const withAud = new Uint8Array([0, 0, 0, 1, 0x09, 0x10, ...au]);
  eq(deriveCodecString(withAud).string, "avc1.64000d");
});

// ------------------------------------------------------------------------------------------------ jitter buffer

test("jitter: clamps match the native settings", () => {
  eq([clampMaxJitterBufferMs(-5), clampMaxJitterBufferMs(9999), clampMaxJitterBufferMs(2000), clampMaxJitterBufferMs("x")], [0, 5000, 2000, 2000]);
  eq([clampCatchUpSpeedPercent(0), clampCatchUpSpeedPercent(99), clampCatchUpSpeedPercent(7), clampCatchUpSpeedPercent(NaN)], [5, 10, 7, 5]);
});

test("jitter: steady 120 fps stream is delayed by ~the 40 ms floor and drains in order", () => {
  const jb = new JitterBuffer({ maxJitterBufferMs: 2000 });
  let now = 1000;
  const period = 1e6 / 120; // µs
  for (let i = 0; i < 600; i++) {
    jb.push(i, { ptsUs: i * period, keyframe: i % 120 === 0 }, now);
    now += period / 1000;
  }
  ok(Math.abs(jb.appliedBufferMs - 40) < 1, `applied buffer ${jb.appliedBufferMs}`);
  const out = [];
  while (jb.size) out.push(...jb.drain(now + 1000, 0).items); // the decoder takes up to MAX_DECODE_QUEUE per call
  eq(out, Array.from({ length: 600 }, (_, i) => i), "FIFO order");
});

test("jitter: a latency spike grows the buffer (<= 20 ms per chunk) and it shrinks back no faster than the catch-up cap", () => {
  const jb = new JitterBuffer({ maxJitterBufferMs: 2000, catchUpSpeedPercent: 5 });
  let now = 0;
  let pts = 0;
  const step = 1e6 / 60;
  const push = (arrivalGapMs) => {
    now += arrivalGapMs;
    pts += step;
    jb.push(0, { ptsUs: pts, keyframe: false }, now);
  };
  for (let i = 0; i < 120; i++) push(step / 1000);
  const base = jb.appliedBufferMs;
  push(300); // 300 ms hiccup
  let prev = jb.appliedBufferMs;
  let peak = prev;
  for (let i = 0; i < 200; i++) {
    push(step / 1000);
    ok(jb.appliedBufferMs - prev <= 20 + 1e-9, "growth capped per chunk");
    prev = jb.appliedBufferMs;
    peak = Math.max(peak, prev);
  }
  for (let i = 0; i < 60; i++) push(200); // sustained bad network: it keeps growing, up to the ceiling only
  ok(jb.appliedBufferMs <= 2000, "ceiling");
  ok(peak > base + 50, `buffer should have grown: ${base} -> ${peak}`);
  // shrink: wall time elapsed vs buffer shed must respect 5 %
  const t0 = now;
  const b0 = jb.appliedBufferMs;
  ok(b0 > 100, `buffer is large before the recovery phase: ${b0}`);
  for (let i = 0; i < 600; i++) push(step / 1000);
  const shed = b0 - jb.appliedBufferMs;
  const elapsed = now - t0;
  ok(shed <= elapsed * 0.05 + 1e-6, `shed ${shed.toFixed(1)} ms in ${elapsed.toFixed(0)} ms exceeds 5 %`);
  ok(shed > 0, "buffer does shrink");
});

test("jitter: max=0 releases immediately; maxJitterBufferMs caps the delay", () => {
  const jb0 = new JitterBuffer({ maxJitterBufferMs: 0 });
  jb0.push("a", { ptsUs: 0, keyframe: true }, 100);
  eq(jb0.nextDelay(100), 0);
  eq(jb0.drain(100).items, ["a"]);
  const jb = new JitterBuffer({ maxJitterBufferMs: 100 });
  let now = 0;
  for (let i = 0; i < 100; i++) {
    now += i % 2 ? 90 : 1; // wildly jittery
    jb.push(i, { ptsUs: i * 16666, keyframe: false }, now);
  }
  ok(jb.appliedBufferMs <= 100, "capped");
});

test("jitter: decode-queue backstop drops the delta run up to the next queued keyframe, never a lone delta", () => {
  const jb = new JitterBuffer({ maxJitterBufferMs: 0 });
  const seq = [["K0", true], ["d1", false], ["d2", false], ["d3", false], ["K4", true], ["d5", false]];
  seq.forEach(([n, key], i) => jb.push(n, { ptsUs: i * 1000, keyframe: key }, 0));
  // head is a keyframe: never skipped
  let r = jb.drain(1000, DECODE_QUEUE_CATCHUP_THRESHOLD + 2);
  eq(r.dropped, 0);
  eq(r.items, ["K0", "d1", "d2"], "head is a keyframe: nothing skipped; only the decoder's remaining budget (8 - 5) is released");
  // head is a delta with a keyframe queued behind it
  const jb2 = new JitterBuffer({ maxJitterBufferMs: 0 });
  [["d1", false], ["d2", false], ["K3", true], ["d4", false]].forEach(([n, key], i) => jb2.push(n, { ptsUs: i * 1000, keyframe: key }, 0));
  r = jb2.drain(1000, DECODE_QUEUE_CATCHUP_THRESHOLD + 1);
  eq(r.dropped, 2);
  eq(r.items, ["K3", "d4"]);
  // no keyframe queued: nothing is dropped (dropping would corrupt references)
  const jb3 = new JitterBuffer({ maxJitterBufferMs: 0 });
  [["d1", false], ["d2", false]].forEach(([n, key], i) => jb3.push(n, { ptsUs: i * 1000, keyframe: key }, 0));
  r = jb3.drain(1000, 5);
  eq(r.dropped, 0);
  eq(r.items, ["d1", "d2"]);
  // decoder keeping up: nothing dropped
  const jb4 = new JitterBuffer({ maxJitterBufferMs: 0 });
  [["d1", false], ["K2", true]].forEach(([n, key], i) => jb4.push(n, { ptsUs: i * 1000, keyframe: key }, 0));
  eq(jb4.drain(1000, 1).dropped, 0);
});

test("jitter: the decoder is never handed more than MAX_DECODE_QUEUE requests; the rest waits in the buffer", () => {
  const jb = new JitterBuffer({ maxJitterBufferMs: 0 });
  for (let i = 0; i < 20; i++) jb.push(i, { ptsUs: i * 1000, keyframe: i === 0 }, 0);
  eq(jb.drain(1000, 0).items.length, MAX_DECODE_QUEUE, "empty decoder queue: up to the cap");
  eq(jb.drain(1000, MAX_DECODE_QUEUE).items.length, 0, "full decoder queue: nothing released");
  eq(jb.drain(1000, MAX_DECODE_QUEUE - 3).items.length, 3, "released as the decoder dequeues");
  eq(jb.size, 20 - MAX_DECODE_QUEUE - 3);
});

test("video receiver: a decoder slower than the stream keeps bounded latency (drops to keyframes) instead of an unbounded backlog", async () => {
  const { codec } = await vec();
  const keyAu = hexToBytes(codec.find((c) => c.name === "vah264enc").auHex);
  const delta = new Uint8Array([0, 0, 0, 1, 0x41, 1, 2, 3, 4, 5, 6, 7, 8]);
  const decoded = [];
  let maxQueue = 0;
  class SlowDecoder {
    static async isConfigSupported() {
      return { supported: true };
    }
    constructor({ output }) {
      this.output = output;
      this.state = "unconfigured";
      this.decodeQueueSize = 0;
      this.q = [];
      this.timer = setInterval(() => {
        const c = this.q.shift(); // ~100 fps decoder
        if (!c) return;
        this.decodeQueueSize--;
        decoded.push(c.timestamp);
        this.output({ displayWidth: 1, displayHeight: 1, close() {} });
        if (this.ondequeue) this.ondequeue();
      }, 10);
    }
    configure() {
      this.state = "configured";
    }
    decode(chunk) {
      this.q.push(chunk);
      this.decodeQueueSize++;
      maxQueue = Math.max(maxQueue, this.decodeQueueSize);
    }
    close() {
      this.state = "closed";
      clearInterval(this.timer);
    }
  }
  const rx = new VideoReceiver({ onFrame: (f) => f.close(), DecoderClass: SlowDecoder, maxJitterBufferMs: 0 });
  // 250 fps for 4 s into a 100 fps decoder, a keyframe every 50 frames (0.2 s)
  const periodMs = 4;
  const t0 = performance.now();
  let n = 0;
  let lastPush = 0;
  while (performance.now() - t0 < 4000) {
    const due = Math.floor((performance.now() - t0) / periodMs);
    while (n <= due) {
      const ptsUs = n * periodMs * 1000;
      rx.pushFrame({ ptsUs, keyframe: n % 50 === 0, payload: n % 50 === 0 ? keyAu : delta });
      lastPush = ptsUs;
      n++;
    }
    await new Promise((r) => setTimeout(r, 2));
  }
  await new Promise((r) => setTimeout(r, 50));
  const lastDecoded = decoded.at(-1);
  rx.close();
  ok(maxQueue <= MAX_DECODE_QUEUE, `decoder queue peaked at ${maxQueue}, cap ${MAX_DECODE_QUEUE}`);
  ok(decoded.length > 200, `decoder kept working (${decoded.length} decoded)`);
  const lagMs = (lastPush - lastDecoded) / 1000;
  ok(lagMs < 600, `latency stays within about one GOP (0.2 s) plus the decoder cap, not growing without bound: lag ${lagMs.toFixed(0)} ms`);
  ok(rx.jb.catchUpDropped > 100, `catch-up dropped delta runs to keyframes (${rx.jb.catchUpDropped})`);
});

test("jitter: reset() clears queue and pacing anchor", () => {
  const jb = new JitterBuffer();
  jb.push("x", { ptsUs: 5_000_000, keyframe: true }, 10);
  jb.reset();
  eq(jb.size, 0);
  jb.push("y", { ptsUs: 0, keyframe: true }, 20);
  ok(jb.nextDelay(20) <= 40 + 1e-9, "re-anchored on the first chunk after reset");
});

test("presets: the four native presets are mapped", () => {
  eq(Object.keys(ENCODER_PRESETS), ["low_latency", "balanced", "quality", "gaming"]);
  eq(ENCODER_PRESETS.gaming.gopMs, 750);
  eq(ENCODER_PRESETS.balanced.gopMs, 2000);
});

// ------------------------------------------------------------------------------------------------ reverse vectors (JS -> Rust)

test("reverse vectors: JS-made frames/previews/control messages/links for Rust to verify", async () => {
  const keyBytes = randomBytes(32);
  const key = await importRoomKey(keyBytes);
  const connId = 0x1234_5678_9abc_def0n;
  const seq = new NonceSequence(connId, 1000);
  const reverse = { frames: [], previews: [], control: [], links: [] };
  const sizes = [0, 1, 50, 957, 958, 1938, 4000, 70_000, 250_000];
  for (const [i, size] of sizes.entries()) {
    const streamId = (i * 37) & 0xff;
    const { nonce, counter } = seq.take();
    const payload = randomBytes(size);
    const ptsUs = 1_000_000 * (i + 1) + 123;
    const keyframe = i % 2 === 0;
    const ct = await encryptFrame(key, streamId, nonce, ptsUs, payload);
    const dgs = fragmentFrame({ connId, streamId, frameId: counter, ciphertext: ct, flags: keyframe ? 1 : 0, maxDatagramSize: 1024 });
    // shuffle to prove order independence on the Rust side as well
    dgs.reverse();
    reverse.frames.push({
      name: `js frame ${size}`, keyHex: bytesToHex(keyBytes), connIdHex: connId.toString(16).padStart(16, "0"), streamId, counter, ptsUs, keyframe,
      payloadHex: bytesToHex(payload), datagramsHex: dgs.map(bytesToHex),
    });
  }
  for (const size of [0, 300, 20_000]) {
    const { nonce, counter } = seq.take();
    const jpeg = randomBytes(size);
    const ct = await encryptPreview(key, 3, nonce, jpeg);
    const head = P.packPreviewHeader({ connId, streamId: 3, counter });
    reverse.previews.push({ keyHex: bytesToHex(keyBytes), streamId: 3, counter, jpegHex: bytesToHex(jpeg), streamHex: bytesToHex(head) + bytesToHex(ct) });
  }
  const msgs = [
    { type: "Join", connId, roomId: randomBytes(16), displayName: "Zoë 😀" },
    { type: "Leave" },
    { type: "AnnounceStream", streamId: 0, kind: "video", label: "Browser screen", width: 2560, height: 1440 },
    { type: "AnnounceStream", streamId: 1, kind: "audio", label: "Browser audio", width: 0, height: 0 },
    { type: "StopStream", streamId: 1 },
    { type: "Subscribe", target: 0xfedcba9876543210n, streamId: 200 },
    { type: "Unsubscribe", target: 1n, streamId: 0 },
  ];
  for (const m of msgs) reverse.control.push({ name: m.type, hex: bytesToHex(P.encodeClientToRelay(m)) });
  const room = createRoom("127.0.0.1:4433", { certHash: randomBytes(32) });
  reverse.links.push({ link: room.link, roomIdHex: bytesToHex(room.roomId), roomKeyHex: bytesToHex(room.roomKey) });
  window.__reverse = reverse;
  ok(reverse.frames.length === sizes.length);
});

// ------------------------------------------------------------------------------------------------ runner

export async function runAll({ onResult = () => {} } = {}) {
  const results = [];
  for (const t of tests) {
    const t0 = performance.now();
    let r;
    try {
      await t.fn();
      r = { name: t.name, ok: true, ms: Math.round(performance.now() - t0) };
    } catch (e) {
      r = { name: t.name, ok: false, error: String(e && e.message ? e.message : e), stack: e && e.stack };
    }
    results.push(r);
    onResult(r);
  }
  return results;
}
