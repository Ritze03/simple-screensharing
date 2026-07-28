// view.js — View tab content: the auto-tiled grid of watched streams.
//
// Mock/real fallback pattern (see `superdoc/ui/FRONTEND-SHELL.md`): every
// data touchpoint is an `async function` with a real `invoke(...)` call on
// one branch and a mock-state fallback on the other.
//
// There's no single `list_watched_streams` command (no DTO/command in
// `commands.rs`/`dto.rs` describes "every watched stream across every
// room" directly) — the real branch below builds it itself: `list_saved_rooms`
// for the joined rooms, then `room_detail` per room, joining that room's
// `watched` (`StreamKey[]`) against its `streams` (`StreamInfo[]`, for
// resolution/kind) and `members` (`Member[]`, for the display name). This
// mirrors the same join `shell.js`'s `loadRoomMembers` and
// `stream-control.js`'s room-scoped Members card do elsewhere in this
// frontend for the same reason (no DTO inlines the cross-room view this tab
// needs).
//
// Real video: each real tile's `<canvas>` is driven by a WebCodecs
// `VideoDecoder` fed from `commands::stream_video_url` (falls back to
// `null`/the placeholder gradient until the producer thread has actually
// started, which is effectively immediate). See
// `src-tauri/src/video_server.rs`'s module doc for the raw H.264-over-HTTP
// wire format this URL now serves (it replaced the old MJPEG-over-HTTP
// delivery this file used to just point an `<img>` at) and this file's
// `startTileVideo`/`videoControllers` below for the client-side
// parse-chunks + decode + render pipeline.
//
// Known gap: no DTO carries a per-watched-stream "is this actually
// decoding right now" signal, so every real tile still renders in the
// `'live'` visual state regardless of whether frames are actually flowing;
// the `'stalled'` placeholder is only reachable via the mock branch's demo
// toggle until a later worker wires a real signal for it. A `VideoDecoder`
// entering its `error` state *is* surfaced (see `startTileVideo`), reusing
// the same `.media-tile-placeholder` visual the `'stalled'` state uses.

