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
// delivery this file used to just point an `<img>` at). The actual
// fetch/parse-chunks/decode/jitter-buffer pipeline lives in
// `tile-video.js`'s `window.TileVideo` (extracted so the standalone
// fullscreen/borderless viewer window can drive the same pipeline against
// its own tiles) — this file only owns grid layout, tile creation/removal,
// and the fps `<span>` DOM wiring (`fpsSpans`/`updateStats`).
//
// Known gap: no DTO carries a per-watched-stream "is this actually
// decoding right now" signal, so every real tile still renders in the
// `'live'` visual state regardless of whether frames are actually flowing;
// the `'stalled'` placeholder is only reachable via the mock branch's demo
// toggle until a later worker wires a real signal for it. A `VideoDecoder`
// entering its `error` state *is* surfaced (see `tile-video.js`'s
// `startTileVideo`), reusing the same `.media-tile-placeholder` visual the
// `'stalled'` state uses.

(function () {
  "use strict";

  const { el, icon, renderEmptyState, secondaryButton, statusBadge } = window.Components;

  const MOCK_WATCHED = [
    { id: "w1", ownerLabel: "Ana", roomLabel: "Movie Night", resolution: "1920x1080", fps: 30, state: "live", hue: 220 },
    { id: "w2", ownerLabel: "Priya", roomLabel: "Book Club", resolution: "1280x720", fps: 60, state: "live", hue: 160 },
    { id: "w3", ownerLabel: "Sam", roomLabel: "Gaming Night", resolution: "1920x1080", fps: null, state: "stalled", hue: 0 },
  ];

  let mockWatched = [];

  // Tile ids (`${room_handle}:${owner}:${stream_id}`) the user has explicitly
  // checked via each tile's own selection checkbox, for the "Fullscreen
  // selected" / "Borderless selected" buttons below. Deliberately NOT
  // cleared on every `render()` (a `stream-stats-updated`-driven re-render
  // shouldn't silently wipe a selection the user just made) — only trimmed
  // to drop ids for tiles that no longer exist, right before each render.
  let selectedTileIds = new Set();

  // Maps tile id (`${room_handle}:${owner}:${stream_id}`) -> that tile's fps
  // `<span>`, rebuilt on every `render()`. `stream-stats-updated` fires
  // unconditionally ~4x/sec (see `dto::StreamStatsUpdated`'s doc comment),
  // so `updateStats` below patches just this span's text in place rather
  // than re-rendering the tile, to avoid flicker. Also written to by each
  // tile's own `TileVideo`-owned fps interval via the `tile.onFps` hook set
  // up in `renderTile` — see `tile-video.js`'s `startTileVideo` doc comment
  // for why that module reaches back out through a callback instead of
  // touching this DOM node itself (it knows nothing about tile layout).
  let fpsSpans = new Map();

  // tile.id -> ms epoch first observed 'stalled'. No per-watched-stream DTO
  // carries a "since" timestamp (see this file's header note on the known
  // decode-health gap) — tracked client-side, same pattern as
  // `stream-control.js`'s `destStalledSince`.
  const tileStalledSince = new Map();
  function getStalledSince(tile) {
    if (tile.state !== "stalled") {
      tileStalledSince.delete(tile.id);
      return null;
    }
    if (!tileStalledSince.has(tile.id)) tileStalledSince.set(tile.id, Date.now());
    return tileStalledSince.get(tile.id);
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

  /** `window.__ELECTRON__.openViewer` is the entry point built by the
   * fullscreen/borderless viewer feature (see `electron/main.js`'s
   * `open-viewer-window` IPC handler and `electron/preload.js`'s bridge) —
   * this is the only place in the View tab that calls it. `displayId: null`
   * defers to `resolveTargetDisplay`'s nearest-cursor-display default;
   * exposing a display picker here is a reasonable follow-up but out of
   * scope for this pass (see `electron/main.js`'s `list-displays` IPC,
   * already wired and ready for one).
   *
   * Selection default, per the feature's original design intent: with
   * nothing checked, act on every currently-visible tile rather than
   * forcing the user to select first for the common single-stream case. */
  function requestOpenViewer(mode) {
    if (!window.__ELECTRON__) return;
    const streamKeys = selectedTileIds.size > 0 ? Array.from(selectedTileIds) : mockWatched.map((t) => t.id);
    if (streamKeys.length === 0) return;
    window.__ELECTRON__.openViewer({ mode, displayId: null, streamKeys });
  }

  function render() {
    const root = document.getElementById("view-root");
    if (!root) return;
    root.innerHTML = "";

    // Drop selection entries for tiles that no longer exist (stream ended,
    // room left, etc.) before deciding what "selected" means for this render.
    const liveIds = new Set(mockWatched.map((t) => t.id));
    for (const id of selectedTileIds) {
      if (!liveIds.has(id)) selectedTileIds.delete(id);
    }
    // Same prune for `tileStalledSince` — a tile id is `room_handle:owner:
    // stream_id` and *can* recur (the same stream watched again later), so
    // leaving a stale entry behind isn't just an unbounded-growth risk over
    // a long session, it can also make a later re-stall of the same id
    // report elapsed time from the *previous* stall instead of starting at
    // 0s (`getStalledSince` only sets a fresh timestamp when the id has no
    // entry yet).
    for (const id of tileStalledSince.keys()) {
      if (!liveIds.has(id)) tileStalledSince.delete(id);
    }

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
    // Hidden entirely (not just disabled) when there's no Electron bridge —
    // same pattern as this codebase's existing `window.__TAURI__` guards —
    // so a plain browser-preview session can't throw clicking a button that
    // has nothing to call. Also hidden when there are zero watched streams:
    // nothing to show fullscreen.
    if (window.__ELECTRON__ && mockWatched.length > 0) {
      controls.appendChild(
        el("div", { className: "view-viewer-actions" }, [
          secondaryButton("Fullscreen selected", () => requestOpenViewer("fullscreen"), { className: "view-viewer-action-btn" }),
          secondaryButton("Borderless selected", () => requestOpenViewer("borderless"), { className: "view-viewer-action-btn" }),
        ])
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
    window.TileVideo.stopAllTileVideo();

    if (mockWatched.length === 0) {
      root.appendChild(
        renderEmptyState("Pick someone from a room in the sidebar and click Watch.", null, null, {
          icon: "visibility",
          headline: "Nobody's on screen",
        })
      );
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
      placeholder.appendChild(statusBadge("degraded", "WAITING FOR CAPTURE", { since: getStalledSince(tile) }));
      wrap.appendChild(placeholder);
    } else if (tile.videoUrl) {
      // Real video: a `<canvas>` driven by a WebCodecs `VideoDecoder`
      // pulling from this stream's raw H.264-over-HTTP URL (see
      // `src-tauri/src/video_server.rs` for the wire format). Unlike the
      // old MJPEG `<img>`, decoding is genuinely our job now — see
      // `tile-video.js`'s `startTileVideo` for the fetch/parse/decode/draw
      // pipeline. The sibling placeholder starts hidden and is only
      // revealed if `startTileVideo` hits a fetch or `VideoDecoder` error,
      // reusing the same `.media-tile-placeholder` visual the `'stalled'`
      // state above uses rather than inventing a new error look. `onFps` is
      // this tab's own hook into `startTileVideo`'s fps interval (see
      // `tile-video.js`'s doc comment) — it patches this tile's fps `<span>`
      // in place, the DOM-touching part `tile-video.js` deliberately leaves
      // to its caller.
      const canvas = el("canvas", { className: "media-tile-video", attrs: { role: "img", "aria-label": tile.ownerLabel } });
      const errorPlaceholder = el("div", { className: "media-tile-placeholder", attrs: { hidden: "hidden" } });
      errorPlaceholder.appendChild(el("span", { text: "Stream error" }));
      wrap.appendChild(canvas);
      wrap.appendChild(errorPlaceholder);
      tile.onFps = (fps) => {
        const span = fpsSpans.get(tile.id);
        if (span) span.textContent = `@${Math.round(fps)}fps`;
      };
      window.TileVideo.startTileVideo(tile, canvas, errorPlaceholder);
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

    // Per-tile selection checkbox, feeding `selectedTileIds` for the
    // "Fullscreen selected" / "Borderless selected" buttons in
    // `.view-controls`. Only meaningful once Electron's viewer bridge
    // exists — hidden otherwise so a mock/browser-preview session doesn't
    // show a control with no consumer.
    const actions = el("div", { className: "media-tile-overlay-actions" });
    if (tile.id && window.__ELECTRON__) {
      const checkbox = el("input", {
        className: "media-tile-select-checkbox",
        attrs: { type: "checkbox", title: "Select for fullscreen/borderless", "aria-label": `Select ${tile.ownerLabel} for fullscreen/borderless` },
      });
      checkbox.checked = selectedTileIds.has(tile.id);
      checkbox.addEventListener("click", (e) => e.stopPropagation());
      checkbox.addEventListener("change", () => {
        if (checkbox.checked) selectedTileIds.add(tile.id);
        else selectedTileIds.delete(tile.id);
      });
      actions.appendChild(checkbox);
    }
    actions.appendChild(
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
    top.appendChild(actions);
    wrap.appendChild(top);

    return wrap;
  }

  /** Fetches the user's `max_jitter_buffer_ms` preference once at startup
   * and passes it to `tile-video.js`'s `window.TileVideo.setMaxJitterBufferMs`
   * — the jitter buffer itself lives in that module now (extracted so the
   * standalone fullscreen/borderless viewer can share it), so this file has
   * no direct access to the underlying binding. No-op on the mock branch
   * (no `window.__TAURI__`) — `tile-video.js` just keeps its own
   * `DEFAULT_MAX_JITTER_BUFFER_MS` starting value, mirroring `settings.js`'s
   * `fetchSettings` mock fallback. */
  async function fetchMaxJitterBufferMs() {
    if (!window.__TAURI__) return;
    const settings = await window.__TAURI__.core.invoke("get_settings");
    window.TileVideo.setMaxJitterBufferMs(settings.max_jitter_buffer_ms);
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
   * `tile-video.js`'s `frameTimestamps`/`recordFrame`/`currentFps` for
   * why: the server-side counter this event used to report stopped being
   * fed anything the moment the H.264-relay redesign stopped calling
   * `StreamDecoder::latest_frame()`). Rather than let this handler and
   * `startTileVideo`'s own `setInterval`-driven `onFps` callback both write
   * the same `<span>` on different cadences (flicker, and a race over which
   * value "wins" on any given tick), a tile with an active `TileVideo`
   * controller is skipped here entirely (`TileVideo.hasController`) — that
   * interval owns the span exclusively for as long as the controller is
   * alive. This branch only still matters for the mock branch
   * (`window.__TAURI__` absent, so `mockWatched` entries carry a static
   * `fps` and never get a real controller) and for the brief window before
   * a tile's `startTileVideo` call has set up its own interval. */
  function updateStats(payload) {
    if (!payload || !Array.isArray(payload.watched)) return;
    for (const w of payload.watched) {
      const id = `${w.room_handle}:${w.owner}:${w.stream_id}`;
      if (window.TileVideo.hasController(id)) continue;
      const span = fpsSpans.get(id);
      if (!span) continue;
      span.textContent = `@${Math.round(w.received_fps)}fps`;
    }
  }

  window.ViewTab = { refresh: init, updateStats };

  document.addEventListener("DOMContentLoaded", init);
})();
