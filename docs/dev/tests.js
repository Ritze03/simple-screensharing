// Unit tests for docs/js/core, run in the browser by dev/tests.html (headless: `node dev/headless.mjs run dev/tests.html`).
// Cross-language vectors (dev/vectors.json) are generated from the REAL Rust crates (protocol/ + client lib) by a
// throwaway generator; see dev/README.md. Reverse vectors (JS-made, to be decrypted/parsed by Rust) are returned in
// the page result under `reverse`.

import { PostcardReader, PostcardWriter } from "../js/core/postcard.js";
import * as P from "../js/core/protocol.js";
import { MAX_LINK_ROOM_NAME_CHARS, base64urlDecode, bytesToHex, createRoom, encodeLinkName, formatJoinLink, hexToBytes, JoinLinkError, parseJoinLink, parseServerAddr, splitHostPort, stripLinkName } from "../js/core/joinlink.js";
import { NonceSequence, NonceSpaceExhausted, buildAad, buildNonce, decryptFrame, decryptPreview, encryptFrame, encryptPreview, importRoomKey } from "../js/core/crypto.js";
import { NACK_HARD_LIMIT_MS, Reassembler, fragmentFrame, nackRetryInterval, pacePlan } from "../js/core/fragment.js";
import { SEND_RING_BYTES, SendRing, nackEnabledByDefault } from "../js/core/nack.js";
import { deriveCodecString, sniffCodec } from "../js/core/codec.js";
import { DECODE_QUEUE_CATCHUP_THRESHOLD, JitterBuffer, MAX_DECODE_QUEUE, clampCatchUpSpeedPercent, clampMaxJitterBufferMs } from "../js/core/jitter.js";
import { CanvasRenderer, VideoReceiver } from "../js/core/video-rx.js";
import { ENCODER_PRESETS } from "../js/core/video-tx.js";
import { avatarBg, avatarColors, avatarHue, pickEmoji, sliderCtl, snapVolumePct, wheelStepVolumePct, volumeWheel, VOLUME_MAX_PCT } from "../js/ui/dom.js";
import * as G from "../js/ui/stage-geom.js";
import { CHROME_IDLE_MS, chromeRemaining, chromeStart, chromeStep } from "../js/ui/chrome.js";
import { loadSettings } from "../js/ui/store.js";
import { Gate, MIN_GAP_MS, RESUME_QUIET_MS, RoomSounds, SOUNDS, SOUND_NAMES, soundPlan } from "../js/ui/sounds.js";
import { DICT } from "../js/ui/i18n.js";
import { AudioReceiver, MAX_VOLUME } from "../js/core/audio.js";
import { CEILING_DB, GATE_DB, Leveller, MAX_GAIN_DB, RISE_DB_PER_S, TARGET_DB } from "../js/core/leveller.js";

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
      eq(p.name, c.name ?? null, `name of ${c.link}`);
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

// Task #69: the 5th field (room name). Vectors mirror the native unit tests in client/src/net.rs
// (join_string_name_round_trip_all_forms / _special_characters / join_string_bad_name_is_dropped_not_rejected); the encoded
// strings were computed independently with Python `quote(name, safe="-._~")`, which is what the native encoder writes.
const NAME_ID = new Uint8Array(16).fill(3);
const NAME_KEY = new Uint8Array(32).fill(7);
const NAME_HASH = new Uint8Array(32).fill(9);
const NAME_BASE = `h:1|${bytesToHex(NAME_ID)}|${"BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc"}`;

test("join link names: formatted exactly like native, in all forms", () => {
  ok(NAME_BASE === formatJoinLink("h:1", NAME_ID, NAME_KEY), "base link");
  const hashS = NAME_BASE.length && formatJoinLink("h:1", NAME_ID, NAME_KEY, NAME_HASH).split("|")[3];
  eq(formatJoinLink("h:1", NAME_ID, NAME_KEY, NAME_HASH, "Friday Films"), `${NAME_BASE}|${hashS}|Friday%20Films`);
  eq(formatJoinLink("h:1", NAME_ID, NAME_KEY, null, "Movies"), `${NAME_BASE}||Movies`, "no hash: empty 4th field");
  eq(formatJoinLink("h:1", NAME_ID, NAME_KEY, null, "  "), NAME_BASE, "blank name = 3-field form");
  eq(formatJoinLink("h:1", NAME_ID, NAME_KEY, NAME_HASH, null), formatJoinLink("h:1", NAME_ID, NAME_KEY, NAME_HASH));
  const withName = parseJoinLink(`${NAME_BASE}|${hashS}|Friday%20Films`);
  eq([withName.name, withName.certHash.length], ["Friday Films", 32]);
  eq(parseJoinLink(`${NAME_BASE}||Movies`).certHash, null);
  eq(parseJoinLink(`${NAME_BASE}||Movies`).name, "Movies");
  for (const l of [NAME_BASE, `${NAME_BASE}|${hashS}`, `${NAME_BASE}||`, `${NAME_BASE}|${hashS}|`]) eq(parseJoinLink(l).name, null, `no name in ${l}`);
});

test("join link names: special characters round-trip and match native bytes", () => {
  const cases = [
    ["a|b", "a%7Cb"], ["100%", "100%25"], ["%41", "%2541"], ["with space", "with%20space"],
    ["Größe Ünï", "Gr%C3%B6%C3%9Fe%20%C3%9Cn%C3%AF"], ["Movie \u{1F3AC} night", "Movie%20%F0%9F%8E%AC%20night"],
    ["~-._ok", "~-._ok"], ["a|b|c|d", "a%7Cb%7Cc%7Cd"], ["Hi! (x)*'", "Hi%21%20%28x%29%2A%27"],
  ];
  for (const [name, enc] of cases) {
    const l = formatJoinLink("h:1", NAME_ID, NAME_KEY, null, name);
    eq(l, `${NAME_BASE}||${enc}`, `encoding of ${JSON.stringify(name)}`);
    eq(l.split("|").length, 5, "name must not add separators");
    ok(/^[\x20-\x7e]*$/.test(l), `ascii: ${l}`);
    const p = parseJoinLink(l);
    eq(p.name, name, `round trip ${l}`);
    eq(bytesToHex(p.roomId), bytesToHex(NAME_ID));
  }
  // encodeURIComponent output (leaves !'()* raw) decodes too
  eq(parseJoinLink(`${NAME_BASE}||Hi!%20(x)`).name, "Hi! (x)");
  // a raw `|` inside the 5th field stays part of the name (splitn(5))
  eq(parseJoinLink(`${NAME_BASE}||a|b`).name, "a|b");
});

test("join link names: a bad name is dropped, never rejects the link", () => {
  for (const bad of ["100%", "%4", "%zz", "%FF%FE", "%0A", "   ", "%20%20"]) {
    const p = parseJoinLink(`${NAME_BASE}||${bad}`);
    eq(p.name, null, `name of ${bad}`);
    eq(bytesToHex(p.roomKey), bytesToHex(NAME_KEY));
  }
  eq(parseJoinLink(`${NAME_BASE}||%20%20Hi%0A%20`).name, "Hi", "trimmed, control chars dropped");
  eq(parseJoinLink(`${NAME_BASE}||${"x".repeat(200)}`).name.length, MAX_LINK_ROOM_NAME_CHARS, "capped");
  eq(Array.from(parseJoinLink(`${NAME_BASE}||${"%F0%9F%8E%AC".repeat(100)}`).name).length, MAX_LINK_ROOM_NAME_CHARS, "capped in characters, not UTF-16 units");
  // a bad cert hash still rejects, with or without a name
  throws(() => parseJoinLink(`${NAME_BASE}|AAAA|Name`), "bad hash + name", JoinLinkError);
  eq(encodeLinkName("\ud800"), "", "lone surrogate: name left off, no throw");
});

test("join link names: stripLinkName keeps the stored form and createRoom carries the name", () => {
  const hashS = formatJoinLink("h:1", NAME_ID, NAME_KEY, NAME_HASH).split("|")[3];
  eq(stripLinkName(`${NAME_BASE}|${hashS}|Friday%20Films`), `${NAME_BASE}|${hashS}`);
  eq(stripLinkName(`${NAME_BASE}||Movies`), NAME_BASE);
  eq(stripLinkName(`  ${NAME_BASE}|${hashS}  `), `${NAME_BASE}|${hashS}`);
  eq(stripLinkName(NAME_BASE), NAME_BASE);
  const r = createRoom("h:1", { name: "Movie Night", certHash: NAME_HASH });
  eq(parseJoinLink(r.link).name, "Movie Night");
  eq(stripLinkName(r.link), formatJoinLink("h:1", r.roomId, r.roomKey, NAME_HASH));
});

test("createRoom: fresh random id + key, valid link", () => {
  const a = createRoom("relay.example:4433", { name: " Movie Night " });
  const b = createRoom("relay.example:4433");
  eq(a.name, "Movie Night");
  ok(a.link !== b.link && bytesToHex(a.roomId) !== bytesToHex(b.roomId) && bytesToHex(a.roomKey) !== bytesToHex(b.roomKey), "unique");
  const p = parseJoinLink(a.link);
  eq([bytesToHex(p.roomId), bytesToHex(p.roomKey), p.server], [bytesToHex(a.roomId), bytesToHex(a.roomKey), "relay.example:4433"]);
  throws(() => createRoom("  "), "empty server", JoinLinkError);
  throws(() => createRoom("relay.example"), "server without a port", JoinLinkError);
  ok(parseJoinLink(createRoom("h:1", { certHash: new Uint8Array(32).fill(9) }).link).certHash.length === 32);
});

