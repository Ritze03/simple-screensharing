// Join links: `server|room-id|key[|certhash[|name]]`, mirroring `client/src/net.rs`
// (`parse_join_string` / `format_join_string_full`); protocol-spec.md section 11.4.
//
//   server    host:port of the relay
//   room-id   16 bytes, 32 lowercase hex chars
//   key       32-byte AES-256-GCM room key, base64url, no padding (43 chars)
//   certhash  optional, SHA-256 of the relay's leaf cert DER, base64url, no padding (43 chars);
//             empty when only a name follows (`server|id|key||name`, a CA-certificate relay)
//   name      optional, the room's display name, UTF-8 percent-encoded (task #69, user request 2026-10-15).
//             Cosmetic and not part of the room's identity: a name that does not decode (malformed `%`, invalid
//             UTF-8, empty after trimming) is dropped, the link stays valid.

export class JoinLinkError extends Error {
  constructor(message) {
    super(message);
    this.name = "JoinLinkError";
  }
}

const B64URL = /^[A-Za-z0-9_-]*$/;

/** Strict base64url (no padding) decode; rejects non-canonical trailing bits like Rust's `base64` does. */
export function base64urlDecode(s) {
  if (!B64URL.test(s)) throw new JoinLinkError("not valid base64url (no padding)");
  if (s.length % 4 === 1) throw new JoinLinkError("not valid base64url (bad length)");
  const std = s.replace(/-/g, "+").replace(/_/g, "/") + "===".slice(0, (4 - (s.length % 4)) % 4);
  const bin = atob(std);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  if (base64urlEncode(out) !== s) throw new JoinLinkError("not valid base64url (non-canonical encoding)");
  return out;
}

