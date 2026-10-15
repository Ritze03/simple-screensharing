// A watch's "media" event from the core (VideoReceiver.onEvent) -> the watch's UI state. Pure so dev/tests.js can run it.
//
// Why a "codec" event clears the error (user report 2026-10-15, #87): the receiver announces "codec" whenever a keyframe
// reveals a different codec string, and only afterwards (async, isConfigSupported) "unsupported" if this browser cannot
// decode it. The tile's "This browser can't decode H.265" used to stay forever after the sharer switched back to H.264.
// A new codec string means the old verdict is stale; if the new codec is unsupported too, "unsupported" follows at once.

/** @param w the watch (`{ codec, error }`); @param d the event detail `{ type, codec? }`; @param tr `(codecUpper) => message`. Returns true if `w` changed. */
export function applyMediaEvent(w, d, tr) {
  if (d.type === "codec") {
    w.codec = d.codec || w.codec;
    w.error = null;
    return true;
  }
  if (d.type === "unsupported") {
    w.error = tr(String(d.codec || "").toUpperCase() || "?");
    return true;
  }
  return false;
}