test("parseServerAddr: host:port, ip:port, [v6]:port; a port is required (mirrors native parse_server_addr)", () => {
  eq(parseServerAddr("vogelnest.any64.de:41000"), { host: "vogelnest.any64.de", port: 41000 });
  eq(parseServerAddr("  host:1  "), { host: "host", port: 1 });
  eq(parseServerAddr("host:65535"), { host: "host", port: 65535 });
  eq(parseServerAddr("1.2.3.4:4433"), { host: "1.2.3.4", port: 4433 });
  eq(parseServerAddr("[::1]:4433"), { host: "::1", port: 4433 });
  const code = (s) => throws(() => parseServerAddr(s), `reject ${JSON.stringify(s)}`, JoinLinkError).code;
  eq(code(""), "empty");
  for (const s of ["host", "1.2.3.4", "::1", "[::1]"]) eq(code(s), "no-port");
  for (const s of ["host:0", "host:70000", "host:", "host:abc", ":4433", "[::1]x", "[::1"]) eq(code(s), "bad-port");
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

// Same cases as the native `pace_plan` tests in client/src/capture.rs (#102): both sharers must pace alike.
const planWindow = (count, p) => p.gapMs * (Math.ceil(count / p.perChunk) - 1);

test("pacing: small frames and ordinary deltas go out at once; a small keyframe is spread over at most ~60 ms", () => {
  eq(pacePlan(10, 12_000, true, 60, 8300), null);
  eq(pacePlan(30, 36_000, false, 120, 8300), null);
  eq(pacePlan(240, 288_000, true, 60, 100_000), { perChunk: 8, gapMs: 2 }, "240 datagrams at 60 fps: 30 chunks of 8 every 2 ms");
  ok(planWindow(240, pacePlan(240, 288_000, true, 240, 100_000)) <= 17, "window shrinks with the frame period");
  for (const count of [24, 25, 100, 301]) {
    const p = pacePlan(count, count * 1200, true, 30, 100_000);
    ok(p.perChunk * Math.ceil(count / p.perChunk) >= count, `every datagram of ${count} is covered`);
  }
});

test("pacing: a big keyframe is spread by rate up to the 600 ms cap; a huge delta frame is paced too", () => {
  // the #102 field case: 984 datagrams (1.18 MB) at 8300 kbps / 120 fps
  const w = planWindow(984, pacePlan(984, 1_180_000, true, 120, 8300));
  ok(w >= 350 && w <= 500, `1.18 MB IDR window ${w} ms`);
  ok(planWindow(5000, pacePlan(5000, 6_000_000, true, 120, 1000)) <= 600, "capped");
  ok(pacePlan(1600, 1_900_000, false, 120, 8300) !== null, "1.9 MB delta is paced");
  eq(pacePlan(34, 40_000, false, 120, 8300), null, "40 kB delta is not");
  ok(planWindow(100, pacePlan(100, 100_000, true, 60, 8300)) >= 45, "short rate window keeps the 60 ms one");
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

test("video receiver: on the first decoder overload a Gecko-style receiver re-configures the decoder without optimizeForLatency (once, at a keyframe)", async () => {
  const { codec } = await vec();
  const keyAu = hexToBytes(codec.find((c) => c.name === "vah264enc").auHex);
  const delta = new Uint8Array([0, 0, 0, 1, 0x41, 1, 2, 3, 4, 5, 6, 7, 8]);
  const configs = []; // { optimizeForLatency, nextIsKey }
  const make = () =>
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
          this.output({ displayWidth: 1, displayHeight: 1, close() {} });
          if (this.ondequeue) this.ondequeue();
        }, 10);
      }
      configure(cfg) {
        this.state = "configured";
        configs.push({ optimizeForLatency: cfg.optimizeForLatency });
        this.sinceConfigure = 0;
      }
      decode(chunk) {
        if (this.sinceConfigure === 0 && chunk.type !== "key" && configs.length > 1) fail("first chunk after a re-configure was not a keyframe");
        this.sinceConfigure++;
        this.q.push(chunk);
        this.decodeQueueSize++;
      }
      close() {
        this.state = "closed";
        clearInterval(this.timer);
      }
    };
  const feed = async (rx) => {
    const periodMs = 4;
    const t0 = performance.now();
    let n = 0;
    while (performance.now() - t0 < 2000) {
      const due = Math.floor((performance.now() - t0) / periodMs);
      while (n <= due) {
        rx.pushFrame({ ptsUs: n * periodMs * 1000, keyframe: n % 50 === 0, payload: n % 50 === 0 ? keyAu : delta });
        n++;
      }
      await new Promise((r) => setTimeout(r, 2));
    }
    rx.close();
  };
  const events = [];
  await feed(new VideoReceiver({ onFrame: (f) => f.close(), onEvent: (t) => events.push(t), DecoderClass: make(), maxJitterBufferMs: 0, adaptiveLatency: true }));
  eq(configs.map((c) => c.optimizeForLatency), [true, false], "decoder configs: low-latency first, then once without");
  ok(events.includes("decoder-mode"), "decoder-mode event emitted");
  configs.length = 0;
  await feed(new VideoReceiver({ onFrame: (f) => f.close(), DecoderClass: make(), maxJitterBufferMs: 0, adaptiveLatency: false }));
  eq(configs.map((c) => c.optimizeForLatency), [true], "non-adaptive receivers never change the decoder config");
});

test("jitter: reset() clears queue and pacing anchor", () => {
  const jb = new JitterBuffer();
  jb.push("x", { ptsUs: 5_000_000, keyframe: true }, 10);
  jb.reset();
  eq(jb.size, 0);
  jb.push("y", { ptsUs: 0, keyframe: true }, 20);
  ok(jb.nextDelay(20) <= 40 + 1e-9, "re-anchored on the first chunk after reset");
});

test("presets: the three native presets are mapped, the retired names load as Quality", () => {
  eq(Object.keys(ENCODER_PRESETS), ["text", "quality", "gaming", "balanced", "low_latency"]);
  const row = (p) => [p.fps, p.latencyMode, p.gopMs];
  eq(row(ENCODER_PRESETS.text), [15, "quality", 250]);
  eq(row(ENCODER_PRESETS.quality), [60, "quality", 1000]);
  eq(row(ENCODER_PRESETS.gaming), [120, "realtime", 250]);
  eq(ENCODER_PRESETS.balanced, ENCODER_PRESETS.quality);
  eq(ENCODER_PRESETS.low_latency, ENCODER_PRESETS.quality);
});

// ------------------------------------------------------------------------------------------------ reverse vectors (JS -> Rust)

test("session: relay close codes map to rejections; unknown/absent codes map to nothing", async () => {
  const { CLOSE_CODES, classifyClose } = await import("../js/core/session.js");
  eq(classifyClose({ closeCode: 0x102 }), "room-full");
  eq(classifyClose({ closeCode: 0x101 }), "malformed");
  eq(classifyClose({ closeCode: 0x200 }), "relay-shutdown");
  eq(classifyClose({ closeCode: 0, reason: "bye" }), null);
  eq(classifyClose({ error: new Error("Connection lost.") }), null);
  eq(classifyClose(null), null);
  eq([CLOSE_CODES.MALFORMED, CLOSE_CODES.ROOM_FULL, CLOSE_CODES.SHUTDOWN], [0x101, 0x102, 0x200]);
});

test("media events: a codec change clears a stale can't-decode error (H.265 -> H.264); unsupported sets it again", async () => {
  const { applyMediaEvent } = await import("../js/ui/media-event.js");
  const tr = (c) => `This browser can't decode ${c}.`;
  const w = { codec: null, error: null };
  applyMediaEvent(w, { type: "codec", codec: "h265" }, tr);
  applyMediaEvent(w, { type: "unsupported", codec: "h265" }, tr);
  eq(w.error, "This browser can't decode H265.");
  applyMediaEvent(w, { type: "codec", codec: "h264" }, tr);
  eq([w.codec, w.error], ["h264", null]);
  applyMediaEvent(w, { type: "codec", codec: "h265" }, tr);
  applyMediaEvent(w, { type: "unsupported", codec: "h265" }, tr);
  eq(w.error !== null, true);
  eq(applyMediaEvent(w, { type: "need-keyframe" }, tr), false);
  eq(w.error !== null, true, "unrelated events leave the error alone");
});

test("ui i18n: en and de have the same keys and placeholders; German uses 'du', never 'Sie'", async () => {
  const { DICT } = await import("../js/ui/i18n.js");
  const ph = (s) => [...String(s).matchAll(/%\{(\w+)\}/g)].map((m) => m[1]).sort().join(",");
  const missingDe = Object.keys(DICT.en).filter((k) => !(k in DICT.de));
  const extraDe = Object.keys(DICT.de).filter((k) => !(k in DICT.en));
  eq([missingDe, extraDe], [[], []], "key parity");
  const badPh = Object.keys(DICT.en).filter((k) => ph(DICT.en[k]) !== ph(DICT.de[k]));
  eq(badPh, [], "placeholders differ");
  const formal = Object.entries(DICT.de).filter(([, v]) => /\b(Sie|Ihr|Ihre|Ihren|Ihrem|Ihrer|Ihres|Ihnen)\b/.test(v) || /\b(Beitrittslink|Freigabe)/.test(v)).map(([k]) => k);
  eq(formal, [], "formal/old-term German strings");
});

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

