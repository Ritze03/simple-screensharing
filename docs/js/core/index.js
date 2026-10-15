// Public entry point of the browser client core: `import { RoomClient, parseJoinLink, ... } from "./js/core/index.js"`.
// Everything is a plain ES module with relative imports (no build step); see README.md.

export { RoomClient, WatchHandle, ShareHandle, capabilities, probeEncoders, streamKey } from "./room-client.js";
export { CanvasRenderer, visibleRectFor } from "./video-rx.js";
export { createRoom, parseJoinLink, formatJoinLink, stripLinkName, encodeLinkName, decodeLinkName, MAX_LINK_ROOM_NAME_CHARS, JoinLinkError, base64urlEncode, base64urlDecode } from "./joinlink.js";
export { ConnectError, JoinRejectedError, EXPIRED_LINK_HINT, CLOSE_CODES, classifyClose, isHashVerified, supportsWebTransport } from "./session.js";
export { ENCODER_PRESETS } from "./video-tx.js";
export { clampMaxJitterBufferMs, clampCatchUpSpeedPercent, DEFAULT_MAX_JITTER_BUFFER_MS, DEFAULT_CATCH_UP_SPEED_PERCENT } from "./jitter.js";
