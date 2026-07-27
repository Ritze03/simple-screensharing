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
   * feeds every VideoFrame chunk to it as an `EncodedVideoChunk`. Decoded
   * frames are drawn directly in `decoder.output` (the simplest correct
   * pattern here — the decoder itself already paces output, so no separate
   * rAF loop is needed) and immediately `close()`d to release their
   * underlying resources. `errorPlaceholder` is revealed (and the canvas
   * left showing its last frame underneath) if the decoder enters its
   * `error` state, matching the existing `'stalled'`
   * `.media-tile-placeholder` visual rather than inventing a new one. */
  async function startTileVideo(tile, canvas, errorPlaceholder) {
    const controller = { decoder: null, reader: null, stopped: false };
    videoControllers.set(tile.id, controller);

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
      response = await fetch(tile.videoUrl);
    } catch (e) {
      console.error(`[view] failed to fetch video stream for ${tile.id}:`, e);
      showError();
      return;
    }
    if (controller.stopped) return;
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
            try {
              decoder.decode(
                new EncodedVideoChunk({
                  type: flags & 0x01 ? "key" : "delta",
                  timestamp: Number(ptsUs),
                  data: payload,
                })
              );
            } catch (e) {
              // A single bad access unit shouldn't kill the whole tile —
              // log and keep decoding subsequent chunks. A sustained
              // problem surfaces via the decoder's own `error` callback.
              console.error(`[view] VideoDecoder.decode() failed for ${tile.id}:`, e);
            }
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

  async function init() {
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
   * value; real removal is handled elsewhere via `refresh()`. */
  function updateStats(payload) {
    if (!payload || !Array.isArray(payload.watched)) return;
    for (const w of payload.watched) {
      const span = fpsSpans.get(`${w.room_handle}:${w.owner}:${w.stream_id}`);
      if (!span) continue;
      span.textContent = `@${Math.round(w.received_fps)}fps`;
    }
  }

  window.ViewTab = { refresh: init, updateStats };

  document.addEventListener("DOMContentLoaded", init);
})();