test("canvas renderer: WebGL and 2D backends draw the same upright picture; a cropped frame draws only its visible rect", async () => {
  if (typeof VideoFrame === "undefined") return; // no WebCodecs in this browser: nothing to render
  const src = new OffscreenCanvas(64, 48);
  const c = src.getContext("2d");
  c.fillStyle = "#ff0000"; c.fillRect(0, 0, 32, 24); //   top-left red
  c.fillStyle = "#00ff00"; c.fillRect(32, 0, 32, 24); //  top-right green
  c.fillStyle = "#0000ff"; c.fillRect(0, 24, 32, 24); //  bottom-left blue
  c.fillStyle = "#ffffff"; c.fillRect(32, 24, 32, 24); // bottom-right white
  const render = async (opts, mk) => {
    const canvas = document.createElement("canvas");
    const r = new CanvasRenderer(canvas, { preserveDrawingBuffer: true, ...opts });
    r.present(mk());
    for (let i = 0; i < 100 && !r.drawn; i++) await new Promise((res) => setTimeout(res, 20));
    ok(r.drawn === 1, `${JSON.stringify(opts)} drew ${r.drawn} frames`);
    const probe = document.createElement("canvas");
    probe.width = canvas.width;
    probe.height = canvas.height;
    const pc = probe.getContext("2d");
    pc.drawImage(canvas, 0, 0);
    const at = (x, y) => Array.from(pc.getImageData(x, y, 1, 1).data.slice(0, 3));
    const out = { backend: r.backend, w: canvas.width, h: canvas.height, at };
    r.stop();
    return out;
  };
  const near = (a, b) => a.every((v, i) => Math.abs(v - b[i]) <= 6);
  {
    // The pop-out moves the tile's canvas into another document (Document PiP / fullscreen stage): the renderer must keep drawing there.
    const iframe = document.body.appendChild(document.createElement("iframe"));
    const canvas = document.createElement("canvas");
    const r = new CanvasRenderer(canvas, { preserveDrawingBuffer: true, webgl: "force" });
    r.present(new VideoFrame(src, { timestamp: 0 }));
    for (let i = 0; i < 100 && !r.drawn; i++) await new Promise((res) => setTimeout(res, 20));
    iframe.contentDocument.body.appendChild(iframe.contentDocument.adoptNode(canvas));
    r.present(new VideoFrame(src, { timestamp: 1 }));
    for (let i = 0; i < 100 && r.drawn < 2; i++) await new Promise((res) => setTimeout(res, 20));
    const probe = document.createElement("canvas").getContext("2d");
    probe.canvas.width = 64;
    probe.canvas.height = 48;
    probe.drawImage(canvas, 0, 0);
    ok(r.drawn === 2 && near(Array.from(probe.getImageData(8, 6, 1, 1).data.slice(0, 3)), [255, 0, 0]), `${r.backend}: drawing continues after the canvas moved to another document (drawn ${r.drawn})`);
    r.stop();
    iframe.remove();
  }
  for (const opts of [{ webgl: "force" }, { webgl: false }]) {
    const o = await render(opts, () => new VideoFrame(src, { timestamp: 0 }));
    if (opts.webgl === "force" && document.createElement("canvas").getContext("webgl2")) eq(o.backend, "webgl", "forced WebGL backend");
    eq([o.w, o.h], [64, 48], `${o.backend} size`);
    ok(near(o.at(8, 6), [255, 0, 0]), `${o.backend}: top-left should be red, got ${o.at(8, 6)}`);
    ok(near(o.at(56, 6), [0, 255, 0]), `${o.backend}: top-right should be green, got ${o.at(56, 6)}`);
    ok(near(o.at(8, 42), [0, 0, 255]), `${o.backend}: bottom-left should be blue, got ${o.at(8, 42)}`);
    ok(near(o.at(56, 42), [255, 255, 255]), `${o.backend}: bottom-right should be white, got ${o.at(56, 42)}`);
    const cropped = await render(opts, () => new VideoFrame(new VideoFrame(src, { timestamp: 0 }), { visibleRect: { x: 32, y: 0, width: 32, height: 48 } }));
    eq([cropped.w, cropped.h], [32, 48], `${cropped.backend} cropped size`);
    ok(near(cropped.at(8, 6), [0, 255, 0]), `${cropped.backend}: cropped top-left should be green, got ${cropped.at(8, 6)}`);
    ok(near(cropped.at(8, 42), [255, 255, 255]), `${cropped.backend}: cropped bottom-left should be white, got ${cropped.at(8, 42)}`);
  }
});

// ------------------------------------------------------------------------------------------------ avatar colour

// Same table as client/src/theme.rs `avatar_colour_vectors_match_the_browser` (cross-client contract, tasks #77, #83).
test("avatar colour: gradient vectors match the native client; trimmed and case-insensitive", () => {
  const v = [
    ["Ritze", 177, "#25938d", "#186252"], ["mo", 245, "#2e2593", "#181f62"], ["client-1", 144, "#259351", "#18622a"],
    ["client-2", 321, "#93256c", "#621854"], ["Zed", 4, "#932c25", "#621820"], ["user-d740", 328, "#93255f", "#62184c"],
    ["Größe", 314, "#932579", "#62185d"], ["", 61, "#919325", "#625718"], ["  Mo ", 245, "#2e2593", "#181f62"],
  ];
  for (const [name, hue, a, b] of v) {
    eq(avatarHue(name), hue, `hue of ${JSON.stringify(name)}`);
    eq(avatarColors(name), [a, b], `gradient of ${JSON.stringify(name)}`);
  }
  eq(avatarColors("RITZE"), avatarColors("ritze"), "case");
  eq(avatarBg("Ritze"), "background:linear-gradient(140deg, #25938d, #186252)", "inline style");
});

test("avatar colour: white initials keep >= 3:1 contrast against both stops on every hue", () => {
  const lin = (c) => (c / 255 <= 0.04045 ? c / 255 / 12.92 : ((c / 255 + 0.055) / 1.055) ** 2.4);
  const seen = new Set();
  for (let i = 0; seen.size < 360 && i < 100000; i++) {
    const name = "n" + i;
    const hue = avatarHue(name);
    if (seen.has(hue)) continue;
    seen.add(hue);
    const lum = [];
    for (const hex of avatarColors(name)) {
      const [r, g, b] = [1, 3, 5].map((k) => parseInt(hex.slice(k, k + 2), 16));
      const y = 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
      lum.push(y);
      ok(1.05 / (y + 0.05) >= 3, `hue ${hue} ${hex}: contrast ${(1.05 / (y + 0.05)).toFixed(2)}`);
    }
    ok(lum[0] > lum[1], `hue ${hue}: stop A should be the lighter one`);
  }
  eq(seen.size, 360, "hues covered");
});

// ------------------------------------------------------------------------------------------------ viewer volume

test("volume: gain goes up to 500 %, the UI cap matches the core cap, 100 % stays the default", () => {
  const gains = [];
  const ctx = { currentTime: 0, destination: {}, createGain: () => ({ connect() {}, gain: { setTargetAtTime: (v) => gains.push(v) } }) };
  const rx = new AudioReceiver({ context: ctx });
  eq(gains.at(-1), 1, "default 100 %");
  eq(VOLUME_MAX_PCT, MAX_VOLUME * 100);
  rx.setVolume(5);
  eq(gains.at(-1), 5);
  rx.setVolume(2); // a pre-raise stored value stays valid
  eq(gains.at(-1), 2);
  rx.setVolume(9);
  eq(gains.at(-1), 5, "clamped at the cap");
  rx.setMuted(true);
  eq(gains.at(-1), 0);
  rx.setMuted(false);
  eq(gains.at(-1), 5, "unmute restores the chosen volume");
});

test("volume: the slider's 100 % detent snaps within 10 and leaves everything else alone", () => {
  for (const v of [90, 95, 100, 105, 110]) eq(snapVolumePct(v), 100);
  for (const v of [0, 85, 115, 200, 500]) eq(snapVolumePct(v), v);
});

test("volume: wheel step is 5 up to 200 %, 10 above, symmetric at the boundary, clamped (#113)", () => {
  for (const [from, up, to] of [[0, true, 5], [95, true, 100], [100, true, 105], [195, true, 200], [200, true, 210], [205, true, 210],
    [490, true, 500], [500, true, 500], [500, false, 490], [220, false, 210], [210, false, 200], [205, false, 200], [200, false, 195],
    [105, false, 100], [5, false, 0], [0, false, 0]]) eq(wheelStepVolumePct(from, up), to, `${from} ${up ? "up" : "down"}`);
  for (let p = 0; p < 500; p += 5) if (p <= 195 || p % 10 === 0) eq(wheelStepVolumePct(wheelStepVolumePct(p, true), false), p, "up then down at " + p);
});

