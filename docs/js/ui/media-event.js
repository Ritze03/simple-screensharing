// A watch's "media" event from the core (VideoReceiver.onEvent) -> the watch's UI state. Pure so dev/tests.js can run it.
//
// Why a "codec" event clears the error (user report 2026-10-15, #87): the receiver announces "codec" whenever a keyframe
// reveals a different codec string, and only afterwards (async, isConfigSupported) "unsupported" if this browser cannot
// decode it. The tile's "This browser can't decode H.265" used to stay forever after the sharer switched back to H.264.
// A new codec string means the old verdict is stale; if the new codec is unsupported too, "unsupported" follows at once.

/** The Connection log's line for a media event, null for one it does not show. A phone has only this screen, so one decoder failure is one line with all that is known about it. */
export function mediaLogLine(d) {
  if (d.type === "need-keyframe") return `need-keyframe (${d.reason === "gap" ? `gap ${d.from}->${d.to}, id +${d.idStep ?? "?"}${d.ms != null ? `, unrepaired ${d.ms}ms` : ""}, dropin=${d.dropIn ?? "?"}` : d.reason || "?"})`;
  if (d.type === "decoder-mode" && d.hardwareAcceleration) return `decoder-mode (${d.hardwareAcceleration})`;
  if (d.type === "repaired") return `repaired (${d.from}->${d.to}, id +${d.idStep}, ${d.ms}ms, ${d.rounds} rounds, ${d.bytes} B, ${d.repaired}/${d.gaps}, dropin=${d.dropIn ?? "?"})`;
  if (d.type !== "decoder-error") return null;
  if (d.codecString === undefined) return `decoder-error (${d.message || "?"})`; // audio
  return `decoder-error #${d.count} (${d.name || "?"}: ${d.message || "?"}) ${d.codecString} ${d.width}x${d.height} hw=${d.hardwareAcceleration} q=${d.queue} key=${d.keyAgoMs ?? "?"}ms/${d.sinceKey}f break=${d.sinceBreak === null ? "none" : `${d.sinceBreak}f`} out=${d.outputs} gaps=${d.gaps} chain=${d.chain} ptsback=${d.ptsBack} stale=${d.stale} repaired=${d.repaired}`;
}

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