(function () {
  "use strict";

  const { el, icon, renderEmptyState } = window.Components;

  const MOCK_WATCHED = [
    { id: "w1", ownerLabel: "Ana", roomLabel: "Movie Night", resolution: "1920x1080", fps: 30, state: "live", hue: 220 },
    { id: "w2", ownerLabel: "Priya", roomLabel: "Book Club", resolution: "1280x720", fps: 60, state: "live", hue: 160 },
    { id: "w3", ownerLabel: "Sam", roomLabel: "Gaming Night", resolution: "1920x1080", fps: null, state: "stalled", hue: 0 },
  ];

  let mockWatched = [];

  // Maps tile id (`${room_handle}:${owner}:${stream_id}`) -> that tile's fps
  // `<span>`, rebuilt on every `render()`. `stream-stats-updated` fires
  // unconditionally ~4x/sec (see `dto::StreamStatsUpdated`'s doc comment),
  // so `updateStats` below patches just this span's text in place rather
  // than re-rendering the tile, to avoid flicker.
  let fpsSpans = new Map();

  // Maps tile id (same `${room_handle}:${owner}:${stream_id}` scheme as
  // `fpsSpans`) -> that tile's live video controller: `{ decoder, reader,
  // stopped }`. One entry per currently-watched real tile, created by
  // `startTileVideo` when a tile with a `videoUrl` is rendered and torn
  // down by `stopTileVideo`/`stopAllTileVideo` (see `render()`'s "full
  // rebuild" comment — every render() tears down and recreates every
  // controller, same lifecycle `fpsSpans` already follows).
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
  // Ceiling on the applied playout delay. Defaults to the backend's own
  // default (`rooms::AppSettings::default_max_jitter_buffer_ms`) and is
  // overwritten once during `init()` from the real `get_settings()` value —
  // see `fetchMaxJitterBufferMs`. Every read below goes through this one
  // `let` binding rather than a literal, which is what made wiring the real
  // setting in a one-line change to `init()` instead of a hunt through the
  // jitter math for inlined `2000`s.
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
          console.error(`[view] VideoDecoder.decode() failed for ${tile.id}:`, e);
        }
      }
      maybeScheduleNext(controller, decoder, tile);
    }, delay);
  }

  /** Queues one already-parsed VideoFrame chunk for delayed decode instead
   * of decoding it immediately — see this file's `startTileVideoInner` doc
   * comment and the module header's jitter-buffer note for why the delay
   * lives here (buffering cheap still-encoded chunk data) rather than after
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
        console.error("[view] VideoDecoder.close() failed:", e);
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
   * rather than inventing a new one. */
  async function startTileVideo(tile, canvas, errorPlaceholder) {
    const controller = {
      decoder: null,
      reader: null,
      stopped: false,
      abortController: new AbortController(),
      // Rolling window of `performance.now()` timestamps, one per real
      // decoded frame — see `recordFrame`/`currentFps` above. This is the
      // "received fps" the old `dto::StreamStatsUpdated`-driven counter
      // used to report before the H.264-relay redesign silently broke it
      // (see this file's header note); WebCodecs' own `decoder.output`
      // firing once per genuinely decoded frame makes this tab the actual
      // source of truth now, not the server.
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
    // `updateStats` below for why it skips tiles with an active controller
    // rather than racing this interval to write the same DOM node.
    controller.fpsIntervalId = setInterval(() => {
      const span = fpsSpans.get(tile.id);
      if (!span) return;
      span.textContent = `@${Math.round(currentFps(controller))}fps`;
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
      console.error(`[view] failed to fetch video stream for ${tile.id}:`, e);
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
      console.error(`[view] video stream fetch for ${tile.id} failed: HTTP ${response.status}`);
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
        // `recordFrame` here now measures true display-time fps rather
        // than network-arrival fps, a strict improvement over the old
        // decode-immediately behavior with no code change needed.
        recordFrame(controller);
        if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
          canvas.width = frame.displayWidth;
          canvas.height = frame.displayHeight;
        }
        canvas.getContext("2d").drawImage(frame, 0, 0, canvas.width, canvas.height);
        frame.close();
      },
      error: (e) => {
        console.error(`[view] VideoDecoder error for ${tile.id}:`, e);
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
              console.error(`[view] VideoDecoder.configure({codec: "${codec}"}) failed for ${tile.id}:`, e);
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
        console.error(`[view] video stream for ${tile.id} ended with an error:`, e);
        showError();
      }
    }
  }

  async function fetchWatchedStreams() {
    if (window.__TAURI__) {
      const rooms = await window.__TAURI__.core.invoke("list_saved_rooms");
      const joined = rooms.filter((r) => r.state === "joined" && r.room_handle != null);
      const tiles = [];
      for (const room of joined) {
        const detail = await window.__TAURI__.core.invoke("room_detail", { roomHandle: room.room_handle });
        if (!detail) continue;
        for (const key of detail.watched) {
          const streamInfo = detail.streams.find((s) => s.owner === key.owner && s.stream_id === key.stream_id);
          // `detail.watched` and `detail.streams` are pruned independently
          // server-side (see `src-tauri/src/driver.rs`'s "3.5. Prune
          // watches..." step) — that prune runs on the same ~250ms tick as
          // the `presence-updated` this file's `refresh()` reacts to, so in
          // practice `watched` never outlives `streams` by more than one
          // tick, but a defensive skip here means this tab never renders a
          // tile for a stream that's already known to be gone, even in that
          // brief window, rather than trusting the backend prune alone.
          if (!streamInfo) continue;
          const member = detail.members.find((m) => m.conn_id === key.owner);
          const videoUrl = await window.__TAURI__.core.invoke("stream_video_url", {
            roomHandle: room.room_handle,
            owner: key.owner,
            streamId: key.stream_id,
          });
          tiles.push({
            id: `${room.room_handle}:${key.owner}:${key.stream_id}`,
            room_handle: room.room_handle,
            owner: key.owner,
            stream_id: key.stream_id,
            ownerLabel: member ? member.display_name : String(key.owner),
            roomLabel: room.name,
            resolution: streamInfo ? `${streamInfo.width}x${streamInfo.height}` : null,
            fps: null,
            videoUrl,
            // See this file's header note: no real per-stream decode-health
            // signal is wired up yet, so every real tile is 'live'.
            state: "live",
            // `key.owner` is a lowercase-hex `ConnId` string (no `0x`
            // prefix — see `dto::hex_u64`'s doc comment for why it's a
            // string at all, not a JS `Number`), so it needs the prefix
            // added back before `BigInt` can parse it.
            hue: Number(BigInt("0x" + key.owner) % 360n),
          });
        }
      }
      return tiles;
    }
    return MOCK_WATCHED.slice();
  }

  async function popOutWatch(tile) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("pop_out_stream", { roomHandle: tile.room_handle, owner: tile.owner, streamId: tile.stream_id });
      return;
    }
    // Mock: no real second OS window in a plain browser; flag it locally so
    // a Playwright test can assert the click was registered.
    const t = mockWatched.find((x) => x.id === tile.id);
    if (t) t.poppedOut = true;
  }

  function render() {
    const root = document.getElementById("view-root");
    if (!root) return;
    root.innerHTML = "";

    const controls = el("div", { className: "view-controls" });
    if (!window.__TAURI__) {
      controls.appendChild(
        el("button", {
          className: "view-demo-toggle",
          text: mockWatched.length === 0 ? "dev: restore watched-streams demo state" : "dev: clear watched streams (show empty state)",
          onClick: () => {
            mockWatched = mockWatched.length === 0 ? MOCK_WATCHED.slice() : [];
            render();
          },
        })
      );
    }
    root.appendChild(controls);

    // Full rebuild — every fps span reference and every video controller
    // from the previous render is now detached/stale, so tear down all
    // running fetch+decoder pipelines and start the fps map fresh before
    // deciding what to render next. Must happen before the empty-state
    // early return too, otherwise clearing the last watched tile would
    // leak that tile's reader/decoder.
    fpsSpans = new Map();
    stopAllTileVideo();

    if (mockWatched.length === 0) {
      root.appendChild(renderEmptyState("Nobody's on screen right now — pick someone from a room in the sidebar and click Watch.", null, null));
      return;
    }

    const grid = el("div", { className: "view-grid" });
    mockWatched.forEach((tile) => grid.appendChild(renderTile(tile)));
    root.appendChild(grid);
  }

  function renderTile(tile) {
    const wrap = el("div", { className: "media-tile" });

    if (tile.state === "stalled") {
      const placeholder = el("div", { className: "media-tile-placeholder" });
      placeholder.appendChild(el("div", { className: "media-tile-spinner" }));
      placeholder.appendChild(el("span", { text: "Waiting for capture…" }));
      wrap.appendChild(placeholder);
    } else if (tile.videoUrl) {
      // Real video: a `<canvas>` driven by a WebCodecs `VideoDecoder`
      // pulling from this stream's raw H.264-over-HTTP URL (see
      // `src-tauri/src/video_server.rs` for the wire format). Unlike the
      // old MJPEG `<img>`, decoding is genuinely our job now — see
      // `startTileVideo` for the fetch/parse/decode/draw pipeline. The
      // sibling placeholder starts hidden and is only revealed if
      // `startTileVideo` hits a fetch or `VideoDecoder` error, reusing the
      // same `.media-tile-placeholder` visual the `'stalled'` state above
      // uses rather than inventing a new error look.
      const canvas = el("canvas", { className: "media-tile-video", attrs: { role: "img", "aria-label": tile.ownerLabel } });
      const errorPlaceholder = el("div", { className: "media-tile-placeholder", attrs: { hidden: "hidden" } });
      errorPlaceholder.appendChild(el("span", { text: "Stream error" }));
      wrap.appendChild(canvas);
      wrap.appendChild(errorPlaceholder);
      startTileVideo(tile, canvas, errorPlaceholder);
    } else {
      // Mock "video" (no `window.__TAURI__`, so no real MJPEG URL exists) —
      // a gradient standing in for a decoded frame, so a plain-browser
      // Playwright run still has something visually distinct per tile.
      wrap.appendChild(
        el("div", {
          className: "media-tile-mock-frame",
          attrs: {
            style: `background: linear-gradient(135deg, hsl(${tile.hue} 45% 22%), hsl(${(tile.hue + 40) % 360} 45% 12%));`,
          },
        })
      );
    }

    const top = el("div", { className: "media-tile-overlay-top" });
    const baseLabel = tile.resolution ? `${tile.ownerLabel} · ${tile.roomLabel} · ${tile.resolution}` : `${tile.ownerLabel} · ${tile.roomLabel}`;
    const label = el("span", { className: "media-tile-label", text: baseLabel + " " });
    // Separate `<span>` for the fps part so `updateStats` (below, fed by the
    // `stream-stats-updated` event) can patch just this text node ~4x/sec
    // without touching (and re-flickering) the rest of the tile.
    const fpsSpan = el("span", { className: "media-tile-fps", text: tile.fps ? `@${tile.fps}fps` : "" });
    label.appendChild(fpsSpan);
    top.appendChild(label);
    if (tile.id) fpsSpans.set(tile.id, fpsSpan);
    top.appendChild(
      el(
        "button",
        {
          className: "media-tile-popout-btn",
          attrs: { title: "Pop out", "aria-label": "Pop out" },
          onClick: () => popOutWatch(tile),
        },
        [icon("open_in_new")]
      )
    );
    wrap.appendChild(top);

    return wrap;
  }

  /** Fetches the user's `max_jitter_buffer_ms` preference once at startup
   * (see the "Jitter buffer" block's `maxJitterBufferMs` doc comment above)
   * and assigns it into that module-level binding so every later jitter
   * calculation picks it up. No-op on the mock branch (no `window.__TAURI__`)
   * — `maxJitterBufferMs` just keeps its `DEFAULT_MAX_JITTER_BUFFER_MS`
   * starting value, mirroring `settings.js`'s `fetchSettings` mock fallback. */
  async function fetchMaxJitterBufferMs() {
    if (!window.__TAURI__) return;
    const settings = await window.__TAURI__.core.invoke("get_settings");
    maxJitterBufferMs = settings.max_jitter_buffer_ms;
  }

  async function init() {
    await fetchMaxJitterBufferMs();
    mockWatched = await fetchWatchedStreams();
    render();
  }

  /** Fed by `shell.js`'s `stream-stats-updated` listener (~4x/sec,
   * unconditional — see `dto::StreamStatsUpdated`'s doc comment). Only
   * `payload.watched` is this tab's concern (`payload.destinations` is
   * `stream-control.js`'s). Matches each entry to a rendered tile by
   * `room_handle:owner:stream_id` (same id scheme `fetchWatchedStreams`
   * already uses) and patches that tile's fps span text in place — no
   * re-render, so no flicker. A tile with no matching entry (timing edge
   * case, or the stream just stopped) is simply left showing its last
   * value; real removal is handled elsewhere via `refresh()`.
   *
   * `payload.watched[].received_fps` is permanently 0 for any tile with a
   * live WebCodecs pipeline (see this file's header note and
   * `startTileVideo`'s `frameTimestamps`/`recordFrame`/`currentFps` for
   * why: the server-side counter this event used to report stopped being
   * fed anything the moment the H.264-relay redesign stopped calling
   * `StreamDecoder::latest_frame()`). Rather than let this handler and
   * `startTileVideo`'s own `setInterval` both write the same `<span>` on
   * different cadences (flicker, and a race over which value "wins" on any
   * given tick), a tile with an active `videoControllers` entry is skipped
   * here entirely — that interval owns the span exclusively for as long as
   * the controller is alive. This branch only still matters for the mock
   * branch (`window.__TAURI__` absent, so `mockWatched` entries carry a
   * static `fps` and never get a real controller) and for the brief window
   * before a tile's `startTileVideo` call has set up its own interval. */
  function updateStats(payload) {
    if (!payload || !Array.isArray(payload.watched)) return;
    for (const w of payload.watched) {
      const id = `${w.room_handle}:${w.owner}:${w.stream_id}`;
      if (videoControllers.has(id)) continue;
      const span = fpsSpans.get(id);
      if (!span) continue;
      span.textContent = `@${Math.round(w.received_fps)}fps`;
    }
  }

  window.ViewTab = { refresh: init, updateStats };

  document.addEventListener("DOMContentLoaded", init);
})();