test("volume: sliders apply on the input event, while the thumb is still being dragged (#135)", async () => {
  // The generic slider: `put` on every input, `onCommit` only on the release-time change event.
  const puts = [];
  let commits = 0;
  let value = 100;
  const [sl] = sliderCtl("t-vol", "Volume", 0, 500, 5, () => value, (v) => { value = v; puts.push(v); }, String, () => { commits++; });
  document.body.appendChild(sl);
  try {
    for (const v of ["120", "150", "180"]) {
      sl.value = v;
      sl.dispatchEvent(new Event("input", { bubbles: true }));
    }
    eq(puts, [120, 150, 180], "every input event reached put with no change event yet");
    eq(commits, 0, "nothing committed before release");
    sl.dispatchEvent(new Event("change", { bubbles: true }));
    eq(commits, 1, "the release commits once");
    eq(puts.length, 3, "and applies nothing more");
  } finally {
    sl.remove();
  }
  // The viewer volume slider (menus.js tileMenu, the stream menu) is not importable without a stage: guard its wiring by source.
  // It must listen to `input` (fires per drag step) and never to `change` (fires on release only).
  const src = await (await fetch(new URL("../js/ui/menus.js", import.meta.url))).text();
  const menu = src.slice(src.indexOf("function tileMenu("), src.indexOf("/* ---------- other menus"));
  ok(menu.includes('slider.addEventListener("input"') && menu.includes("setWatchVolume(r, w,"), "the stream menu applies the gain on input");
  ok(!/slider\.addEventListener\("change"|onchange/.test(menu), "the stream menu has no change-only path");
  ok(menu.includes("volumeWheel(slider)"), "the stream menu's slider takes the wheel");
});

test("volume: wheel events on the slider step it, fire input, are consumed and do not snap to 100 (#113)", () => {
  const s = document.createElement("input");
  Object.assign(s, { type: "range", min: "0", max: "500", step: "5", value: "195" });
  document.body.appendChild(s);
  const seen = [];
  s.addEventListener("input", (ev) => seen.push([s.value, !!ev.fromWheel]));
  volumeWheel(s);
  const wheel = (deltaY, deltaMode = 0) => {
    const e = new WheelEvent("wheel", { deltaY, deltaMode, bubbles: true, cancelable: true });
    s.dispatchEvent(e);
    return e.defaultPrevented;
  };
  try {
    eq(wheel(-100), true, "wheel is consumed (no page scroll)");
    eq(s.value, "200");
    wheel(-120);
    eq(s.value, "210");
    wheel(100);
    eq(s.value, "200");
    wheel(100);
    eq(s.value, "195");
    wheel(3, 1); // Firefox line mode: one notch = 3 lines
    eq(s.value, "190");
    wheel(-20);
    eq(s.value, "190", "a small touchpad delta accumulates first");
    wheel(-40);
    eq(s.value, "195", "...until it adds up to half a notch");
    s.value = "95";
    wheel(-100);
    wheel(-100);
    eq(s.value, "105", "95 -> 100 -> 105 passes through the detent");
    eq(seen.every(([, fromWheel]) => fromWheel), true, "input events carry the wheel flag");
    s.value = "500";
    wheel(-100);
    eq(s.value, "500", "clamped at the top");
  } finally {
    s.remove();
  }
});


// ------------------------------------------------------------------------------------------------ NACK (spec §6.6, #111)
// Ports of the native tests in client/src/fragment.rs / client/src/nack.rs (same cases, same numbers).

const RTT = 25;

/** A reassembler in NACK mode with a clock the test moves. */
function nackReassembler() {
  const clock = { t: 0 };
  const drops = [];
  const r = new Reassembler({ now: () => clock.t, onDrop: (c, s, f) => drops.push(f) });
  r.setNack(true);
  return { r, clock, drops };
}

/** Frame `id` as `count` fragments of 3 bytes labelled with their index. */
function nackFrame(id, count, { keyframe = false, streamId = 0, connId = 1n } = {}) {
  return Array.from({ length: count }, (_, i) => ({
    h: { version: 1, flags: keyframe ? 1 : 0, connId, streamId, frameId: id, fragmentIndex: i, fragmentCount: count, keyframe, stalled: false },
    p: new Uint8Array(3).fill(i),
  }));
}
/** Feed fragments (skipping the listed indices) at time `t`; returns every frame delivered. */
function nackFeed(ctx, t, frags, skip = []) {
  ctx.clock.t = t;
  const out = [];
  for (const { h, p } of frags) if (!skip.includes(h.fragmentIndex)) out.push(...ctx.r.ingest(h, p));
  return out;
}
function nackPoll(ctx, t, rtt = RTT) {
  ctx.clock.t = t;
  return ctx.r.poll(rtt);
}
const runs = (reqs) => reqs.map((q) => [q.first, q.count]);

test("nack: a gap is requested after two later fragments and the repair completes the frame", () => {
  const c = nackReassembler();
  const f = nackFrame(5, 10);
  eq(nackFeed(c, 0, f, [3, 4]).length, 0);
  const { frames, requests } = nackPoll(c, 1);
  eq(frames.length, 0);
  eq(requests, [{ connId: 1n, streamId: 0, frameId: 5, first: 3, count: 2 }]);
  const out = nackFeed(c, 30, f.slice(3, 5)); // the retransmits arrive
  eq(out.length, 1);
  eq(out[0].frameId, 5);
  eq(Array.from(out[0].ciphertext), f.flatMap(({ p }) => Array.from(p)));
  eq(c.drops, []);
  const s = c.r.nackStats;
  eq([s.requests, s.fragmentsRequested, s.fragmentsRecovered, s.framesRecovered, s.framesAbandoned], [1, 2, 2, 1, 0]);
  ok(!c.r.hasPending());
});

test("nack: a hole right behind the newest fragment is not asked for yet, but a quiet frame has lost its tail", () => {
  const c = nackReassembler();
  nackFeed(c, 0, nackFrame(1, 10), [8]); // 8 missing, 9 arrived: one fragment behind it could be reordering
  eq(nackPoll(c, 1).requests.length, 0);
  eq(runs(nackPoll(c, 21).requests), [[8, 1]]); // 20 ms of silence
});

test("nack: a lost tail is requested when the frame goes quiet", () => {
  const c = nackReassembler();
  nackFeed(c, 0, nackFrame(1, 10), [7, 8, 9]);
  eq(nackPoll(c, 5).requests.length, 0, "still arriving as far as we know");
  eq(runs(nackPoll(c, 25).requests), [[7, 3]]);
});

test("nack: later complete frames wait for the repair and come out in order", () => {
  const c = nackReassembler();
  const f1 = nackFrame(1, 6, { keyframe: true });
  eq(nackFeed(c, 0, f1, [2]).length, 0);
  eq(nackFeed(c, 2, nackFrame(2, 3)).length, 0, "held behind frame 1");
  eq(nackFeed(c, 3, nackFrame(3, 1)).length, 0);
  const p = nackPoll(c, 4);
  eq([p.frames.length, p.requests.length], [0, 1]);
  const out = nackFeed(c, 40, f1.slice(2, 3));
  eq(out.map((f) => f.frameId), [1, 2, 3]);
  eq(out.map((f) => f.keyframe), [true, false, false]);
  ok(out.every((f) => f.connId === 1n && f.streamId === 0));
  eq(c.drops, []);
});

test("nack: retries are spaced by the interval, then the frame is abandoned and reported as loss before the held frame is released", () => {
  const c = nackReassembler();
  eq(nackFeed(c, 0, nackFrame(1, 8, { keyframe: true }), [2]).length, 0);
  const iv = nackRetryInterval(RTT);
  eq(iv, 110);
  eq(nackPoll(c, 1).requests.length, 1); // round 1
  eq(nackPoll(c, 1 + iv - 1).requests.length, 0, "not before the interval");
  eq(nackPoll(c, 1 + iv).requests.length, 1); // round 2
  eq(nackPoll(c, 1 + iv * 2).requests.length, 1); // round 3
  eq(nackFeed(c, 1 + iv * 2 + 5, nackFrame(2, 1)).length, 0, "a later frame is held while we wait");
  const { frames, requests } = nackPoll(c, 1 + iv * 3);
  eq(requests.length, 0);
  eq(c.drops, [1], "onDrop fired (the room client turns it into a loss marker) before the held frame is returned");
  eq(frames.map((f) => f.frameId), [2]);
  eq(c.r.nackStats.framesAbandoned, 1);
  eq(c.r.droppedFrames, 1);
  ok(!c.r.hasPending());
});

test("nack: a Miss from the relay gives the frame up at the next poll", () => {
  const c = nackReassembler();
  nackFeed(c, 0, nackFrame(1, 6), [1]);
  const { requests } = nackPoll(c, 1);
  nackFeed(c, 2, nackFrame(2, 1));
  c.r.nackMiss(requests[0]);
  const { frames } = nackPoll(c, 3);
  eq(frames.map((f) => f.frameId), [2]);
  eq(c.drops, [1]);
});

test("nack: a frame with too many holes is abandoned without asking", () => {
  const c = nackReassembler();
  nackFeed(c, 0, nackFrame(1, 400), Array.from({ length: 200 }, (_, i) => 100 + i));
  const { requests } = nackPoll(c, 1);
  eq(requests.length, 0);
  eq(c.drops, [1]);
  ok(!c.r.hasPending());
});

test("nack: duplicates of finished or abandoned frames never open a ghost frame (the 256-id memory)", () => {
  const c = nackReassembler();
  const f1 = nackFrame(1, 4);
  eq(nackFeed(c, 0, f1).length, 1);
  eq(nackFeed(c, 5, f1).length, 0, "the whole frame again: a retransmit that raced the original");
  ok(!c.r.hasPending(), "no ghost incomplete frame");
  const f2 = nackFrame(2, 4);
  nackFeed(c, 0, f2, [0]);
  const iv = nackRetryInterval(RTT);
  for (let k = 0; k < 4; k++) nackPoll(c, 1 + iv * k);
  ok(!c.r.hasPending());
  eq(nackFeed(c, 900, f2.slice(0, 1)).length, 0, "an abandoned frame's late fragment is ignored");
  ok(!c.r.hasPending());
  eq(c.drops, [2], "the abandoned frame is reported once");
  // the memory is bounded: after 300 more frames the first id is forgotten, the recent ones are not
  for (let id = 10; id < 310; id++) nackFeed(c, 1000 + id, nackFrame(id, 1));
  eq(nackFeed(c, 2000, nackFrame(309, 1)).length, 0, "a recent id is still remembered");
  eq(nackFeed(c, 2001, nackFrame(1, 1)).length, 1, "the oldest one has aged out of the 256-id memory");
});

test("nack: nothing is held without NACK mode (legacy untouched)", () => {
  const t = 0;
  const r = new Reassembler({ now: () => t });
  const feedAll = (frags, skip = []) => frags.filter(({ h }) => !skip.includes(h.fragmentIndex)).flatMap(({ h, p }) => r.ingest(h, p));
  eq(feedAll(nackFrame(1, 4), [1]).length, 0);
  eq(feedAll(nackFrame(2, 1)).length, 1, "a later complete frame is delivered at once, the older incomplete one is not waited for");
  eq(r.poll(RTT).requests.length, 0);
  eq(r.poll(RTT).frames.length, 0);
  ok(!r.hasPending());
  eq(r.droppedFrames, 1);
});

test("nack: the retry interval follows the RTT within 100..300 ms", () => {
  eq([5, 25, 100, 400].map(nackRetryInterval), [100, 110, 260, 300]);
});

test("nack: nothing waits longer than the 1 s hard limit, whatever the RTT", () => {
  const c = nackReassembler();
  nackFeed(c, 0, nackFrame(1, 6), [1]);
  nackPoll(c, NACK_HARD_LIMIT_MS + 1, 5000);
  ok(!c.r.hasPending());
  eq(c.drops, [1]);
});

test("nack: a hole that shows up late in a long frame gets its own rounds", () => {
  const c = nackReassembler();
  const f = nackFrame(1, 100, { keyframe: true });
  nackFeed(c, 0, f.slice(0, 30), [3]);
  eq(runs(nackPoll(c, 1).requests), [[3, 1]]);
  nackFeed(c, 50, f.slice(30, 60), [40]); // the paced keyframe is still arriving; another hole behind the newest fragments
  eq(runs(nackPoll(c, 55).requests), [[40, 1]], "3 is not due yet");
  const iv = nackRetryInterval(RTT);
  ok(nackPoll(c, 1 + iv).requests.some((q) => q.first === 3));
  ok(nackPoll(c, 55 + iv).requests.some((q) => q.first === 40));
  ok(c.r.hasPending());
});

test("nack: streams are independent (a hold on one never delays another)", () => {
  const c = nackReassembler();
  nackFeed(c, 0, nackFrame(1, 6, { streamId: 1 }), [1]);
  eq(nackFeed(c, 1, nackFrame(2, 2, { streamId: 2 })).length, 1);
  eq(runs(nackPoll(c, 25).requests), [[1, 1]]);
  eq(nackPoll(c, 26).requests.length, 0, "not asked twice within the interval");
});

test("nack: Request/Miss datagrams match the native bytes; a Hello or a media datagram is not a range", () => {
  const r = { connId: 0x0102030405060708n, streamId: 3, frameId: 99, first: 7, count: 4 };
  for (const tag of [P.NACK_TAG_REQUEST, P.NACK_TAG_MISS]) {
    const b = P.packNackRange(tag, r);
    eq(b.length, 18);
    ok(P.isNackDatagram(b));
    eq(P.unpackNackRange(b), { tag, ...r });
  }
  // the hex asserted by the Rust test `nack_datagrams_round_trip_and_never_look_like_media`
  eq(bytesToHex(P.packNackRange(P.NACK_TAG_REQUEST, r)), "e2010203040506070803000000630007" + "0004");
  ok(P.isNackDatagram(Uint8Array.of(P.NACK_TAG_HELLO)) && P.isNackDatagram(Uint8Array.of(P.NACK_TAG_HELLO_ACK)));
  eq(P.unpackNackRange(Uint8Array.of(P.NACK_TAG_HELLO)), null);
  eq(P.unpackNackRange(P.packNackRange(P.NACK_TAG_REQUEST, { ...r, count: 0 })), null);
  ok(!P.isNackDatagram(makeFrame(1, 100)[0]), "a media datagram is never a NACK");
  ok(!P.isNackDatagram(new Uint8Array(0)));
  eq(P.MEDIA_FLAG_RETRANSMIT, 4);
});

test("nack send ring: a re-send is the original datagram plus the retransmit flag; unknown or evicted frames give nothing", () => {
  let t = 0;
  const ring = new SendRing({ now: () => t });
  const sent = makeFrame(40, 5000, { streamId: 2, connId: 7n, keyframe: true });
  eq(sent.length, 6);
  ring.record(2, 40, sent);
  const got = ring.resends({ streamId: 2, frameId: 40, first: 1, count: 3 });
  eq(got.length, 3);
  got.forEach((d, i) => {
    const { header: h, payload } = P.unpackMediaHeader(d);
    const { header: o, payload: op } = P.unpackMediaHeader(sent[i + 1]);
    eq(h.flags, o.flags | P.MEDIA_FLAG_RETRANSMIT);
    ok(h.keyframe);
    eq([h.frameId, h.fragmentIndex, h.fragmentCount, h.connId], [o.frameId, o.fragmentIndex, o.fragmentCount, o.connId]);
    eq(payload, op, "same ciphertext slice: nothing is re-encrypted");
    ok(d !== sent[i + 1] && sent[i + 1][1] === o.flags, "the cached original is not modified");
  });
  eq(ring.resends({ streamId: 2, frameId: 41, first: 0, count: 2 }).length, 0);
  eq(ring.resends({ streamId: 3, frameId: 40, first: 0, count: 2 }).length, 0);
  eq(ring.resends({ streamId: 2, frameId: 40, first: 4, count: 10 }).length, 2, "a range past the end is clipped");
  // eviction by age: 2 s
  t = 2001;
  ring.record(2, 41, makeFrame(41, 100, { streamId: 2, connId: 7n }));
  eq(ring.resends({ streamId: 2, frameId: 40, first: 0, count: 2 }).length, 0, "older than 2 s is gone");
  eq(ring.resends({ streamId: 2, frameId: 41, first: 0, count: 1 }).length, 1);
  ring.forget(2);
  eq(ring.resends({ streamId: 2, frameId: 41, first: 0, count: 1 }).length, 0);
});

test("nack send ring: capped at 8 MiB per stream, oldest frame first; one request gets at most 256 fragments", () => {
  const ring = new SendRing({ now: () => 0 });
  const big = (id) => makeFrame(id, 1_000_000, { streamId: 1, connId: 7n });
  for (let id = 1; id <= 12; id++) ring.record(1, id, big(id)); // ~12 MB
  eq(ring.resends({ streamId: 1, frameId: 1, first: 0, count: 1 }).length, 0, "the oldest was evicted by the byte cap");
  eq(ring.resends({ streamId: 1, frameId: 12, first: 0, count: 1 }).length, 1);
  ok(ring.streams.get(1).bytes <= SEND_RING_BYTES);
  eq(ring.resends({ streamId: 1, frameId: 12, first: 0, count: 5000 }).length, 256);
});

test("nack kill switch: ?nack=0 / off, localStorage sss-nack=0; default on", () => {
  const url = location.href;
  try {
    eq(nackEnabledByDefault(), true);
    history.replaceState(null, "", "?nack=0");
    eq(nackEnabledByDefault(), false);
    history.replaceState(null, "", "?nack=off");
    eq(nackEnabledByDefault(), false);
    history.replaceState(null, "", "?nack=1");
    eq(nackEnabledByDefault(), true);
    history.replaceState(null, "", url);
    localStorage.setItem("sss-nack", "0");
    eq(nackEnabledByDefault(), false);
  } finally {
    try {
      localStorage.removeItem("sss-nack");
    } catch {}
    history.replaceState(null, "", url);
  }
  eq(nackEnabledByDefault(), true);
});

// ------------------------------------------------------------------------------------------------ loudness leveller (#119)

// Same scenarios and thresholds as the native tests in client/src/leveller.rs (the port must behave identically).
const levRate = 48000;
const levLin = (db) => Math.pow(10, db / 20);
/** Stereo planar sine of `secs` at `rmsDb` dBFS RMS (peak = RMS + 3 dB), continuing phase `ph.v`. */
function levSine(secs, rmsDb, ph) {
  const n = Math.round(secs * levRate);
  const amp = levLin(rmsDb) * Math.SQRT2;
  const a = new Float32Array(n);
  for (let i = 0; i < n; i++) {
    a[i] = amp * Math.sin(ph.v * 2 * Math.PI);
    ph.v = (ph.v + 440 / levRate) % 1;
  }
  return [a, Float32Array.from(a)];
}
const levCat = (parts) => [0, 1].map((c) => {
  const out = new Float32Array(parts.reduce((n, p) => n + p[0].length, 0));
  let o = 0;
  for (const p of parts) { out.set(p[c], o); o += p[0].length; }
  return out;
});
const levPeak = (a) => a.reduce((m, x) => Math.max(m, Math.abs(x)), 0);
const levRms = (a) => 10 * Math.log10(a.reduce((s, x) => s + x * x, 0) / a.length + 1e-20);
/** Run planes through `l` in chunks of `chunk` frames; returns the levelled copy. */
function levRun(l, planes, chunk) {
  const out = planes.map((p) => Float32Array.from(p));
  for (let o = 0; o < out[0].length; o += chunk) l.process(out.map((p) => p.subarray(o, Math.min(o + chunk, p.length))));
  return out;
}

test("leveller: a sudden -40 dBFS -> 0 dBFS step never exceeds the ceiling, even in the first samples, then settles near the target", () => {
  const l = new Leveller(levRate);
  const ph = { v: 0 };
  const quiet = levSine(8, -40, ph); // long enough for the gain to reach its maximum
  const loud = levSine(3, -3, ph); // a full-scale (0 dBFS peak) sine
  const out = levRun(l, levCat([quiet, loud]), 960);
  ok(l.gainDb < MAX_GAIN_DB, "the step pulled the gain down: " + l.gainDb);
  const ceil = levLin(CEILING_DB);
  const q = quiet[0].length;
  for (const ch of out) {
    ok(levPeak(ch.subarray(q)) <= ceil + 1e-6, "peak after the step " + levPeak(ch.subarray(q)));
    ok(levPeak(ch.subarray(q, q + 96)) <= ceil + 1e-6, "first 2 ms already under the ceiling");
  }
  const r = levRms(out[0].subarray(out[0].length - levRate));
  ok(Math.abs(r - TARGET_DB) < 3, `settled at ${r.toFixed(1)} dBFS RMS`);
});

test("leveller: a quiet signal is raised slowly (a few dB per second), up to +12 dB, never instantly", () => {
  const l = new Leveller(levRate);
  const ph = { v: 0 };
  levRun(l, levSine(0.5, -35, ph), 480);
  ok(l.gainDb > 0 && l.gainDb <= RISE_DB_PER_S * 0.5 + 0.2, "after 0.5 s: " + l.gainDb);
  levRun(l, levSine(1.5, -35, ph), 480);
  ok(l.gainDb <= RISE_DB_PER_S * 2 + 0.3, "after 2 s: " + l.gainDb);
  levRun(l, levSine(20, -35, ph), 480);
  ok(Math.abs(l.gainDb - MAX_GAIN_DB) < 0.05, "settled at " + l.gainDb);
});

test("leveller: silence and a noise floor below the gate are not boosted", () => {
  const l = new Leveller(levRate);
  const out = levRun(l, [new Float32Array(levRate * 10), new Float32Array(levRate * 10)], 480);
  eq(l.gainDb, 0);
  ok(out[0].every((x) => x === 0));
  levRun(l, levSine(10, GATE_DB - 15, { v: 0 }), 480);
  eq(l.gainDb, 0, "a -65 dBFS floor stays a floor");
});

test("leveller: loud material is pulled down fast; any input, incl. NaN/Infinity, stays under the ceiling; chunking is irrelevant", () => {
  const l = new Leveller(levRate);
  const ph = { v: 0 };
  const out = levRun(l, levSine(0.1, -3, ph), 480);
  ok(l.gainDb < -5, "after 100 ms: " + l.gainDb);
  ok(levPeak(out[0]) <= levLin(CEILING_DB) + 1e-6);
  const settled = levRun(l, levSine(3, -3, ph), 480);
  ok(Math.abs(levRms(settled[0].subarray(settled[0].length - levRate)) - TARGET_DB) < 2, "settles at the target");

  let seed = 0x2545f491;
  const rnd = () => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return ((seed >>> 0) / 0xffffffff) * 2 - 1; };
  const w = new Leveller(levRate);
  for (let b = 0; b < 60; b++) {
    const lvl = [0.001, 0.05, 1, 40][b % 4];
    const n = 200 + ((b * 37) % 3000);
    const p = [0, 1].map(() => Float32Array.from({ length: n }, () => rnd() * lvl));
    if (b === 7) { p[0][3] = NaN; p[0][4] = Infinity; p[1][5] = -Infinity; }
    w.process(p);
    for (const ch of p) ok(ch.every((x) => Number.isFinite(x) && Math.abs(x) <= levLin(CEILING_DB) + 1e-6), "burst " + b);
  }

  const input = levCat([levSine(1, -30, { v: 0 }), levSine(1, -6, { v: 0 })]);
  const a = levRun(new Leveller(levRate), input, 960);
  const c = levRun(new Leveller(levRate), input, 7);
  ok(a[0].every((x, i) => x === c[0][i]) && a[1].every((x, i) => x === c[1][i]), "same result for any chunking");
});

