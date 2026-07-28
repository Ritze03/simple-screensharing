// tile-video.js — shared WebCodecs decode pipeline for a single stream tile.
//
// Extracted from view.js (see git history) so the standalone viewer window
// (viewer.js/viewer.html) can drive the exact same fetch+parse+decode+jitter
// pipeline against its own grid of `<canvas>` elements, independent of how
// the tile is laid out in the DOM. Everything here operates purely on a
// `{ id, videoUrl, ... }`-shaped tile object and a `<canvas>` — it knows
// nothing about grids, FPS `<span>` DOM wiring, or tab rendering; that stays
// each caller's own concern (see view.js's `fpsSpans`/`updateStats` and the
// new viewer.js for two different callers of the same module).
//
// Plain `<script>`-tag module convention (matches every other file in this
// directory) — exposed as `window.TileVideo`, no ES module system in use
// here.
//
// See `src-tauri/src/video_server.rs`'s module doc for the raw
// H.264-over-HTTP wire format this pipeline parses.

(function () {
  "use strict";

  // Maps tile id (`${room_handle}:${owner}:${stream_id}`) -> that tile's live
  // video controller: `{ decoder, reader, stopped, ... }`. One entry per
  // currently-started real tile, created by `startTileVideo` and torn down
  // by `stopTileVideo`/`stopAllTileVideo`.
  let videoControllers = new Map();

  // How far back a controller's `frameTimestamps` window looks when
  // computing its received-fps — mirrors the spirit of `src/capture.rs`'s
  // `ENCODER_FPS_WINDOW` (a rolling window smooths out per-frame jitter
  // better than a naive last-interval delta), reimplemented fresh in JS
  // here since there's no way to share the Rust code across the process
  // boundary.
  const FPS_WINDOW_MS = 1500;
  // Cadence for pushing the locally-computed fps into the DOM — matches
  // `stream-stats-updated`'s ~4x/sec (250ms) cadence purely for visual
  // consistency; this counter no longer has anything to do with that event.
  const FPS_UPDATE_INTERVAL_MS = 250;

  // --- Jitter buffer (RFC 3550-style EWMA over wire-format `pts_us`) ---
  //
  // Ceiling on the applied playout delay. Defaults to this module's own
  // default and is overwritten once at startup by whichever page embeds
  // this module (`view.js`/`viewer.js`) from the real `get_settings()`
  // value, via `window.TileVideo.setMaxJitterBufferMs` — see that function
  // below. Every read below goes through this one `let` binding rather than
  // a literal, which is what makes that a one-line external call instead of
  // a hunt through the jitter math for inlined `2000`s.
  const DEFAULT_MAX_JITTER_BUFFER_MS = 2000;
  let maxJitterBufferMs = DEFAULT_MAX_JITTER_BUFFER_MS;
  // Floor applied to the target buffer even when the jitter estimate is
  // (near-)zero, e.g. right at stream start — RFC 3550's own jitter
  // estimator has no such floor, but a few tens of ms of slack absorbs the
  // ordinary scheduling noise of `setTimeout`/decode itself rather than
  // starting the buffer at literally 0ms.
  const JITTER_BUFFER_FLOOR_MS = 40;
  // RFC 3550 section 6.4.1's smoothing constant (1/16) for the jitter EWMA.
  const JITTER_EWMA_ALPHA = 1 / 16;
  // Max change to `appliedBufferMs` per chunk arrival — caps how fast the
  // applied delay can move even if `target_buffer_ms` jumps a lot in one
  // step, so buffer growth/shrink is an invisible drift rather than a
  // visible pacing jump in the playout schedule.
  const MAX_BUFFER_EASE_STEP_MS = 20;

  function clamp(value, lo, hi) {
    return Math.max(lo, Math.min(hi, value));
  }

  /** RFC 3550 6.4.1-style jitter EWMA update, fed by each VideoFrame
   * chunk's *arrival* time and the wire format's `pts_us` (the sender's
   * capture-time PTS) — not by decode or draw time, which this buffer
   * deliberately delays. `controller.lastArrivalMs`/`lastPtsUs` carry the
   * previous chunk's readings across calls; the very first chunk on a
   * connection has no previous sample to diff against and is skipped (this
   * function still seeds `lastArrivalMs`/`lastPtsUs` for it). A chunk that
   * arrives very late (e.g. after a real buffer underrun) naturally
   * produces a large `deviation` here — arrival delta far outpacing the pts
   * delta — so a stall feeds the *next* target buffer calculation without
   * any separate "detect underrun" branch. */
  function updateJitterEstimate(controller, arrivalMs, ptsUs) {
    if (controller.lastArrivalMs != null) {
      const deviation = (arrivalMs - controller.lastArrivalMs) - (ptsUs - controller.lastPtsUs) / 1000;
      controller.jitterEwmaMs += (Math.abs(deviation) - controller.jitterEwmaMs) * JITTER_EWMA_ALPHA;
    }
    controller.lastArrivalMs = arrivalMs;
    controller.lastPtsUs = ptsUs;
  }

  /** Eases `controller.appliedBufferMs` toward the jitter-derived target by
   * at most `MAX_BUFFER_EASE_STEP_MS`, rather than snapping straight to the
   * target — see `MAX_BUFFER_EASE_STEP_MS`'s comment. Call once per chunk
   * arrival, after `updateJitterEstimate` has folded in that chunk's
   * sample. */
  function updateAppliedBuffer(controller) {
    const target = clamp(controller.jitterEwmaMs * 4 + JITTER_BUFFER_FLOOR_MS, 0, maxJitterBufferMs);
    const diff = target - controller.appliedBufferMs;
    controller.appliedBufferMs += clamp(diff, -MAX_BUFFER_EASE_STEP_MS, MAX_BUFFER_EASE_STEP_MS);
  }

  /** Pops and decodes `controller.pendingChunks[0]` once its scheduled
   * playout time arrives, then reschedules itself for whatever's next in
   * the queue. A no-op (does not set a timeout) whenever the queue is
   * currently empty — the next `enqueueChunk` call kicks scheduling back
   * off, so an empty queue just means nothing is pending, not a stuck
   * scheduler. Guarded by `controller.scheduleTimeoutId` so at most one
   * timeout is ever outstanding per controller; `stopController` clears
   * that id (see its doc comment for why teardown here follows the same
   * own-controller-only discipline as the fetch/decoder cleanup). */
  function maybeScheduleNext(controller, decoder, tile) {
    if (controller.stopped) return;
    if (controller.scheduleTimeoutId != null) return;
    if (controller.pendingChunks.length === 0) return;

    const item = controller.pendingChunks[0];
    const delay = Math.max(0, item.decodeAtMs - performance.now());
    controller.scheduleTimeoutId = setTimeout(() => {
      controller.scheduleTimeoutId = null;
      if (controller.stopped) return;
      controller.pendingChunks.shift();
      if (decoder.state === "configured") {
        try {
          decoder.decode(item.chunk);
        } catch (e) {
          // Same reasoning as the pre-buffering `decode()` call site this
          // replaced: a single bad access unit shouldn't kill the whole
          // tile — log and keep going, a sustained problem surfaces via the
          // decoder's own `error` callback.
          console.error(`[tile-video] VideoDecoder.decode() failed for ${tile.id}:`, e);
        }
      }
      maybeScheduleNext(controller, decoder, tile);
    }, delay);
  }

  /** Queues one already-parsed VideoFrame chunk for delayed decode instead
   * of decoding it immediately — see `startTileVideoInner`'s doc comment
   * and the module header's jitter-buffer note for why the delay lives
   * here (buffering cheap still-encoded chunk data) rather than after
   * decode (buffering ~3MB `VideoFrame`s). Anchors the playout schedule on
   * the very first chunk (post-keyframe/post-CodecInfo) it sees for this
   * controller; every later chunk's target decode time is anchored off that
   * same `(anchorArrivalMs, anchorPtsUs)` pair plus the current eased
   * buffer. */
  function enqueueChunk(controller, decoder, tile, ptsUs, chunk) {
    const arrivalMs = performance.now();
    updateJitterEstimate(controller, arrivalMs, ptsUs);
    updateAppliedBuffer(controller);

    if (controller.anchorArrivalMs == null) {
      controller.anchorArrivalMs = arrivalMs;
      controller.anchorPtsUs = ptsUs;
    }
    const decodeAtMs = controller.anchorArrivalMs + (ptsUs - controller.anchorPtsUs) / 1000 + controller.appliedBufferMs;

    controller.pendingChunks.push({ decodeAtMs, chunk });
    maybeScheduleNext(controller, decoder, tile);
  }

  /** Records one decoded-frame timestamp into `controller.frameTimestamps`
   * and drops anything older than `FPS_WINDOW_MS`, so the array stays
   * bounded no matter how long the tile has been playing. Called once per
   * real decoded frame from `decoder.output` in `startTileVideoInner`. */
  function recordFrame(controller) {
    const now = performance.now();
    controller.frameTimestamps.push(now);
    const cutoff = now - FPS_WINDOW_MS;
    while (controller.frameTimestamps.length && controller.frameTimestamps[0] < cutoff) {
      controller.frameTimestamps.shift();
    }
  }

  /** Frames-per-second implied by `controller.frameTimestamps` over
   * whatever span has actually elapsed since the oldest timestamp still in
   * the window (shorter than `FPS_WINDOW_MS` right after a tile starts, so
   * early readings aren't inflated by dividing by the full window). */
  function currentFps(controller) {
    const timestamps = controller.frameTimestamps;
    if (timestamps.length < 2) return 0;
    const spanMs = performance.now() - timestamps[0];
    if (spanMs <= 0) return 0;
    return (timestamps.length - 1) / (spanMs / 1000);
  }

  /** Cancels `controller`'s fetch reader and closes its `VideoDecoder`.
   * Operates on the controller object itself, never by looking it back up
   * in `videoControllers` by id — a tile can be re-watched (new fetch, new
   * decoder, same id) while the *previous* controller's read loop is still
   * unwinding from its own cancellation, and that stale loop's `finally`
   * must only ever tear down the resources it itself owns, never whatever
   * newer controller now happens to sit at the same id (that bug shipped
   * once already: a decoder `error` callback for a controller already
   * superseded by a newer one closed the *new* decoder instead, so an
   * error on the old stream silently killed the next stream's video before
   * it ever drew a frame). Safe to call more than once (e.g. from both a
   * decoder `error` callback and the read loop's own `finally`) —
   * `stopped` guards double-close and stops the read loop from feeding a
   * decoder that's already closing. */
  function stopController(controller) {
    if (!controller) return;
    controller.stopped = true;
    if (controller.fpsIntervalId != null) {
      clearInterval(controller.fpsIntervalId);
      controller.fpsIntervalId = null;
    }
    // Same class of bug this file has already hit once (see this
    // function's own doc comment above): a scheduled `decode()` firing
    // after teardown would feed a closing/closed decoder. Clearing the
    // timeout here — the only place a pending decode is ever scheduled —
    // and dropping the queue means no `maybeScheduleNext` callback can fire
    // for this controller again after this point.
    if (controller.scheduleTimeoutId != null) {
      clearTimeout(controller.scheduleTimeoutId);
      controller.scheduleTimeoutId = null;
    }
    controller.pendingChunks.length = 0;
    // Aborts the fetch outright if it's still in flight (no reader yet —
    // headers haven't come back), and is a harmless no-op if the fetch has
    // already settled by the time this runs.
    if (controller.abortController) {
      controller.abortController.abort();
    }
    if (controller.reader) {
      controller.reader.cancel().catch(() => {});
    }
    if (controller.decoder && controller.decoder.state !== "closed") {
      try {
        controller.decoder.close();
      } catch (e) {
        console.error("[tile-video] VideoDecoder.close() failed:", e);
      }
    }
  }

  /** Stops and forgets whatever controller is currently registered for
   * `id` — used for real teardown (tile removed, full re-render) where the
   * caller genuinely means "whatever's live for this id right now", unlike
   * `stopController` above. */
  function stopTileVideo(id) {
    const controller = videoControllers.get(id);
    if (!controller) return;
    videoControllers.delete(id);
    stopController(controller);
  }

  function stopAllTileVideo() {
    for (const id of Array.from(videoControllers.keys())) stopTileVideo(id);
  }

  /** True iff `id` currently has a live video controller — lets a caller
   * (e.g. view.js's `updateStats`) avoid fighting this module's own
   * fps-`<span>`-writing interval over the same DOM node. */
  function hasController(id) {
    return videoControllers.has(id);
  }

  /** Fetches `tile.videoUrl` (see `src-tauri/src/video_server.rs`'s module
   * doc for the wire format), parses the length-prefixed chunk stream
   * (buffering across `read()` calls since a chunk can straddle two network
   * reads), configures a `VideoDecoder` from the first CodecInfo chunk, and
   * feeds every VideoFrame chunk through the jitter buffer above
   * (`enqueueChunk`/`maybeScheduleNext`) rather than calling
   * `decoder.decode()` on it immediately — see the "Jitter buffer" block
   * for why the delay sits before decode (buffering cheap still-encoded
   * chunk data) rather than after. Decoded frames are still drawn directly
   * in `decoder.output` the moment each delayed `decode()` call produces
   * one (the simplest correct pattern here — the decoder itself already
   * paces output, so no separate rAF loop is needed) and immediately
   * `close()`d to release their underlying resources — that part is
   * unchanged. `errorPlaceholder` is revealed (and the canvas left showing
   * its last frame underneath) if the decoder enters its `error` state,
   * matching the existing `'stalled'` `.media-tile-placeholder` visual
   * rather than inventing a new one.
   *
   * `tile` needs only `{ id, videoUrl }` — callers may attach whatever
   * other fields they like (view.js's tiles carry room/owner/label info,
   * viewer.js's don't), nothing here reads beyond those two. */
  async function startTileVideo(tile, canvas, errorPlaceholder) {
    const controller = {
      decoder: null,
      reader: null,
      stopped: false,
      abortController: new AbortController(),
      // Rolling window of `performance.now()` timestamps, one per real
      // decoded frame — see `recordFrame`/`currentFps` above.
      frameTimestamps: [],
      fpsIntervalId: null,
      // Jitter buffer state — see the "Jitter buffer" block above for the
      // constants/helpers these feed. `pendingChunks` holds still-encoded
      // `{ decodeAtMs, chunk: EncodedVideoChunk }` entries (cheap, KBs
      // each), never decoded `VideoFrame`s (~3MB each at 1080p) — decode
      // itself still happens right before draw, just delayed until each
      // chunk's scheduled time via `maybeScheduleNext`'s `setTimeout`.
      pendingChunks: [],
      jitterEwmaMs: 0,
      appliedBufferMs: 0,
      anchorArrivalMs: null,
      anchorPtsUs: null,
      lastArrivalMs: null,
      lastPtsUs: null,
      scheduleTimeoutId: null,
    };
    videoControllers.set(tile.id, controller);

    // Owns the fps span for this tile exclusively from here on — see
    // `hasController` above for why a caller's own stats-update path should
    // skip tiles with an active controller rather than racing this interval
    // to write the same DOM node. `onFps`, if provided, is this module's
    // only DOM-touching hook — everything else here is layout-agnostic.
    controller.fpsIntervalId = setInterval(() => {
      if (tile.onFps) tile.onFps(currentFps(controller));
    }, FPS_UPDATE_INTERVAL_MS);

    const showError = () => {
      if (errorPlaceholder) errorPlaceholder.removeAttribute("hidden");
    };

    // Every exit path below (including the two early `return`s right after
    // this) funnels through this `finally`, so a fetch failure can't leak
    // this controller in `videoControllers` forever — see `stopController`'s
    // doc comment for why this is own-controller cleanup, not a
    // `stopTileVideo(tile.id)` map lookup.
    try {
      await startTileVideoInner(tile, canvas, controller, showError);
    } finally {
      stopController(controller);
      if (videoControllers.get(tile.id) === controller) videoControllers.delete(tile.id);
    }
  }

  async function startTileVideoInner(tile, canvas, controller, showError) {
    let response;
    try {
      response = await fetch(tile.videoUrl, { signal: controller.abortController.signal });
    } catch (e) {
      // A stop-triggered abort() rejects fetch() with an AbortError — that's
      // expected teardown, not a real failure, so don't flash the error
      // placeholder for it.
      if (controller.stopped) return;
      console.error(`[tile-video] failed to fetch video stream for ${tile.id}:`, e);
      showError();
      return;
    }
    if (controller.stopped) {
      // Stopped between the fetch resolving and this check (no reader
      // exists yet for `stopController` to `cancel()` on) — release the
      // now-unread response body ourselves so the connection doesn't idle.
      if (response.body) response.body.cancel().catch(() => {});
      return;
    }
    if (!response.ok || !response.body) {
      console.error(`[tile-video] video stream fetch for ${tile.id} failed: HTTP ${response.status}`);
      showError();
      return;
    }

    const reader = response.body.getReader();
    controller.reader = reader;

    const decoder = new VideoDecoder({
      output: (frame) => {
        if (controller.stopped) {
          frame.close();
          return;
        }
        // `output` still fires (and draws) immediately once the delayed
        // `decoder.decode()` call above actually runs — the jitter buffer
        // only moves *when* decode is called, not what happens after — so
        // `recordFrame` here measures true display-time fps rather than
        // network-arrival fps.
        recordFrame(controller);
        if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
        }
        canvas.getContext("2d").drawImage(frame, 0, 0, canvas.width, canvas.height);
        frame.close();
      },
      error: (e) => {
        console.error(`[tile-video] VideoDecoder error for ${tile.id}:`, e);
        // `stopController` (own-controller only, see its doc comment) —
        // this callback can fire after a newer controller has already
        // taken over `tile.id` (e.g. this decoder was mid-error right as
        // the tile got re-rendered), and must not reach into the map and
        // close that newer one instead. `startTileVideo`'s outer `finally`
        // handles removing this controller from the map once the read
        // loop this unblocks actually exits.
        stopController(controller);
        showError();
      },
    });
    controller.decoder = decoder;

    // Buffers bytes across `read()` calls: the underlying TCP stream has no
    // relationship to chunk boundaries, so a single `read()` can yield a
    // partial chunk, several whole chunks, or a whole chunk plus part of
    // the next one.
    let buf = new Uint8Array(0);

    try {
      while (!controller.stopped) {
        const { done, value } = await reader.read();
        if (done) break;
        if (value && value.length) {
          const merged = new Uint8Array(buf.length + value.length);
          merged.set(buf, 0);
          merged.set(value, buf.length);
          buf = merged;
        }

        // Drain every complete chunk currently buffered.
        while (buf.length >= 4) {
          const view = new DataView(buf.buffer, buf.byteOffset, buf.length);
          const bodyLen = view.getUint32(0, false);
          const totalLen = 4 + bodyLen;
          if (buf.length < totalLen) break; // chunk not fully buffered yet

          const frameType = buf[4];
          const flags = buf[5];
          const ptsUs = view.getBigUint64(6, false);
          const payload = buf.subarray(14, totalLen);

          if (frameType === 0) {
            // CodecInfo — always the first chunk of the connection.
            const codec = new TextDecoder().decode(payload);
            try {
              decoder.configure({ codec, optimizeForLatency: true });
            } catch (e) {
              console.error(`[tile-video] VideoDecoder.configure({codec: "${codec}"}) failed for ${tile.id}:`, e);
              showError();
            }
          } else if (frameType === 1 && decoder.state === "configured") {
            // Parsing/construction happens immediately on arrival as
            // before; only the `decoder.decode()` call itself is delayed —
            // see `enqueueChunk`/`maybeScheduleNext` above for the jitter
            // buffer this now goes through instead of decoding inline.
            const chunk = new EncodedVideoChunk({
              type: flags & 0x01 ? "key" : "delta",
              timestamp: Number(ptsUs),
              data: payload,
            });
            enqueueChunk(controller, decoder, tile, Number(ptsUs), chunk);
          }

          buf = buf.slice(totalLen);
        }
      }
    } catch (e) {
      if (!controller.stopped) {
        console.error(`[tile-video] video stream for ${tile.id} ended with an error:`, e);
        showError();
      }
    }
  }

  /** Overrides the ceiling on applied jitter-buffer delay (see the "Jitter
   * buffer" block's `maxJitterBufferMs` doc comment above) — called once at
   * startup by whichever page embeds this module, with the user's real
   * `get_settings()` value, so both `view.js` and `viewer.js` share one
   * setting without either needing direct access to this module's private
   * state. Clamped defensively (the backend already clamps too). */
  function setMaxJitterBufferMs(ms) {
    maxJitterBufferMs = clamp(Number(ms) || DEFAULT_MAX_JITTER_BUFFER_MS, 0, 5000);
  }

  window.TileVideo = {
    startTileVideo,
    stopTileVideo,
    stopAllTileVideo,
    hasController,
    setMaxJitterBufferMs,
  };
})();
