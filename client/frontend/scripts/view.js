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
// `tile-video.js`'s `window.TileVideo` (extracted so the standalone pop-out
// viewer window — see below — can drive the same pipeline against its own
// tile) — this file only owns grid layout, tile creation/removal, and the
// fps `<span>` DOM wiring (`fpsSpans`/`updateStats`).
//
// Known gap: no DTO carries a per-watched-stream "is this actually
// decoding right now" signal, so every real tile still renders in the
// `'live'` visual state regardless of whether frames are actually flowing;
// the `'stalled'` placeholder is only reachable via the mock branch's demo
// toggle until a later worker wires a real signal for it. A `VideoDecoder`
// entering its `error` state *is* surfaced (see `tile-video.js`'s
// `startTileVideo`), reusing the same `.media-tile-placeholder` visual the
// `'stalled'` state uses.
//
// "Watch grid" restyle (design handoff "Stream Control Redesign Options",
// option 1e) — grid layout, hover chrome, held-slot placeholders, room
// filter, and the "NOT ON SCREEN" rail below. Scoped-down/known-limitation
// notes, each next to the code it applies to:
//
//  - Held slots for popped-out tiles (`poppedOutIds` below) are tracked
//    client-side, kept in sync two ways: the user's own "Bring it back"
//    button, and (2026-07-28) a real `window.__ELECTRON__.onPopoutClosed`
//    IPC signal fired when the pop-out `BrowserWindow` itself closes for any
//    reason (× button, OS close, etc.) — see the listener registered near
//    the bottom of this file and `electron/main.js`'s `openPopoutWindow`.
//
// Pop-out consolidation (2026-07-28): the "Fullscreen selected"/"Borderless
// selected" multi-select buttons and their Electron singleton viewer window
// are gone — see `superdoc/features/viewer-window.md`'s matching dated entry.
// The per-tile "Pop out" button (`popOutWatch`) below is now the one
// mechanism for viewing a stream in its own OS window: it opens (or, for an
// already-popped-out stream, focuses) a lean chrome-free window via
// `window.__ELECTRON__.openPopout`, one independent window per stream
// (multiple streams can each be popped out simultaneously). F11 inside that
// window toggles real OS fullscreen; exiting fullscreen never closes it —
// see `frontend/scripts/viewer.js`.
//  - Per-tile volume/mute (2026-07-28): a tile whose sender is also sharing
//    audio gets a mute button + volume slider in its hover overlay, wired to
//    the real `commands::set_volume`. There is deliberately NO listen/unlisten
//    toggle: the sender's audio is already subscribed and playing by the time
//    a tile exists, because the backend pairs it with the video watch
//    automatically (`session.rs`'s `sync_audio_watches`). A viewer never sees
//    a sender's audio as its own stream — see `superdoc/features/
//    video-pipeline.md`'s 2026-07-28 "Audio follows video" block. (This
//    replaces an older note here that said a volume slider was unwireable
//    because listening had to be toggled on first; that's no longer true.)
//  - No global volume icon in the header: per-tile is the meaningful scope
//    (each sender's audio is independently adjustable), and a master volume
//    would need a mixer concept the backend doesn't have.
//  - The "NOT ON SCREEN" rail (streams the user can watch but isn't) is
//    real-data on the Tauri branch (`detail.streams` minus `detail.watched`,
//    joined with `detail.members` for the display name) but has one gap: no
//    DTO here distinguishes "a stream I'm broadcasting myself" from "a
//    stream a room-mate is broadcasting", so the rail can list the user's
//    own outgoing stream with no way to hide it. A real fix needs a backend
//    DTO change; out of scope for this pass.