test("leveller: AudioReceiver levels decoded frames before scheduling, and 'Level loudness' off passes them untouched", async () => {
  const realDecoder = globalThis.AudioDecoder;
  const realChunk = globalThis.EncodedAudioChunk;
  globalThis.AudioDecoder = class { constructor({ output }) { this.output = output; this.state = "configured"; } configure() {} decode(chunk) { this.output(chunk.frame); } close() {} };
  globalThis.EncodedAudioChunk = class { constructor(init) { this.frame = init.data; } };
  try {
    const run = async (level) => {
      const ctx = new OfflineAudioContext(2, levRate, levRate);
      const bufs = [];
      const cb = ctx.createBuffer.bind(ctx);
      ctx.createBuffer = (...a) => { const b = cb(...a); bufs.push(b); return b; };
      const rx = new AudioReceiver({ context: ctx, level });
      const ph = { v: 0 };
      const frames = [...Array.from({ length: 100 }, () => levSine(0.02, -40, ph)), ...Array.from({ length: 50 }, () => levSine(0.02, -3, ph))];
      frames.forEach((f, i) => {
        const data = { numberOfFrames: f[0].length, numberOfChannels: 2, sampleRate: levRate, timestamp: i * 20000, copyTo: (dst, o) => dst.set(f[o.planeIndex]), close() {} };
        rx.pushPacket({ ptsUs: i * 20000, payload: data });
      });
      rx.close();
      return bufs;
    };
    const on = await run(true);
    eq(on.length, 150);
    const ceil = levLin(CEILING_DB);
    ok(on.every((b) => levPeak(b.getChannelData(0)) <= ceil + 1e-6 && levPeak(b.getChannelData(1)) <= ceil + 1e-6), "every scheduled frame is under the ceiling");
    ok(levPeak(on[100].getChannelData(0)) < 0.9, "the loud step is already pulled down in its first frame");
    const off = await run(false);
    ok(levPeak(off[149].getChannelData(0)) > 0.99, "off: the full-scale frame is untouched (" + levPeak(off[149].getChannelData(0)) + ")");
    ok(Math.abs(levPeak(off[0].getChannelData(0)) - levLin(-37)) < 1e-4, "off: the quiet frame is untouched");
  } finally {
    globalThis.AudioDecoder = realDecoder;
    globalThis.EncodedAudioChunk = realChunk;
  }
});

