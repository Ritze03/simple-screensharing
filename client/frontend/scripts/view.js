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
//    client-side only — there's no IPC signal for "the pop-out window
//    closed", so a held slot only clears when the user clicks its "Bring it
//    back" button. This is a deliberate best-effort approximation (see the
//    task brief this shipped against), not a bug: building a real
//    cross-window close signal is a bigger change than this frontend-only
//    restyle pass.
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
    // without-audio tile layouts.
    { id: "w1", ownerLabel: "Ana", roomLabel: "Movie Night", resolution: "1920x1080", fps: 30, state: "live", hue: 220, hasAudio: true },
    { id: "w2", ownerLabel: "Priya", roomLabel: "Book Club", resolution: "1280x720", fps: 60, state: "live", hue: 160, hasAudio: false },
    { id: "w3", ownerLabel: "Sam", roomLabel: "Gaming Night", resolution: "1920x1080", fps: null, state: "stalled", hue: 0 },
  ];

  // Mock-only stand-in for the "NOT ON SCREEN" rail's real-branch data (see
  // `fetchWatchedStreams`'s real branch for how that's actually derived).
  const MOCK_NOT_ON_SCREEN = [
    { room_handle: null, owner: "w4", stream_id: 0, ownerLabel: "Jonas", roomLabel: "Movie Night" },
    { room_handle: null, owner: "w5", stream_id: 0, ownerLabel: "Lea", roomLabel: "Book Club" },
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
  // rendered as a held dashed-border slot instead of video. See this file's
  // header note on why there's no real "the window closed" signal to clear
  // this automatically.
  const poppedOutIds = new Set();

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
      resolution: null,
      fps: null,
      state: "live",
      hue: Math.floor(Math.random() * 360),
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
        await window.__ELECTRON__.openPopout({ streamKey: tile.id });
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
    window.TileVideo.stopAllTileVideo();

    if (mockWatched.length === 0) {
      root.appendChild(
        renderEmptyState("Pick someone from a room in the sidebar and click Watch.", null, null, {
          icon: "visibility",
          headline: "Nobody's on screen",
        })
      );
      renderNotOnScreenRail(root);
      return;
    }

    const shownTiles = roomFilter === null ? mockWatched : mockWatched.filter((t) => t.roomLabel === roomFilter);

    const grid = el("div", { className: "view-grid" });
    // 1e's "Watch grid" spec: a computed square-ish column count
    // (`ceil(sqrt(n))`) rather than the old organic `auto-fit` reflow — n
    // includes held-slot placeholders (popped-out/viewer tiles below) so
    // the grid doesn't reshuffle every other tile just because one left the
    // grid for another window.
    grid.style.setProperty("--grid-cols", String(Math.max(1, Math.ceil(Math.sqrt(shownTiles.length)))));
    shownTiles.forEach((tile) => grid.appendChild(renderTile(tile)));
    root.appendChild(grid);

    renderNotOnScreenRail(root);
  }

  /** "NOT ON SCREEN" rail — see this file's header note for the real-branch
   * data source and its one known gap. Renders nothing when there's
   * nothing to show, rather than an empty labeled section. */
  function renderNotOnScreenRail(root) {
    if (notOnScreenStreams.length === 0) return;
    const rail = el("div", { className: "view-not-on-screen" });
    rail.appendChild(el("div", { className: "view-not-on-screen-label", text: "Not on screen" }));
    const chips = el("div", { className: "view-not-on-screen-chips" });
    notOnScreenStreams.forEach((item) => {
      chips.appendChild(
        el("div", { className: "view-not-on-screen-chip" }, [
          el("span", { text: `${item.ownerLabel} · ${item.roomLabel}` }),
          el(
            "button",
            {
              className: "view-not-on-screen-chip-play",
              attrs: { title: `Watch ${item.ownerLabel}`, "aria-label": `Watch ${item.ownerLabel}` },
              onClick: () => playNotOnScreen(item),
            },
            [icon("play_arrow")]
          ),
        ])
      );
    });
    rail.appendChild(chips);
    root.appendChild(rail);
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

    // No-chrome-by-default + hover overlay (1e spec): the name-plate pill is
    // always visible (bottom-left, minimal — just name · room), everything
    // else (room+resolution detail, selection checkbox, pop-out button, fps
    // stats) only fades in on hover/focus via `.media-tile-overlay`'s CSS.
    wrap.appendChild(el("div", { className: "media-tile-nameplate", text: `${tile.ownerLabel} · ${tile.roomLabel}` }));

    const overlay = el("div", { className: "media-tile-overlay" });

    const top = el("div", { className: "media-tile-overlay-top" });
    const titleText = tile.resolution ? `${tile.ownerLabel} · ${tile.roomLabel} · ${tile.resolution}` : `${tile.ownerLabel} · ${tile.roomLabel}`;
    top.appendChild(el("span", { className: "media-tile-overlay-title", text: titleText }));

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

    // Bottom bar — right-aligned fps stats (1e spec). Separate `<span>` so
    // `updateStats` (below, fed by the `stream-stats-updated` event) can
    // patch just this text node ~4x/sec without touching (and
    // re-flickering) the rest of the tile.
    const bottom = el("div", { className: "media-tile-overlay-bottom" });

    // Audio control — present only when this sender is actually sharing
    // audio, and *only* as a volume/mute knob on their video tile. There is
    // no "listen" toggle here on purpose: the audio is already playing by the
    // time this tile exists (the backend subscribed it together with the
    // video — `session.rs`'s `sync_audio_watches`), so the only thing left
    // for a viewer to want is "quieter" or "off". Rendered before the fps
    // span so it sits bottom-left, opposite the stats.
    if (tile.hasAudio) {
      const muted = tileMuted.has(tile.id);
      const audioGroup = el("div", { className: "media-tile-audio" + (muted ? " media-tile-audio-muted" : "") });
      const muteBtn = el(
        "button",
        {
          className: "media-tile-audio-btn",
          attrs: { title: muted ? "Unmute" : "Mute", "aria-label": muted ? `Unmute ${tile.ownerLabel}` : `Mute ${tile.ownerLabel}` },
          onClick: (e) => {
            e.stopPropagation();
            if (tileMuted.has(tile.id)) tileMuted.delete(tile.id);
            else tileMuted.add(tile.id);
            pushVolume(tile);
            render();
          },
        },
        [icon(muted ? "volume_off" : "volume_up")]
      );
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
        // Dragging the slider off 0 is an implicit unmute — the two controls
        // would otherwise contradict each other on screen.
        if (Number(slider.value) > 0) tileMuted.delete(tile.id);
        pushVolume(tile);
      });
      audioGroup.appendChild(muteBtn);
      audioGroup.appendChild(slider);
      bottom.appendChild(audioGroup);
    }

    const fpsSpan = el("span", { className: "media-tile-fps", text: tile.fps ? `@${tile.fps}fps` : "" });
    bottom.appendChild(fpsSpan);
    overlay.appendChild(bottom);
    if (tile.id) fpsSpans.set(tile.id, fpsSpan);

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