export function base64urlEncode(bytes) {
  let bin = "";
  for (let i = 0; i < bytes.length; i++) bin += String.fromCharCode(bytes[i]);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

export function bytesToHex(bytes) {
  let out = "";
  for (const b of bytes) out += b.toString(16).padStart(2, "0");
  return out;
}

export function hexToBytes(hex) {
  if (hex.length % 2 || !/^[0-9a-fA-F]*$/.test(hex)) throw new JoinLinkError("invalid hex");
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** Longest room name (chars) a link's 5th field may carry; longer names are cut (native `MAX_LINK_ROOM_NAME_CHARS`). */
export const MAX_LINK_ROOM_NAME_CHARS = 64;

/**
 * Percent-encode a room name as UTF-8 exactly like the native encoder: only `A-Za-z0-9-._~` stay raw, so `|` and `%`
 * can never appear and the output is plain ASCII. `encodeURIComponent` additionally leaves `! ' ( ) *` raw; they are
 * escaped here so a link formatted by either client is byte-identical. Returns "" for a blank name or one that is
 * not valid UTF-16 (a lone surrogate): the name is optional, so it is simply left off.
 */
export function encodeLinkName(name) {
  const n = String(name ?? "").trim();
  if (!n) return "";
  try {
    return encodeURIComponent(n).replace(/[!'()*]/g, (c) => "%" + c.charCodeAt(0).toString(16).toUpperCase());
  } catch {
    return "";
  }
}

/**
 * Lenient inverse of {@link encodeLinkName} (native `decode_link_name`): null for a malformed `%` sequence, invalid
 * UTF-8, or a name that is empty after trimming. Control characters are dropped and the result is capped at
 * {@link MAX_LINK_ROOM_NAME_CHARS} characters (code points). Accepts `! ' ( ) *` raw, as `encodeURIComponent` emits.
 */
export function decodeLinkName(field) {
  let decoded;
  try {
    decoded = decodeURIComponent(String(field ?? "").trim());
  } catch {
    return null;
  }
  const chars = Array.from(decoded.replace(/\p{Cc}/gu, "")).slice(0, MAX_LINK_ROOM_NAME_CHARS);
  const name = chars.join("").trim();
  return name || null;
}

const EXPECTED = "expected server|room-id|key or server|room-id|key|certhash";

/**
 * Parse a join link. Throws JoinLinkError with the same wording as the native client.
 * @returns {{server:string, host:string, port:number|null, roomId:Uint8Array, roomIdHex:string,
 *            roomKey:Uint8Array, certHash:Uint8Array|null, name:string|null}}
 */
export function parseJoinLink(link) {
  const s = String(link).trim();
  // Rust: splitn(5, '|') — the 5th field (the name) keeps any further '|'.
  const parts = [];
  let rest = s;
  for (let i = 0; i < 4; i++) {
    const at = rest.indexOf("|");
    if (at < 0) {
      parts.push(rest);
      rest = null;
      break;
    }
    parts.push(rest.slice(0, at));
    rest = rest.slice(at + 1);
  }
  if (rest !== null) parts.push(rest);

  if (!parts[0]) throw new JoinLinkError(`join string missing server address (${EXPECTED})`);
  if (parts.length < 2) throw new JoinLinkError(`join string missing room id (${EXPECTED})`);
  if (parts.length < 3) throw new JoinLinkError(`join string missing key (${EXPECTED})`);

  const server = parts[0];
  const roomIdHex = parts[1];
  if (roomIdHex.length !== 32) throw new JoinLinkError(`room id must be 32 hex characters (16 bytes), got ${roomIdHex.length} characters`);
  if (!/^[0-9a-fA-F]{32}$/.test(roomIdHex)) throw new JoinLinkError("room id has invalid hex");
  const roomId = hexToBytes(roomIdHex);

  let roomKey;
  try {
    roomKey = base64urlDecode(parts[2]);
  } catch (e) {
    throw new JoinLinkError("room key is not valid base64url (no padding)");
  }
  if (roomKey.length !== 32) throw new JoinLinkError(`room key must decode to 32 bytes for AES-256, got ${roomKey.length} bytes`);

  // A trailing `|` with nothing after it is "no hash".
  let certHash = null;
  const hashField = parts[3] === undefined ? "" : parts[3].trim();
  if (hashField) {
    try {
      certHash = base64urlDecode(hashField);
    } catch (e) {
      throw new JoinLinkError("join string has an invalid cert hash");
    }
    if (certHash.length !== 32) throw new JoinLinkError(`join string has an invalid cert hash (must decode to 32 bytes, got ${certHash.length})`);
  }

  const name = parts[4] === undefined ? null : decodeLinkName(parts[4]);

  const { host, port } = splitHostPort(server);
  return { server, host, port, roomId, roomIdHex: roomIdHex.toLowerCase(), roomKey, certHash, name };
}

/** Split `host:port`, `[v6]:port` or a bare host. */
export function splitHostPort(server) {
  const m6 = /^\[([^\]]+)\](?::(\d+))?$/.exec(server);
  if (m6) return { host: m6[1], port: m6[2] ? Number(m6[2]) : null };
  const at = server.lastIndexOf(":");
  if (at > 0 && /^\d+$/.test(server.slice(at + 1)) && server.indexOf(":") === at) {
    return { host: server.slice(0, at), port: Number(server.slice(at + 1)) };
  }
  return { host: server, port: null };
}

/**
 * `server|room-id|key[|certhash[|name]]`. A blank `name` yields the 3/4-field form; a name without a hash leaves the
 * 4th field empty (`server|id|key||name`), like native `format_join_string_full`.
 */
export function formatJoinLink(server, roomId, roomKey, certHash = null, name = null) {
  let out = `${server}|${bytesToHex(roomId)}|${base64urlEncode(roomKey)}`;
  const enc = encodeLinkName(name);
  if (certHash || enc) out += "|";
  if (certHash) out += base64urlEncode(certHash);
  if (enc) out += `|${enc}`;
  return out;
}

/**
 * The link as a saved room stores it: without the name (a local, editable label; native also keeps the name out of the
 * saved join string). A link with at most four fields is returned unchanged (trimmed).
 */
export function stripLinkName(link) {
  const parts = String(link).trim().split("|");
  if (parts.length < 5) return parts.join("|");
  return parts.slice(0, 4).join("|").replace(/\|$/, "");
}

/**
 * Mint a brand-new room (random id + key, like native `create_room`).
 * The room name is local (echoed back as `name`); the returned `link` carries it as the 5th field, like native
 * `invite_link`, so a link shared from here pre-fills the receiver's join dialog.
 * Nothing touches the network; joining the room is what creates it on the relay.
 */
export function createRoom(server, { name = "", certHash = null } = {}) {
  server = String(server).trim();
  if (!server) throw new JoinLinkError("server address must not be empty");
  const roomId = crypto.getRandomValues(new Uint8Array(16));
  const roomKey = crypto.getRandomValues(new Uint8Array(32));
  return { name: String(name).trim(), server, roomId, roomKey, link: formatJoinLink(server, roomId, roomKey, certHash, name) };
}