// ------------------------------------------------------------------------------------------------ RelaySession.open fallback

// A scripted WebTransport: `plan(opts, n)` says how the n-th handshake ends ("ok" | "fail" | "hang": pending until closed).
async function withFakeWebTransport(plan, fn) {
  const real = globalThis.WebTransport;
  const made = [];
  globalThis.WebTransport = class {
    constructor(url, opts) {
      this.url = url;
      this.opts = opts;
      this.closeCalls = 0;
      made.push(this);
      const how = plan(opts, made.length - 1);
      this.datagrams = { maxDatagramSize: 1200 };
      this.closed = new Promise((res) => (this._closed = res));
      this.ready = new Promise((res, rej) => {
        this._reject = rej;
        if (how === "ok") res();
        else if (how === "fail") setTimeout(() => rej(new Error("Opening handshake failed.")), 1);
      });
    }
    close() {
      this.closeCalls++;
      this._reject(new Error("closed"));
      this._closed({});
    }
  };
  try {
    await fn(made);
  } finally {
    globalThis.WebTransport = real;
  }
}
const pinnedOf = (wt) => !!(wt.opts && wt.opts.serverCertificateHashes);

test("relay session: no hash in the link = one plain attempt (no fallback to try)", async () => {
  const { RelaySession, isHashVerified } = await import("../js/core/session.js");
  await withFakeWebTransport(() => "fail", async (made) => {
    let err;
    await RelaySession.open({ server: "fb-nohash.test:4433", certHash: null }).catch((e) => (err = e));
    eq(made.length, 1, "attempts");
    ok(!pinnedOf(made[0]), "no pin");
    eq(err.reason, "no-hash");
    ok(!err.mayBeExpiredLink);
  });
  await withFakeWebTransport(() => "ok", async (made) => {
    const s = await RelaySession.open({ server: "fb-nohash.test:4433", certHash: null });
    eq(made.length, 1, "attempts");
    ok(!isHashVerified("fb-nohash.test:4433", new Uint8Array(32)));
    s.close({ graceful: false });
  });
});

test("relay session: a matching hash connects on the first attempt and is marked verified", async () => {
  const { RelaySession, isHashVerified } = await import("../js/core/session.js");
  const hash = new Uint8Array(32).fill(1);
  await withFakeWebTransport(() => "ok", async (made) => {
    const s = await RelaySession.open({ server: "fb-selfsigned.test:4433", certHash: hash });
    eq(made.length, 1, "attempts");
    ok(pinnedOf(made[0]), "pinned");
    ok(isHashVerified("fb-selfsigned.test:4433", hash));
    s.close({ graceful: false });
  });
});

test("relay session: a hash that fails falls back to a plain (WebPKI) attempt, remembered without faking a verified hash", async () => {
  const { RelaySession, isHashVerified } = await import("../js/core/session.js");
  const server = "fb-ca.test:4433";
  const hash = new Uint8Array(32).fill(2);
  // CA relay: only a handshake without a pin succeeds.
  const plan = (opts) => (opts && opts.serverCertificateHashes ? "fail" : "ok");
  await withFakeWebTransport(plan, async (made) => {
    const s = await RelaySession.open({ server, certHash: hash });
    eq(made.length, 2, "attempts");
    ok(pinnedOf(made[0]) && !pinnedOf(made[1]), "pinned first, plain second");
    ok(made[0].closeCalls >= 1, "the failed first transport was closed");
    ok(!isHashVerified(server, hash), "a WebPKI success must not mark the hash as verified");
    s.close({ graceful: false });
  });
  // A reconnect to the same relay skips the pointless pinned attempt.
  await withFakeWebTransport(plan, async (made) => {
    const s = await RelaySession.open({ server, certHash: hash });
    eq(made.length, 1, "attempts on reconnect");
    ok(!pinnedOf(made[0]), "plain first");
    s.close({ graceful: false });
  });
  // The relay goes down: both fail, and it is reported as not answering (it was reachable before), not as an expired link.
  await withFakeWebTransport(() => "fail", async (made) => {
    let err;
    await RelaySession.open({ server, certHash: hash }).catch((e) => (err = e));
    eq(made.length, 2, "attempts");
    eq(err.reason, "unreachable");
    ok(!err.mayBeExpiredLink);
  });
  // The relay went back to a self-signed cert with this very hash: the pinned attempt (now second) still works.
  await withFakeWebTransport((opts) => (opts && opts.serverCertificateHashes ? "ok" : "fail"), async (made) => {
    const s = await RelaySession.open({ server, certHash: hash });
    eq(made.length, 2, "attempts");
    ok(isHashVerified(server, hash));
    s.close({ graceful: false });
  });
});

test("relay session: both attempts failing gives the fresh-link hint, each attempt with its own timeout", async () => {
  const { RelaySession, EXPIRED_LINK_HINT } = await import("../js/core/session.js");
  await withFakeWebTransport(() => "hang", async (made) => {
    let err;
    const t0 = performance.now();
    await RelaySession.open({ server: "fb-dead.test:4433", certHash: new Uint8Array(32).fill(3), timeoutMs: 60 }).catch((e) => (err = e));
    const ms = performance.now() - t0;
    eq(made.length, 2, "attempts");
    ok(pinnedOf(made[0]) && !pinnedOf(made[1]));
    ok(made.every((w) => w.closeCalls >= 1), "both transports closed");
    ok(ms >= 110, `two timeouts back to back (${Math.round(ms)} ms)`);
    eq(err.reason, "unreachable-or-expired-link");
    ok(err.mayBeExpiredLink);
    eq(err.message, EXPIRED_LINK_HINT);
    ok(/fresh invite link/.test(err.message));
  });
});

test("relay session: cancelling (abort signal) never starts the second attempt", async () => {
  const { RelaySession } = await import("../js/core/session.js");
  const hash = new Uint8Array(32).fill(4);
  await withFakeWebTransport(() => "hang", async (made) => {
    const ac = new AbortController();
    let err;
    const p = RelaySession.open({ server: "fb-abort.test:4433", certHash: hash, signal: ac.signal }).catch((e) => (err = e));
    await new Promise((r) => setTimeout(r, 20));
    ac.abort();
    await p;
    eq(made.length, 1, "attempts");
    eq(err.code, "aborted");
    ok(made[0].closeCalls >= 1);
  });
  await withFakeWebTransport(() => "ok", async (made) => {
    const ac = new AbortController();
    ac.abort();
    let err;
    await RelaySession.open({ server: "fb-abort.test:4433", certHash: hash, signal: ac.signal }).catch((e) => (err = e));
    eq(made.length, 0, "attempts");
    eq(err.code, "aborted");
  });
});

// ------------------------------------------------------------------------------------------------ stage geometry (port of client/src/ui/stage/geom.rs tests)

const AR = 16 / 9;
const R = (x, y, w, h) => ({ x, y, w, h });
const near = (a, b, eps = 1e-3, what = "") => ok(Math.abs(a - b) < eps, `${what} ${a} vs ${b}`);
const centerOf = (p, bar) => {
  const b = p.bars.find((x) => G.sameBar(x.bar, bar));
  return G.barCenter(bar, b.rect);
};

test("stage geometry: best_rows picks the row count with the most picture", () => {
  eq(G.bestRows([AR, AR], 1600, 500), [2]);
  eq(G.bestRows([AR, AR], 600, 1000), [1, 1]);
  eq(G.bestRows([AR, AR, AR], 1000, 700), [2, 1], "three: two rows, the first with 2");
  eq(G.bestRows([AR, AR, AR, AR], 1600, 900), [2, 2]);
  eq(G.bestRows([AR], 800, 300), [1]);
  eq(G.bestRows([], 800, 300), []);
});

test("stage geometry: contain letterboxes and centres; cells tile the arena with gaps", () => {
  let r = G.contain(R(10, 20, 400, 400), AR);
  near(r.w, 400); near(r.h, 225); near(r.x + r.w / 2, 210); near(r.y + r.h / 2, 220);
  r = G.contain(R(0, 0, 1000, 200), AR);
  near(r.h, 200); near(r.w, 200 * AR);
  const c = G.gridCells(G.evenGrid([2, 1]), 1000, 500);
  eq(c.length, 3);
  near(c[0].w, 496); near(c[1].x, 504); near(c[0].h, 246); near(c[2].y, 254); near(c[2].w, 1000);
});