(function () {
  "use strict";

  const { el, icon, renderEmptyState, secondaryButton, statusBadge } = window.Components;

  const MOCK_WATCHED = [
    // `hasAudio` mirrors the real branch's per-sender audio flag so a
    // plain-browser/Playwright run exercises both the with-audio and
    // without-audio tile layouts. `bitrate` mirrors the real branch's
    // `received_bitrate_kbps` (see `updateStats`/`dto::WatchedStatsDto`) —
    // a static stand-in since there's no real decode pipeline to measure in
    // the mock branch.
    { id: "w1", ownerLabel: "Ana", roomLabel: "Movie Night", resolution: "1920x1080", fps: 30, bitrate: 4200, state: "live", hue: 220, hasAudio: true },
    { id: "w2", ownerLabel: "Priya", roomLabel: "Book Club", resolution: "1280x720", fps: 60, bitrate: 2600, state: "live", hue: 160, hasAudio: false },
    { id: "w3", ownerLabel: "Sam", roomLabel: "Gaming Night", resolution: "1920x1080", fps: null, bitrate: null, state: "stalled", hue: 0 },
  ];

  // Mock-only stand-in for the "NOT ON SCREEN" rail's real-branch data (see
  // `fetchWatchedStreams`'s real branch for how that's actually derived).
  const MOCK_NOT_ON_SCREEN = [
    { room_handle: null, owner: "w4", stream_id: 0, ownerLabel: "Jonas", roomLabel: "Movie Night", resolution: "1920x1080", hasAudio: true },
    { room_handle: null, owner: "w5", stream_id: 0, ownerLabel: "Lea", roomLabel: "Book Club", resolution: "1280x720", hasAudio: false },
  ];

  let mockWatched = [];

  // "NOT ON SCREEN" rail data — streams the user can watch but isn't
  // currently displaying. Rebuilt by `fetchWatchedStreams` on every
  // `init()`/`refresh()`, same cadence as `mockWatched`.
  let notOnScreenStreams = [];

  // Selected room filter chip ("All rooms" when null) — pruned back to null
  // right before a render if the selected room no longer has any tiles.
  let roomFilter = null;

  // Tile ids currently popped out to their own OS window (`popOutWatch`) —
  // rendered as a held dashed-border slot instead of video. Cleared either
  // by the user's "Bring it back" button or automatically when the pop-out
  // window closes — see this file's header note and the `onPopoutClosed`
  // listener near the bottom of this file.
  const poppedOutIds = new Set();

  // Dynamic grid layout (2026-07-28) — see `computeGridShape`'s doc comment
  // for the algorithm and `applyGridLayout` for how it's wired to the DOM.
  // `currentGridEl`/`currentTileCount` are the last-rendered grid's element
  // and on-screen tile count (including held slots), kept outside `render()`
  // so a debounced resize can recompute the layout without a full rebuild
  // (which would tear down every tile's video pipeline — see `render()`'s
  // `stopAllTileVideo()` call).
  let currentGridEl = null;
  let currentTileCount = 0;
  let gridLayoutDebounce = null;

  // Must match `.view-grid`'s `gap` in view.css.
  const GRID_GAP_PX = 12;

  /** Pure layout math — no DOM access, easy to sanity-check standalone (see
   * this file's matching `superdoc/features/viewer-window.md` entry for how
   * this was verified without a JS test framework).
   *
   * Replaces the old `ceil(sqrt(n))` "square-ish grid" heuristic, which only
   * looked at the tile count and ignored the window's actual shape — in a
   * tall/narrow window, 2 side-by-side 16:9 tiles each shrink to fit the
   * width, wasting most of the window's height as letterboxing above/below.
   *
   * For every `cols` from 1 to `n` (rows = `ceil(n / cols)`, the minimum
   * rows that fit `n` tiles at that column count — never more, per the task
   * brief's `(rows-1) * cols < n` invariant, which `ceil` already
   * guarantees), this computes the size a single 16:9 tile would render at
   * if `availWidth x availHeight` were evenly divided into that `rows x
   * cols` grid (accounting for `gap` between cells), then picks whichever
   * `rows x cols` shape maximizes that per-tile displayed area. Since every
   * candidate is a uniform grid (all cells the same size), maximizing
   * per-tile area is equivalent to maximizing total displayed video area
   * across all `n` tiles — the wasted-space slots from `rows * cols > n`
   * (e.g. 3 tiles in a 2x2 grid, one cell empty) don't change any
   * individual tile's size, so they don't need special-casing here. */
  function computeGridShape(n, availWidth, availHeight, gap = GRID_GAP_PX, aspect = 16 / 9) {
    const w = Math.max(0, availWidth);
    const h = Math.max(0, availHeight);
    if (n <= 1) {
      let tileWidth = w;
      let tileHeight = tileWidth / aspect;
      if (tileHeight > h) {
        tileHeight = h;
        tileWidth = tileHeight * aspect;
      }
      return { cols: 1, rows: n, tileWidth, tileHeight };
    }

    let best = null;
    for (let cols = 1; cols <= n; cols++) {
      const rows = Math.ceil(n / cols);
      if ((rows - 1) * cols >= n) continue; // dead case per the invariant above; ceil() never actually hits this
      const cellWidth = (w - gap * (cols - 1)) / cols;
      const cellHeight = (h - gap * (rows - 1)) / rows;
      if (cellWidth <= 0 || cellHeight <= 0) continue;
      let tileWidth = cellWidth;
      let tileHeight = tileWidth / aspect;
      if (tileHeight > cellHeight) {
        tileHeight = cellHeight;
        tileWidth = tileHeight * aspect;
      }
      const area = tileWidth * tileHeight;
      if (!best || area > best.area) best = { cols, rows, tileWidth, tileHeight, area };
    }
    // Degenerate fallback (availWidth/availHeight not yet laid out, e.g. 0):
    // single row, n columns, no aspect clamp against a nonexistent height.
    if (!best) {
      const tileWidth = n > 0 ? w / n : 0;
      return { cols: Math.max(1, n), rows: 1, tileWidth, tileHeight: tileWidth / aspect };
    }
    return best;
  }

  /** Measures `.content` (the always-present scroll container all tabs
   * share — stable even while this tab's panel is `hidden`) minus everything
   * above the grid (via the grid's own rendered top, see the `Why:` note
   * inside for why this isn't a hand-enumerated sibling list anymore) and
   * the "not on screen" rail below it (measured from its real rendered
   * height, not assumed), then writes the resulting shape as CSS custom
   * properties on `currentGridEl`. No-ops if the grid isn't currently
   * rendered, or if the available box measures to 0 or negative (this tab's
   * panel is `hidden`, or a render happened before first layout) — the
   * `ResizeObserver` below re-fires this once real dimensions are available
   * (a `hidden` -> visible panel transition itself is a resize of
   * `#view-root`, from 0 to its real size). */
  function applyGridLayout() {
    if (!currentGridEl || currentTileCount <= 0) return;
    const content = document.querySelector(".content");
    if (!content) return;
    const cs = getComputedStyle(content);
    const availWidth = content.clientWidth - parseFloat(cs.paddingLeft || "0") - parseFloat(cs.paddingRight || "0");
    if (availWidth <= 0) return;

    // Bug (2026-07-28, see `superdoc/features/viewer-window.md`'s follow-up
    // entry): this used to derive `availHeight` from `.content`'s total
    // height minus *assumed* sibling heights (`.view-header`/`.view-controls`
    // above, `.view-not-on-screen` below, with hardcoded margin literals).
    // That list silently missed the `<h2>` tab title that sits above
    // `#view-root` (see index.html) — its ~35px (19px line box + 16px
    // margin-bottom) was never subtracted, so `availHeight` came out that
    // much too tall on every render. Harmless when width was the binding
    // constraint (the extra vertical slack just went unused), but on
    // height-constrained shapes (e.g. a 1-column stack, or any shape close
    // to filling the container's height) the grid was sized into space that
    // didn't actually exist, pushing its bottom — and the "not on screen"
    // rail below it — past `.content`'s visible bottom edge.
    //
    // Fixed by anchoring the top measurement to `currentGridEl`'s own
    // rendered top (`getBoundingClientRect().top`) instead of re-deriving it
    // from a hand-maintained list of siblings above it. The grid's top
    // position only depends on the layout of *everything before it* in the
    // DOM (h2, header, controls, and whatever else may be added later) —
    // never on the grid's own height — so this can't go stale the way an
    // enumerated sibling list can, and there's no gap-count/margin literal
    // to keep in sync for the "above" side anymore.
    const contentRect = content.getBoundingClientRect();
    const contentPaddingBottom = parseFloat(cs.paddingBottom || "0");
    // `content.clientHeight` spans `.content`'s full padding box (no border
    // on `.content`, so the border box and padding box coincide); subtract
    // the bottom padding to land on the real bottom edge of the usable
    // (scrollable) content area.
    const contentBottom = contentRect.top + content.clientHeight - contentPaddingBottom;
    const gridTop = currentGridEl.getBoundingClientRect().top;

    // The "not on screen" rail still needs its own hardcoded margin-top
    // (`.view-not-on-screen`'s `16px` in view.css) — it sits *after* the
    // grid, so it can't be folded into the same "measure up to my own top"
    // trick without moving the grid's height computation into a second pass.
    const railEl = document.querySelector(".view-not-on-screen");
    const usedBelow = railEl ? railEl.getBoundingClientRect().height + 16 : 0;

    const availHeight = contentBottom - gridTop - usedBelow;
    if (availHeight <= 0) return;

    const { cols, rows, tileWidth, tileHeight } = computeGridShape(currentTileCount, availWidth, availHeight);
    currentGridEl.style.setProperty("--grid-cols", String(cols));
    currentGridEl.style.setProperty("--grid-rows", String(Math.max(1, rows)));
    currentGridEl.style.setProperty("--tile-w", `${tileWidth.toFixed(2)}px`);
    currentGridEl.style.setProperty("--tile-h", `${tileHeight.toFixed(2)}px`);
  }

  // Recomputes the layout (debounced) on any resize of `#view-root` — real
  // window/sidebar resizes while this tab is visible, and also the
  // 0-size -> real-size jump when switching onto this tab from another one
  // (`shell.js`'s `setActiveTab` just flips the `hidden` attribute; it
  // doesn't know to call back into this file, so this observer is what
  // catches that transition instead of leaving a stale/zeroed layout up).
  (function watchGridContainerResize() {
    const watchedEl = document.getElementById("view-root");
    if (!watchedEl || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (gridLayoutDebounce) clearTimeout(gridLayoutDebounce);
      gridLayoutDebounce = setTimeout(() => {
        gridLayoutDebounce = null;
        applyGridLayout();
      }, 120);
    });
    observer.observe(watchedEl);
  })();

  // Per-tile playback volume (0-200, matching `commands::set_volume`'s range)
  // and mute state, keyed by tile id. Client-side only: `RoomDetail` carries
  // no `volume_percent`, and re-`render()`s happen ~4x/sec off
  // `stream-stats-updated`, so a slider that re-read its value from the DOM
  // would snap back to the default constantly. Survives re-renders; pruned
  // for any tile id no longer in the watched set (see below).
  const tileVolume = new Map();
  const tileMuted = new Set();
  const DEFAULT_VOLUME = 100;

  /** Push a tile's effective volume (0 while muted) to the backend.
   *
   * Note the `streamId` passed is the tile's *video* stream id — this tab
   * never learns the sender's audio stream id and deliberately shouldn't:
   * audio isn't a separate thing a viewer interacts with. `commands::
   * set_volume` resolves the sibling audio stream itself (see its doc
   * comment). */
  async function pushVolume(tile) {
    // `tile.room_handle` is a real `u32` `RoomHandle` (see
    // `session::RoomSession::next_room_handle`, which starts at 0) — the
    // first room ever joined in a client session gets handle 0, and `!0` is
    // `true` in JS. A plain truthiness check here silently dropped every
    // volume/mute change for that room for the lifetime of the app process
    // (no error, no retry — `pushVolume` just returned before ever calling
    // `invoke`), which is exactly the "the slider doesn't work at all" bug
    // report: an ordinary session's first (and often only) joined room is
    // room_handle 0. `fetchWatchedStreams` below already gets this right
    // (`r.room_handle != null`) — this is the one place that didn't.
    if (!window.__TAURI__ || tile.room_handle == null) return;
    const percent = tileMuted.has(tile.id) ? 0 : (tileVolume.get(tile.id) ?? DEFAULT_VOLUME);
    try {
      await window.__TAURI__.core.invoke("set_volume", {
        roomHandle: tile.room_handle,
        owner: tile.owner,
        streamId: tile.stream_id,
        volumePercent: percent,
      });
    } catch (e) {
      console.error("set_volume failed", e);
    }
  }

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

  // Same pattern as `fpsSpans` above, for the bitrate readout in the
  // always-visible stat bar (2026-07-29 prominent-stats redesign). Only
  // `updateStats` writes these — there is no client-side (`tile-video.js`)
  // bitrate source the way there is for fps, since `received_bitrate_kbps`
  // is counted server-side off real received bytes (see `decode::
  // ByteRateCounter`'s doc comment) and stays accurate regardless of which
  // decode path the tile's video is using.
  let bitrateSpans = new Map();

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
      const notOnScreen = [];
      for (const room of joined) {
        const detail = await window.__TAURI__.core.invoke("room_detail", { roomHandle: room.room_handle });
        if (!detail) continue;
        const watchedKeys = new Set(detail.watched.map((k) => `${k.owner}:${k.stream_id}`));
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
            // Patched in place by `updateStats` from `stream-stats-updated`'s
            // `payload.watched[].received_bitrate_kbps` (`dto::
            // WatchedStatsDto`) — real received throughput, not the sender's
            // configured encoder bitrate (see that DTO field's doc comment).
            // Starts `null` (nothing measured yet this tick).
            bitrate: null,
            videoUrl,
            // See this file's header note: no real per-stream decode-health
            // signal is wired up yet, so every real tile is 'live'.
            state: "live",
            // Does this sender have audio riding along with their video? The
            // backend has already auto-subscribed and started *playing* it by
            // the time a tile exists (see `session.rs`'s
            // `sync_audio_watches`) — this flag exists only to decide whether
            // to draw a volume control on the tile, never to start or stop
            // anything. Deliberately not a separate entry anywhere: the whole
            // point is that a viewer never sees "someone's audio" as its own
            // stream to act on.
            hasAudio: detail.streams.some((s) => s.owner === key.owner && s.kind === "Audio"),
            // `key.owner` is a lowercase-hex `ConnId` string (no `0x`
            // prefix — see `dto::hex_u64`'s doc comment for why it's a
            // string at all, not a JS `Number`), so it needs the prefix
            // added back before `BigInt` can parse it.
            hue: Number(BigInt("0x" + key.owner) % 360n),
          });
        }
        // "NOT ON SCREEN" rail: this room's announced video streams the
        // user isn't currently watching. See this file's header note for
        // the "can't tell my own outgoing stream apart" gap this leaves.
        for (const s of detail.streams) {
          if (s.kind !== "Video") continue;
          const k = `${s.owner}:${s.stream_id}`;
          if (watchedKeys.has(k)) continue;
          const member = detail.members.find((m) => m.conn_id === s.owner);
          notOnScreen.push({
            room_handle: room.room_handle,
            owner: s.owner,
            stream_id: s.stream_id,
            ownerLabel: member ? member.display_name : String(s.owner),
            roomLabel: room.name,
            // Same `StreamInfoDto` this room's watched-tile join above reads
            // `streamInfo.width`/`.height` from (and the same source
            // `shell.js`'s `renderRoomPanelStreamRow` already shows a
            // resolution string for) — announced up-front by the sender, so
            // it's available even before this stream is ever watched.
            // Bitrate has no such pre-watch source (it's a live encode/
            // transport stat — see this file's header note on the on-screen
            // tile overlay for where that one *is* available), so it's
            // deliberately omitted here rather than shown as a fake number.
            resolution: `${s.width}x${s.height}`,
            hasAudio: detail.streams.some((other) => other.owner === s.owner && other.kind === "Audio"),
          });
        }
      }
      notOnScreenStreams = notOnScreen;
      return tiles;
    }
    notOnScreenStreams = MOCK_NOT_ON_SCREEN.slice();
    return MOCK_WATCHED.slice();
  }

  /** "Play" action on a "NOT ON SCREEN" rail chip — starts watching that
   * stream via the same `toggle_watch` command the sidebar's own per-member
   * Watch button already uses (`shell.js`'s `onWatch` handler), then
   * refetches so the new tile appears in the grid and the chip drops out of
   * the rail. */
  async function playNotOnScreen(item) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("toggle_watch", { roomHandle: item.room_handle, owner: item.owner, streamId: item.stream_id });
      await init();
      return;
    }
    // Mock: no backend `watched` set exists in a plain browser — synthesize
    // the tile locally so a Playwright run can exercise the rail's "play"
    // action end to end.
    notOnScreenStreams = notOnScreenStreams.filter((x) => x !== item);
    mockWatched.push({
      id: `${item.roomLabel}:${item.owner}:${item.stream_id}`,
      ownerLabel: item.ownerLabel,
      roomLabel: item.roomLabel,
      resolution: item.resolution ?? null,
      fps: null,
      bitrate: null,
      state: "live",
      hue: Math.floor(Math.random() * 360),
      hasAudio: item.hasAudio ?? false,
    });
    render();
  }

  /** "Stop watching" button on an on-screen tile — stops watching that
   * stream via the same `toggle_watch` command `playNotOnScreen` above uses
   * to *start* one. `toggle_watch` is a genuine toggle (see
   * `src-tauri/src/commands.rs`'s doc comment: it inserts a `WatchEntry` if
   * there wasn't one, removes it if there was — same call, opposite
   * direction depending on current state), so calling it again here for an
   * already-watched key is exactly the "stop" half of that same command,
   * not a separate one. Refetching afterwards drops this tile from the grid
   * and, since the stream is still announced in the room, lets it
   * reappear in the "NOT ON SCREEN" rail on its own (`fetchWatchedStreams`
   * derives that rail from `detail.streams` minus `detail.watched`). */
  async function stopWatching(tile) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("toggle_watch", { roomHandle: tile.room_handle, owner: tile.owner, streamId: tile.stream_id });
      await init();
      return;
    }
    // Mock: no backend `watched` set exists in a plain browser — remove the
    // tile locally and hand it back to the "NOT ON SCREEN" rail (mirrors
    // `playNotOnScreen`'s reverse move) so a Playwright run can exercise
    // stop-then-restart end to end.
    mockWatched = mockWatched.filter((t) => t.id !== tile.id);
    notOnScreenStreams.push({
      room_handle: null,
      owner: tile.id,
      stream_id: 0,
      ownerLabel: tile.ownerLabel,
      roomLabel: tile.roomLabel,
      resolution: tile.resolution ?? null,
      hasAudio: tile.hasAudio ?? false,
    });
    render();
  }

  /** The one "view this stream in its own window" mechanism (see this
   * file's header note on the 2026-07-28 consolidation). `window.__ELECTRON__
   * .openPopout` (see `electron/main.js`'s `open-popout-window` IPC handler
   * and `electron/preload.js`'s bridge) opens a lean chrome-free
   * `viewer.html` window for exactly this one stream — or, if this stream is
   * already popped out, focuses that existing window instead of creating a
   * duplicate (mirrors the old Tauri `pop_out_stream` command's
   * label-reuse behavior). Each stream gets its own independent window;
   * popping out a second stream never closes the first. */
  async function popOutWatch(tile) {
    // Held-slot pattern: track locally regardless of branch (real or mock)
    // so the grid slot survives as a dashed-border placeholder instead of
    // reflowing the rest of the grid. See this file's header note on why
    // there's no way to clear this automatically when the popped-out window
    // closes.
    if (tile.id) poppedOutIds.add(tile.id);
    if (window.__ELECTRON__ && tile.id) {
      try {
        // Extra fields beyond `streamKey` (2026-07-28, functional parity
        // pass) let `viewer.js` render the same nameplate/fps/volume
        // overlay the main grid's tile shows, without re-deriving them
        // itself — see `electron/main.js`'s `openPopoutWindow` doc comment
        // on `meta`.
        await window.__ELECTRON__.openPopout({
          streamKey: tile.id,
          ownerLabel: tile.ownerLabel,
          roomLabel: tile.roomLabel,
          hasAudio: !!tile.hasAudio,
          resolution: tile.resolution || undefined,
        });
      } catch (e) {
        console.error("openPopout failed", e);
      }
      render();
      return;
    }
    render();
  }

  function render() {
    const root = document.getElementById("view-root");
    if (!root) return;
    root.innerHTML = "";

    const liveIds = new Set(mockWatched.map((t) => t.id));
    // Same prune for the per-tile audio state — a tile id can recur (the same
    // stream watched again later), and a stale mute would silently carry over
    // into that new watch.
    for (const id of tileMuted) {
      if (!liveIds.has(id)) tileMuted.delete(id);
    }
    for (const id of tileVolume.keys()) {
      if (!liveIds.has(id)) tileVolume.delete(id);
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
    // Same prune for the held-slot tracking sets — a tile that's gone for
    // good (stream ended, room left) shouldn't leave a dangling held-slot
    // placeholder behind, or a stale entry that could wrongly "hold" a
    // different later tile that happens to reuse the same id.
    for (const id of poppedOutIds) {
      if (!liveIds.has(id)) poppedOutIds.delete(id);
    }
    // Room filter: fall back to "All rooms" if the selected room dropped
    // out of the watched set entirely (room left, or its last tile ended).
    const roomLabels = [...new Set(mockWatched.map((t) => t.roomLabel))];
    if (roomFilter !== null && !roomLabels.includes(roomFilter)) roomFilter = null;

    const header = el("div", { className: "view-header" });
    const roomCount = roomLabels.length;
    // "On screen" deliberately excludes held-slot tiles (popped out to
    // their own window) — they're still watched, but not actually displayed
    // in this grid right now.
    const onScreenCount = mockWatched.filter((t) => !t.id || !poppedOutIds.has(t.id)).length;
    header.appendChild(
      el("div", {
        className: "view-header-count",
        text: `${onScreenCount} on screen · ${roomCount} room${roomCount === 1 ? "" : "s"}`,
      })
    );
    if (roomLabels.length > 1) {
      const filter = el("div", { className: "view-room-filter" });
      filter.appendChild(
        el("button", {
          className: "view-room-filter-chip" + (roomFilter === null ? " active" : ""),
          text: "All rooms",
          onClick: () => {
            roomFilter = null;
            render();
          },
        })
      );
      roomLabels.forEach((label) => {
        filter.appendChild(
          el("button", {
            className: "view-room-filter-chip" + (roomFilter === label ? " active" : ""),
            text: label,
            onClick: () => {
              roomFilter = label;
              render();
            },
          })
        );
      });
      header.appendChild(filter);
    }
    root.appendChild(header);

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
    bitrateSpans = new Map();
    window.TileVideo.stopAllTileVideo();

    if (mockWatched.length === 0) {
      root.appendChild(
        renderEmptyState("Pick someone from a room in the sidebar and click Watch.", null, null, {
          icon: "visibility",
          headline: "Nobody's on screen",
        })
      );
      renderNotOnScreenRail(root);
      currentGridEl = null;
      currentTileCount = 0;
      return;
    }

    const shownTiles = roomFilter === null ? mockWatched : mockWatched.filter((t) => t.roomLabel === roomFilter);

    const grid = el("div", { className: "view-grid" });
    // 1e's "Watch grid" spec: n includes held-slot placeholders (popped-out
    // tiles below) so the grid doesn't reshuffle every other tile just
    // because one left the grid for another window. The actual rows x cols
    // shape (and per-tile pixel size) is no longer a fixed `ceil(sqrt(n))`
    // — see `computeGridShape`'s doc comment above and `applyGridLayout`
    // just below, which measures the real available area and picks the
    // shape that maximizes total displayed 16:9 video area.
    shownTiles.forEach((tile) => grid.appendChild(renderTile(tile)));

    // Rail appended first (still ends up after the grid in the DOM, see
    // below) so `applyGridLayout` can read its real rendered height as part
    // of the grid's vertical budget.
    const railEl = renderNotOnScreenRail(root);
    if (railEl) root.insertBefore(grid, railEl);
    else root.appendChild(grid);

    currentGridEl = grid;
    currentTileCount = shownTiles.length;
    applyGridLayout();
  }

  /** "NOT ON SCREEN" rail — see this file's header note for the real-branch
   * data source and its one known gap. Renders nothing when there's
   * nothing to show, rather than an empty labeled section. Returns the
   * appended element (or `null`) so `render()`'s grid-layout math can read
   * its real rendered height. */
  /** "Not on screen" rail (2026-07-29 prominent-stats redesign): the owner's
   * complaint ("kinda hidden ... not visible enough") applies most literally
   * here — this used to be a row of small text chips with nothing but a
   * name and a tiny play icon, easy to miss above an otherwise-empty grid.
   * Now a row of bigger cards, each showing the same resolution/audio-
   * sharing info the sidebar's room panel already has for an
   * announced-but-not-yet-watched stream (see `fetchWatchedStreams`'s real
   * branch, which now carries `item.resolution`/`.hasAudio` through from
   * the same `StreamInfoDto` `shell.js`'s `renderRoomPanelStreamRow`
   * reads). No bitrate here on purpose — see this file's header note by
   * `fetchWatchedStreams` for why that figure genuinely doesn't exist
   * before a stream is actually being watched. */
  function renderNotOnScreenRail(root) {
    if (notOnScreenStreams.length === 0) return null;
    const rail = el("div", { className: "view-not-on-screen" });
    rail.appendChild(el("div", { className: "view-not-on-screen-label", text: "Not on screen" }));
    const cards = el("div", { className: "view-not-on-screen-cards" });
    notOnScreenStreams.forEach((item) => {
      const meta = el("div", { className: "view-not-on-screen-card-meta" });
      if (item.resolution) meta.appendChild(el("span", { className: "view-not-on-screen-card-value", text: item.resolution }));
      if (item.hasAudio) {
        const audioBadge = el("span", { className: "view-not-on-screen-card-audio", attrs: { title: "Sharing audio", "aria-label": "Sharing audio" } });
        audioBadge.appendChild(icon("volume_up"));
        meta.appendChild(audioBadge);
      }
      const info = el("div", { className: "view-not-on-screen-card-info" }, [
        el("span", { className: "view-not-on-screen-card-name", text: item.ownerLabel }),
        el("span", { className: "view-not-on-screen-card-room", text: item.roomLabel }),
        meta,
      ]);
      const playBtn = el(
        "button",
        {
          className: "view-not-on-screen-card-play",
          attrs: { title: `Watch ${item.ownerLabel}`, "aria-label": `Watch ${item.ownerLabel}` },
          onClick: () => playNotOnScreen(item),
        },
        [icon("play_arrow")]
      );
      cards.appendChild(el("div", { className: "view-not-on-screen-card" }, [info, playBtn]));
    });
    rail.appendChild(cards);
    root.appendChild(rail);
    return rail;
  }

  /** Held-slot placeholder — see this file's header note on `poppedOutIds`.
   * Deliberately renders NO video/overlay/checkbox machinery at all (the
   * tile is genuinely not decoding — its controller was never started for a
   * held tile, see `renderTile` below), just the dashed-border "bring it
   * back" affordance the design spec calls for. */
  function renderHeldSlot(tile, reasonText) {
    const wrap = el("div", { className: "media-tile media-tile-held" });
    wrap.appendChild(icon("picture_in_picture_alt"));
    wrap.appendChild(el("div", { className: "media-tile-held-text", text: `${tile.ownerLabel} is ${reasonText}` }));
    wrap.appendChild(
      secondaryButton(
        "Bring it back",
        () => {
          if (tile.id) poppedOutIds.delete(tile.id);
          render();
        },
        { className: "media-tile-held-btn" }
      )
    );
    return wrap;
  }

  function renderTile(tile) {
    // Held-slot pattern (1e spec): a tile popped out to its own window keeps
    // its grid slot but shows a placeholder instead of re-decoding video
    // nobody is looking at here. Checked before every other state below.
    if (tile.id && poppedOutIds.has(tile.id)) return renderHeldSlot(tile, "in a separate window");

    const wrap = el("div", { className: "media-tile" });

    // Double-click to fullscreen this tile in the *current* window (2026-07-28)
    // — distinct from `popOutWatch` above, which opens a whole separate OS
    // window. This is the plain Web Fullscreen API (`Element.
    // requestFullscreen`/`document.exitFullscreen`): a second dblclick while
    // already fullscreen exits it, and the browser's native Escape handling
    // exits it too (nothing in this file's or `shell.js`'s `keydown`
    // listeners calls `preventDefault()` on Escape, so that native behavior
    // is untouched). `requestFullscreen()` returns a `Promise` that can
    // reject (e.g. denied by the browser/security context) — caught here so
    // that never surfaces as an unhandled-rejection console error.
    wrap.addEventListener("dblclick", () => {
      if (document.fullscreenElement === wrap) {
        document.exitFullscreen().catch(() => {});
      } else if (wrap.requestFullscreen) {
        wrap.requestFullscreen().catch(() => {});
      }
    });

    if (tile.state === "stalled") {
      const placeholder = el("div", { className: "media-tile-placeholder media-tile-waiting" });
      placeholder.appendChild(el("div", { className: "media-tile-waiting-accent" }));
      placeholder.appendChild(icon("hourglass_top"));
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
        if (span) span.textContent = `${Math.round(fps)}fps`;
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

    // Always-visible stat bar (2026-07-29 "prominent stats" redesign — see
    // this file's header note and `superdoc/features/stream-control.md`'s
    // matching dated entry): the owner did NOT ask for stats gated behind
    // hover, and the previous `.media-tile-nameplate` pill only ever showed
    // "name · room", nothing else. This bar always sits at the top of the
    // tile — no hover required — and carries the same resolution/fps/
    // bitrate/audio-sharing figures the hover overlay used to be the only
    // way to see. `tile.resolution`/`.fps`/`.bitrate` render as
    // `--font-mono` values per this session's font convention (see
    // `superdoc/ui/FRONTEND-SHELL.md`'s Fonts section); the owner·room name
    // stays the sans default. `fpsSpan`/`bitrateSpan` are separate DOM
    // nodes (not folded into one text node) so `tile.onFps` and
    // `updateStats` below can keep patching just their own text in place
    // ~4x/sec without rebuilding the whole bar.
    const statbar = el("div", { className: "media-tile-statbar" });
    statbar.appendChild(el("span", { className: "media-tile-statbar-dot" }));
    statbar.appendChild(el("span", { className: "media-tile-statbar-name", text: `${tile.ownerLabel} · ${tile.roomLabel}` }));
    if (tile.resolution) {
      statbar.appendChild(el("span", { className: "media-tile-statbar-sep", text: "·" }));
      statbar.appendChild(el("span", { className: "media-tile-statbar-value", text: tile.resolution }));
    }
    const fpsSpan = el("span", { className: "media-tile-statbar-value media-tile-fps", text: tile.fps ? `${tile.fps}fps` : "" });
    statbar.appendChild(fpsSpan);
    if (tile.id) fpsSpans.set(tile.id, fpsSpan);
    const bitrateSpan = el("span", {
      className: "media-tile-statbar-value media-tile-bitrate",
      text: tile.bitrate ? `${tile.bitrate >= 1000 ? (tile.bitrate / 1000).toFixed(1) + " Mbps" : Math.round(tile.bitrate) + " kbps"}` : "",
    });
    statbar.appendChild(bitrateSpan);
    if (tile.id) bitrateSpans.set(tile.id, bitrateSpan);
    if (tile.hasAudio) {
      const audioBadge = el("span", { className: "media-tile-statbar-audio-badge", attrs: { title: "Sharing audio", "aria-label": "Sharing audio" } });
      audioBadge.appendChild(icon("volume_up"));
      statbar.appendChild(audioBadge);
    }
    wrap.appendChild(statbar);

    // No-chrome-by-default hover overlay (1e spec, scoped down 2026-07-29):
    // stats moved into the always-visible `.media-tile-statbar` above, so
    // this overlay is now just the hover-reveal actions (pop-out/stop
    // watching, top-right) and the audio mute/volume control (bottom-left)
    // — controls a viewer reaches for occasionally, not stats they want to
    // glance at constantly.
    const overlay = el("div", { className: "media-tile-overlay" });

    const top = el("div", { className: "media-tile-overlay-top" });
    const actions = el("div", { className: "media-tile-overlay-actions" });
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
    // Stop watching this tile — see `stopWatching`'s doc comment above for
    // why the same `toggle_watch` command used to start a watch is also the
    // right one to stop it. Rightmost of the two action buttons (destructive
    // action farthest right, same convention `shell.js`'s leave-room button
    // uses — see its own doc comment for that call).
    actions.appendChild(
      el(
        "button",
        {
          className: "media-tile-popout-btn media-tile-stop-btn",
          attrs: { title: "Stop watching", "aria-label": `Stop watching ${tile.ownerLabel}` },
          onClick: () => stopWatching(tile),
        },
        [icon("close")]
      )
    );
    top.appendChild(actions);
    overlay.appendChild(top);

    // Bottom bar — audio control only now (fps moved up next to the
    // resolution in the title, 2026-07-28 — see `titleWrap` above).
    const bottom = el("div", { className: "media-tile-overlay-bottom" });

    // Audio control — present only when this sender is actually sharing
    // audio, and *only* as a volume/mute knob on their video tile. There is
    // no "listen" toggle here on purpose: the audio is already playing by the
    // time this tile exists (the backend subscribed it together with the
    // video — `session.rs`'s `sync_audio_watches`), so the only thing left
    // for a viewer to want is "quieter" or "off". Rendered before the fps
    // span so it sits bottom-left, opposite the stats.
    if (tile.hasAudio) {
      // Builds the whole `.media-tile-audio` pill for a given mute state.
      // The mute button below swaps this element in place (`bottom.
      // replaceChild`) instead of calling the top-level `render()` — that
      // used to be the actual cause of the "muting flickers" bug report:
      // `render()` tears down and restarts *every* tile's video pipeline
      // (`window.TileVideo.stopAllTileVideo()`, see `render()` above), so
      // clicking mute on one tile flashed the whole grid while every
      // canvas re-decoded from scratch just to swap one icon. Only this
      // tile's audio pill actually needs to change.
      //
      // The slider is only appended while `!muted` (never for a muted
      // tile) — genuinely absent from the DOM, not just CSS-collapsed —
      // both per the "don't show the slider at all while muted" spec and
      // because a flex child that's absent, rather than present-but-
      // zero-width, is what keeps the muted pill's footprint a true circle
      // (see view.css's `.media-tile-audio` comment on why `gap` was
      // dropped in favor of the slider's own margins).
      const buildAudioGroup = (muted) => {
        const group = el("div", { className: "media-tile-audio" + (muted ? " media-tile-audio-muted" : "") });
        const muteBtn = el(
          "button",
          {
            className: "media-tile-audio-btn",
            attrs: { title: muted ? "Unmute" : "Mute", "aria-label": muted ? `Unmute ${tile.ownerLabel}` : `Mute ${tile.ownerLabel}` },
            onClick: (e) => {
              e.stopPropagation();
              const nowMuted = !tileMuted.has(tile.id);
              if (nowMuted) tileMuted.add(tile.id);
              else tileMuted.delete(tile.id);
              pushVolume(tile);
              const next = buildAudioGroup(nowMuted);
              bottom.replaceChild(next, audioGroup);
              audioGroup = next;
            },
          },
          [icon(muted ? "volume_off" : "volume_up")]
        );
        group.appendChild(muteBtn);
        if (!muted) {
          const slider = el("input", {
            className: "media-tile-audio-slider",
            attrs: {
              type: "range",
              min: "0",
              max: "200",
              step: "5",
              "aria-label": `Volume for ${tile.ownerLabel}`,
            },
          });
          slider.value = String(tileVolume.get(tile.id) ?? DEFAULT_VOLUME);
          slider.addEventListener("click", (e) => e.stopPropagation());
          slider.addEventListener("input", () => {
            tileVolume.set(tile.id, Number(slider.value));
            pushVolume(tile);
          });
          group.appendChild(slider);
        }
        return group;
      };
      let audioGroup = buildAudioGroup(tileMuted.has(tile.id));
      bottom.appendChild(audioGroup);
    }

    overlay.appendChild(bottom);

    wrap.appendChild(overlay);

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
   * a tile's `startTileVideo` call has set up its own interval.
   *
   * `payload.watched[].received_bitrate_kbps` (added 2026-07-29 alongside
   * the View tab's prominent-stats redesign — see `decode::
   * ByteRateCounter`'s doc comment) does NOT have the fps field's "dead
   * unless WebCodecs isn't running" problem: it's counted in `decode::
   * StreamDecoder::feed`, which every incoming media frame still passes
   * through regardless of whether anything downstream ever calls
   * `latest_frame()` (see `session.rs`'s `entry.decoder.lock().unwrap()
   * .feed(&frame)`). So the bitrate span is patched here unconditionally,
   * even for a tile with an active `TileVideo` controller — there's no
   * equivalent client-side bitrate source to prefer instead. */
  function updateStats(payload) {
    if (!payload || !Array.isArray(payload.watched)) return;
    for (const w of payload.watched) {
      const id = `${w.room_handle}:${w.owner}:${w.stream_id}`;
      const bitrateSpan = bitrateSpans.get(id);
      if (bitrateSpan) {
        bitrateSpan.textContent =
          w.received_bitrate_kbps >= 1000 ? `${(w.received_bitrate_kbps / 1000).toFixed(1)} Mbps` : `${Math.round(w.received_bitrate_kbps)} kbps`;
      }
      if (window.TileVideo.hasController(id)) continue;
      const span = fpsSpans.get(id);
      if (!span) continue;
      span.textContent = `${Math.round(w.received_fps)}fps`;
    }
  }

  window.ViewTab = { refresh: init, updateStats };

  // Real "the pop-out window closed" signal (2026-07-28) — see this file's
  // header note this replaces and `electron/main.js`'s `openPopoutWindow`/
  // `preload.js`'s `onPopoutClosed`. Registered once here at module load
  // (NOT inside `init()`, which re-runs on every `refresh()` — e.g. every
  // `room-connected`/`presence-updated` event via `shell.js` — and would
  // otherwise pile up a duplicate listener per refresh). Simply deleting the
  // closed streamKey from `poppedOutIds` and re-rendering is enough to bring
  // the tile back to normal, real-time-decoding rendering: `renderTile`
  // already falls through to its normal branches for any id not in
  // `poppedOutIds` (see its held-slot check at the top).
  if (window.__ELECTRON__ && window.__ELECTRON__.onPopoutClosed) {
    window.__ELECTRON__.onPopoutClosed((streamKey) => {
      if (poppedOutIds.delete(streamKey)) render();
    });
  }

  document.addEventListener("DOMContentLoaded", init);
})();