test("stage geometry: bars sit midway between the VISIBLE pictures, not on the cell edge", () => {
  // two 16:9 in a 1800x400 arena: height-limited pictures (711 wide) in 896 wide cells
  const g = G.evenGrid([2]);
  let p = G.place(g, [AR, AR], 1800, 400, false);
  const [a, b] = p.pics;
  ok(a.w < G.gridCells(g, 1800, 400)[0].w - 1, "letterboxed");
  const bar = { t: "v", row: 0, col: 0 };
  near(centerOf(p, bar), (a.x + a.w + b.x) / 2);
  // Crop: the pictures are the cells, the bar is the middle of the gap
  p = G.place(g, [AR, AR], 1800, 400, true);
  near(centerOf(p, bar), 900);
  // a horizontal bar between rows of differing picture height uses the pictures too
  p = G.place(G.evenGrid([1, 1]), [AR, AR], 400, 1000, false);
  near(centerOf(p, { t: "h", row: 0 }), (p.pics[0].y + p.pics[0].h + p.pics[1].y) / 2);
});

test("stage geometry: a dragged bar follows the pointer one to one until the limit (60 px = 60 px)", () => {
  const ars = [AR, AR];
  for (const [w, h, crop] of [[1800, 400, false], [1000, 500, false], [1800, 400, true], [1000, 500, true]]) {
    const g = G.evenGrid([2]);
    const bar = { t: "v", row: 0, col: 0 };
    const c0 = centerOf(G.place(g, ars, w, h, crop), bar);
    for (const d of [60, -60, 25]) {
      const c = centerOf(G.place(G.dragBar(g, bar, c0 + d, ars, w, h, crop), ars, w, h, crop), bar);
      near(c, c0 + d, 0.5, `${w}x${h} crop=${crop} d=${d}`);
    }
    const g2 = G.dragBar(g, bar, w * 3, ars, w, h, crop);
    ok(g2.colFr[0][0] <= 1 - G.MIN_FR + 1e-4 && g2.colFr[0][1] >= G.MIN_FR - 1e-4, "stops at the limit");
    ok(G.dragBar(g, bar, -w, ars, w, h, crop).colFr[0][0] >= G.MIN_FR - 1e-4, "stops at the other limit");
  }
  const g = G.evenGrid([1, 1]);
  const bar = { t: "h", row: 0 };
  const c0 = centerOf(G.place(g, ars, 600, 900, true), bar);
  near(centerOf(G.place(G.dragBar(g, bar, c0 + 60, ars, 600, 900, true), ars, 600, 900, true), bar), c0 + 60, 0.5);
  // dragging a bar keeps the other columns
  const g3 = G.dragBar(G.evenGrid([3]), { t: "v", row: 0, col: 0 }, 200, [AR, AR, AR], 1500, 500, true);
  near(g3.colFr[0][2], 1 / 3, 1e-6);
  near(g3.colFr[0].reduce((x, y) => x + y, 0), 1, 1e-5);
});

test("stage geometry: a floating window always has its stream's aspect ratio, also coming from Crop or after a frame size change", () => {
  const [w, h] = [1200, 600];
  const from = (r) => ({ x: r.x / w, y: r.y / h, w: r.w / w, h: r.h / h });
  const f = G.fitFloat(from(R(0, 0, 600, 600)), AR, w, h); // a Crop cell (wider than the picture) turned into a window
  near((f.w * w) / (f.h * h), AR, 1e-3, "ratio restored");
  const f2 = G.fitFloat(f, 4 / 3, w, h); // the stream's ratio changes: the width stays, the height follows
  near((f2.w * w) / (f2.h * h), 4 / 3);
  const big = G.fitFloat({ x: 0.9, y: 0.9, w: 3, h: 3 }, AR, w, h);
  ok((big.x + big.w) * w <= w + 1e-2 && (big.y + big.h) * h <= h + 1e-2 && big.x >= -1e-3 && big.y >= -1e-3, "inside the arena");
  near((big.w * w) / (big.h * h), AR);
});

test("stage geometry: corner resize uses width and height together (also moving only left or only up shrinks)", () => {
  const [w, h] = [1600, 900];
  const o = G.fitFloat({ x: 0.1, y: 0.1, w: 0.4, h: 0 }, AR, w, h);
  const ow = o.w * w;
  ok(G.resizeFloat(o, 100, 56.25, AR, w, h).w * w > ow + 90, "right and down grows");
  ok(G.resizeFloat(o, -100, -56.25, AR, w, h).w * w < ow - 90, "left and up shrinks");
  ok(G.resizeFloat(o, -80, 0, AR, w, h).w * w < ow - 10, "only left shrinks");
  ok(G.resizeFloat(o, 0, -80, AR, w, h).w * w < ow - 10, "only up shrinks");
  ok(G.resizeFloat(o, 0, 80, AR, w, h).w * w > ow + 10, "only down grows");
  const r = G.resizeFloat(o, 5000, 5000, AR, w, h);
  near(r.x, o.x, 1e-6); near(r.y, o.y, 1e-6);
  ok((r.x + r.w) * w <= w + 1e-2 && (r.y + r.h) * h <= h + 1e-2, "inside the arena");
  near((r.w * w) / (r.h * h), AR);
  near(G.resizeFloat(o, -5000, -5000, AR, w, h).w * w, G.FLOAT_MIN_W, 1e-2);
  const m = G.moveFloat({ x: 0.2, y: 0.2, w: 0.3, h: 0.3 }, 60, -30, 1000, 500);
  near(m.x * 1000, 0.2 * 1000 + 60); near(m.y * 500, 0.2 * 500 - 30);
  const far = G.moveFloat({ x: 0.2, y: 0.2, w: 0.3, h: 0.3 }, 9999, -9999, 1000, 500);
  near(far.x + far.w, 1, 1e-6); near(far.y, 0, 1e-6);
});

test("stage: the hover card stays open over the card, the avatar and the gap straight above it, NOT over the neighbours' avatars", () => {
  // the numbers of the native test (found in the rig: a card is wider than a person's cell)
  const av = { left: 1113, top: 941, right: 1157, bottom: 985 };
  const card = { left: 936, top: 797, right: 1258, bottom: 929 };
  for (const q of [{ x: 1000, y: 850 }, { x: 1135, y: 963 }, { x: 1135, y: 935 }]) ok(G.cardKeeps(card, av, q), show(q));
  ok(!G.cardKeeps(card, av, { x: 1174, y: 963 }), "the next person's avatar");
  ok(!G.cardKeeps(card, av, { x: 1018, y: 963 }), "the previous person's avatar");
});

// ------------------------------------------------------------------------------------------------ fullscreen chrome (port of stage/chrome.rs tests)

function runChrome(s, frames) {
  const out = [];
  for (const [t, inside, active, held] of frames) {
    const [n, shown] = chromeStep(s, t, inside, active, held);
    s = n;
    out.push(shown);
  }
  return [s, out];
}
const SEC = 1000;
test("fullscreen chrome: hides after 3 s without movement, movement brings it back and restarts the 3 s", () => {
  eq(CHROME_IDLE_MS, 3000);
  let [s, shown] = runChrome(chromeStart(0), [[100, true, true, false], [2900, true, false, false], [3200, true, false, false]]);
  eq(shown, [true, true, false]);
  [s, shown] = runChrome(s, [[10 * SEC, true, true, false], [12.9 * SEC, true, false, false], [13.2 * SEC, true, false, false]]);
  eq(shown, [true, true, false], "movement restarts the three seconds");
});
test("fullscreen chrome: leaving the stage hides it at once, a grace period survives it", () => {
  eq(runChrome(chromeStart(0), [[1000, true, true, false], [1100, false, false, false]])[1], [true, false]);
  eq(runChrome(chromeStart(0), [[100, false, false, false], [2900, false, false, false], [3100, false, false, false]])[1], [true, true, false], "entering fullscreen with the pointer elsewhere");
  eq(runChrome(chromeStart(0), [[5000, true, true, false], [5500, false, false, false]])[1], [true, false], "a move on the stage ended the grace");
});
test("fullscreen chrome: a menu stops the timer, and after it closes the bar gets a full 3 s", () => {
  let [s, shown] = runChrome(chromeStart(0), [[500, true, true, false], [30 * SEC, false, false, true], [60 * SEC, false, false, true]]);
  eq(shown, [true, true, true], "held up for as long as the menu is open, off the stage too");
  eq(chromeRemaining(s, 60 * SEC), null, "no timer runs while held");
  [, shown] = runChrome(s, [[60.1 * SEC, false, false, false], [62.9 * SEC, false, false, false], [63.2 * SEC, false, false, false]]);
  eq(shown, [true, true, false], "a full 3 s after the close, not hidden at once");
});

// ------------------------------------------------------------------------------------------------ emoji avatars (spec 9.2)

const hexOf = (b) => bytesToHex(b);
test("avatars: AvatarSet bytes, round trip, limits (the vectors of protocol/src/lib.rs avatar_datagrams_round_trip_and_reject_garbage)", () => {
  const set = P.packAvatarSet("🦊");
  eq(hexOf(set), "ebf09fa68a");
  eq(P.unpackAvatarSet(set), "🦊");
  eq(P.unpackAvatarSet(P.packAvatarSet("")), "", "no bytes after the tag = no emoji");
  const longest = P.packAvatarSet("x".repeat(P.MAX_AVATAR_BYTES));
  ok(longest.length < P.MEDIA_HEADER_LEN && P.isNackDatagram(longest), "short enough that an older relay drops it as undersized");
  eq(P.packAvatarSet("x".repeat(P.MAX_AVATAR_BYTES + 1)), null);
  eq(P.unpackAvatarSet(Uint8Array.from([P.AVATAR_TAG_SET, ...new Array(17).fill(0x78)])), null, "too long");
  eq(P.unpackAvatarSet(Uint8Array.from([P.AVATAR_TAG_SET, 0xff, 0xfe])), null, "not UTF-8");
  eq(P.unpackAvatarSet(Uint8Array.from([P.AVATAR_TAG_ACK])), null);
  eq([P.AVATAR_TAG_HELLO, P.AVATAR_TAG_ACK, P.AVATAR_TAG_SET, P.AVATAR_TAG_LIST], [0xe9, 0xea, 0xeb, 0xec]);
});
test("avatars: AvatarList bytes, round trip, and the whole list is discarded for any bad entry", () => {
  const l = P.packAvatars([[1n, "🦊"], [2n, ""], [0x0102030405060708n, "💀"]]);
  eq(hexOf(l), "ec02000000000000000104f09fa68a010203040506070804f09f9280");
  eq(P.unpackAvatars(l), [[1n, "🦊"], [0x0102030405060708n, "💀"]]);
  eq(P.unpackAvatars(P.packAvatars([])), []);
  eq(P.unpackAvatars(l.subarray(0, l.length - 1)), null, "truncated");
  eq(P.unpackAvatars(Uint8Array.from([...l, 0])), null, "trailing byte");
  eq(P.unpackAvatars(Uint8Array.from([P.AVATAR_TAG_LIST, 1, 0, 0, 0, 0, 0, 0, 0, 1, 0])), null, "empty emoji entry");
  eq(P.unpackAvatars(Uint8Array.from([P.AVATAR_TAG_LIST, 1, 0, 0, 0, 0, 0, 0, 0, 1, 1, 0xff])), null, "not UTF-8");
  eq(P.unpackAvatars(Uint8Array.from([P.AVATAR_TAG_LIST, 1, 0, 0, 0, 0, 0, 0, 0, 1, 17, ...new Array(17).fill(0x78)])), null, "entry over 16 bytes");
  eq(P.unpackAvatars(P.packAvatars(Array.from({ length: 100 }, (_, i) => [BigInt(i), "🦊"]))).length, P.MAX_AVATAR_ENTRIES);
  const widest = P.packAvatars(Array.from({ length: 100 }, (_, i) => [BigInt(i), "xxxxxxxxxxxxxxxx"]));
  ok(widest.length <= 1200, `a full list fits one datagram: ${widest.length}`);
  ok(l.length >= P.MEDIA_HEADER_LEN, "a list can be longer than a media header: the receiver must test tag 0xEC BEFORE the media path");
});
test("avatars: the picker takes the last emoji of a text as one grapheme (a flag or a ZWJ family stays whole), nothing else", () => {
  eq(pickEmoji("🦊"), "🦊");
  eq(pickEmoji("🦊🐻"), "🐻", "pasting after the old one");
  eq(pickEmoji("abc 🇩🇪"), "🇩🇪", "a flag is one grapheme");
  eq(pickEmoji("👨‍👩‍👧"), "👨‍👩‍👧", "a ZWJ family is one grapheme");
  eq(pickEmoji("hello"), "");
  eq(pickEmoji(""), "");
});

// ------------------------------------------------------------------------------------------------ presets in the UI settings (I6)

test("settings: a stored balanced or low_latency preset loads as quality, quality/text/gaming are kept", () => {
  const key = "sss.settings.v1";
  const before = localStorage.getItem(key);
  try {
    for (const [stored, want] of [["balanced", "quality"], ["low_latency", "quality"], ["quality", "quality"], ["text", "text"], ["gaming", "gaming"], ["bogus", "quality"]]) {
      localStorage.setItem(key, JSON.stringify({ stream: { preset: stored } }));
      eq(loadSettings().stream.preset, want, stored);
    }
    localStorage.removeItem(key);
    eq(loadSettings().stream.preset, "quality", "the default");
    eq(loadSettings().emoji, "", "no emoji by default");
    localStorage.setItem(key, JSON.stringify({ emoji: " 🦊 " }));
    eq(loadSettings().emoji, "🦊", "the saved emoji");
  } finally {
    if (before == null) localStorage.removeItem(key);
    else localStorage.setItem(key, before);
  }
});
test("i18n: every stage string has an English and a German text and the same placeholders", () => {
  for (const k of Object.keys(DICT.en)) {
    ok(k in DICT.de, `de is missing ${k}`);
    eq((DICT.de[k].match(/%\{\w+\}/g) || []).sort(), (DICT.en[k].match(/%\{\w+\}/g) || []).sort(), `placeholders of ${k}`);
  }
  for (const k of ["ui.preset.text", "ui.stage.layout_split", "ui.strip.sharing_n", "ui.settings.avatar_paste"]) ok(DICT.en[k] && DICT.de[k], k);
});

// ------------------------------------------------------------------------------------------------ notification sounds (I9, port of client/src/sounds.rs tests)
// Pure: nothing here creates an AudioContext or plays anything.

test("sounds: the four definitions are the replica's, and the plan's peak is volume/100 * 0.3 (nothing at 0)", () => {
  eq(SOUND_NAMES, ["join", "leave", "start", "stop"]);
  eq(SOUNDS.join, { wave: "sine", notes: [659, 880] });
  eq(SOUNDS.leave, { wave: "sine", notes: [880, 587] });
  eq(SOUNDS.start, { wave: "triangle", notes: [523, 659, 784] });
  eq(SOUNDS.stop, { wave: "triangle", notes: [392, 294] });
  for (const n of SOUND_NAMES) {
    eq(soundPlan(n, 0), null, `${n} at 0`);
    for (const v of [5, 50, 100]) near(soundPlan(n, v).peak, (v / 100) * 0.3, 1e-12, `${n} at ${v}`);
    near(soundPlan(n, 1000).peak, 0.3, 1e-12, "more than 100 is 100");
  }
  eq(soundPlan("start", 50).notes.map((x) => x.at), [0, 0.09, 0.18]);
  const plans = SOUND_NAMES.map((n) => JSON.stringify(soundPlan(n, 100)));
  eq(new Set(plans).size, 4, "the four sounds differ");
});

const lists = (o) => new Map(Object.entries(o).map(([k, v]) => [Number(k), v]));
const vpass = (r, mine, l, now = 0) => (r.viewers(new Set(mine), lists(l)), r.drain(now));

test("sounds: viewers join and leave once per change; the relay's 2 s repeats and an old relay are silent", () => {
  const r = new RoomSounds();
  eq(vpass(r, [3], {}), [], "no list yet (an old relay never sends one)");
  eq(vpass(r, [3], { 3: ["a"] }), ["join"]);
  for (let i = 0; i < 5; i++) eq(vpass(r, [3], { 3: ["a"] }), [], "a repeat");
  eq(vpass(r, [3], { 3: ["a", "b"] }), ["join"]);
  eq(vpass(r, [3], { 3: ["b"] }), ["leave"]);
  eq(vpass(r, [3], { 3: [] }), ["leave"]);
  eq(vpass(r, [3], { 3: [] }), []);
});

test("sounds: a person across several streams of mine counts once; a join and a leave in one pass are both heard", () => {
  const r = new RoomSounds();
  eq(vpass(r, [3, 4], { 3: ["a"] }), ["join"]);
  eq(vpass(r, [3, 4], { 3: ["a"], 4: ["a"] }), []);
  eq(vpass(r, [3, 4], { 3: [], 4: ["a"] }), [], "still watching the other one");
  eq(vpass(r, [3, 4], { 3: [], 4: [] }), ["leave"]);
  eq(vpass(r, [3, 4], { 3: ["a", "c"], 4: ["d"] }), ["join"]);
  eq(vpass(r, [3, 4], { 3: ["e"], 4: [] }), ["join", "leave"]);
});

test("sounds: stopping my own stream is not everyone leaving; lists of streams that are not mine are ignored", () => {
  const r = new RoomSounds();
  vpass(r, [3], { 3: ["a", "b"] });
  eq(vpass(r, [], { 3: ["a", "b"] }), [], "I stopped stream 3");
  eq(vpass(r, [], {}), []);
  eq(vpass(r, [5], { 5: ["a"] }), ["join"], "a new stream of mine starts from nobody");
  eq(vpass(r, [], { 9: ["a"] }), []);
  eq(vpass(r, [3], { 9: ["a"] }), []);
});

test("sounds: someone else's streams start and stop by key; the snapshot, a label change and my own streams are silent", () => {
  const r = new RoomSounds();
  r.streams(["x:1", "y:1"], false); // the snapshot of a (re)connect: baseline only
  eq(r.drain(0), []);
  r.streams(["x:1", "y:1"], true); // the same list again (a label change keeps the key)
  eq(r.drain(0), []);
  r.streams(["x:1", "y:1", "z:2"], true);
  eq(r.drain(0), ["start"]);
  r.streams(["x:1", "y:1"], true);
  eq(r.drain(0), ["stop"]);
  r.streams(["x:1", "q:1"], true); // one gone and one new in one pass: both, once each
  eq(r.drain(0), ["start", "stop"]);
  r.streams(["a:1", "a:2", "a:3", "a:4", "a:5", "a:6", "a:7", "a:8"], true);
  eq(r.drain(0), ["start", "stop"], "8 at once are one sound each");
  eq(r.drain(0), [], "drained");
});

test("sounds: a resumed room is quiet for a while but keeps its baselines", () => {
  const r = new RoomSounds();
  r.quiet(RESUME_QUIET_MS);
  r.streams(["x:1"], true);
  r.viewers(new Set([3]), lists({ 3: ["a"] }));
  eq(r.drain(1000), [], "the replay is silent");
  r.viewers(new Set([3]), lists({ 3: ["a"] }));
  eq(r.drain(RESUME_QUIET_MS + 1), [], "the person who came back is the baseline, their repeat is no join");
  r.streams(["x:1", "y:1"], true);
  eq(r.drain(RESUME_QUIET_MS + 1), ["start"]);
});

test("sounds: the gate spaces one sound by a second and leaves the others alone", () => {
  const g = new Gate();
  ok(g.admit("join", 0));
  ok(!g.admit("join", 10));
  ok(g.admit("leave", 10));
  ok(!g.admit("join", MIN_GAP_MS - 1));
  ok(g.admit("join", MIN_GAP_MS));
});

test("sounds: the volume setting defaults to 50, is clamped and survives a bad stored value", () => {
  const key = "sss.settings.v1";
  const before = localStorage.getItem(key);
  try {
    localStorage.removeItem(key);
    eq(loadSettings().soundVol, 50, "default");
    localStorage.setItem(key, JSON.stringify({ soundVol: 0 }));
    eq(loadSettings().soundVol, 0, "0 is silent, not the default");
    localStorage.setItem(key, JSON.stringify({ soundVol: 250 }));
    eq(loadSettings().soundVol, 100);
    localStorage.setItem(key, JSON.stringify({ soundVol: "loud" }));
    eq(loadSettings().soundVol, 50);
  } finally {
    if (before == null) localStorage.removeItem(key);
    else localStorage.setItem(key, before);
  }
});
test("i18n: every Sounds string has both languages", () => {
  for (const k of ["sounds_card", "sound_volume", "sound_volume_tip", "sound_join", "sound_leave", "sound_start", "sound_stop", "sound_test"]) ok(DICT.en["ui.settings." + k] && DICT.de["ui.settings." + k], k);
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
