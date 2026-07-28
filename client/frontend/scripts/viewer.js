// viewer.js — pop-out stream viewer window content.
//
// Loaded by `frontend/viewer.html`, which `electron/main.js`'s
// `open-popout-window` IPC handler (`openPopoutWindow`) points a dedicated
// `BrowserWindow` at via `win.loadFile(VIEWER_HTML, { search })`. Reads its
// stream list off that query string rather than an IPC message so it's
// available synchronously to this script's own startup, with no race
// against when this script attaches a listener (see `openPopoutWindow`'s
// doc comment in main.js).
//
// Deliberately not part of `frontend/index.html`'s tabbed app shell — this
// is a separate, focused page for exactly one purpose (show a stream,
// chrome-free), loaded into its own `BrowserWindow`. It reuses
// `tile-video.js`'s `window.TileVideo` for the actual decode pipeline (same
// module the View tab's `view.js` uses) so the two never drift out of
// sync, but owns its own grid layout / DOM / lifecycle independently of
// `view.js`.
//
// `window.__TAURI__`/`window.__ELECTRON__` are both available here — this
// window shares the exact same `preload.js` as the main window (see
// `main.js`'s `openPopoutWindow`, which passes the same `PRELOAD` path).
//
// One window per stream (`view.js`'s `popOutWatch` opens a new one, or
// focuses an existing one, per stream) — `streamKeys` below is still an
// array purely because the underlying tile-resolving/grid code stayed
// generic over N tiles (shared, already-tested code, no reason to narrow
// it to exactly one), but in practice every pop-out window today is opened
// with exactly one key.
//
// F11 toggles real OS fullscreen on this window (`setFullscreen` below);
// exiting fullscreen — via F11 again, Escape, or an OS-level fullscreen-exit
// gesture — never closes the window, only the explicit "×" button or the
// window's own OS close affordance does. See `setupExitControls`'s keydown
// handler and `superdoc/features/viewer-window.md`'s 2026-07-28 entry for
// why this used to be a bug (OS fullscreen exit and "close" were conflated).

(function () {
  "use strict";

  const params = new URLSearchParams(location.search);
  let streamKeys = [];
  try {
    streamKeys = JSON.parse(params.get("streamKeys") || "[]");
  } catch (e) {
    console.error("[viewer] failed to parse streamKeys query param:", e);
  }

  // Local mirror of this window's OS fullscreen state — pop-out windows
  // always open windowed (never fullscreen-at-open, unlike the old
  // fullscreen/borderless viewer), so this starts false. Kept in sync by
  // `setFullscreen` (our own toggles) and `onViewerLeftFullscreen` (an
  // OS-level exit this window didn't initiate itself).
  let isFullscreen = false;

  /** `room_handle:owner:stream_id` (this project's existing tile-id scheme,
   * see `view.js`'s `fetchWatchedStreams`) -> the three fields
   * `stream_video_url` needs. `owner` is a hex `ConnId` string with no
   * colons in it (see `dto::hex_u64`), so a plain 3-way split is safe. */
  function parseStreamKey(key) {
    const parts = key.split(":");
    if (parts.length !== 3) return null;
    const [roomHandleStr, owner, streamIdStr] = parts;
    return {
      id: key,
      roomHandle: Number(roomHandleStr),
      owner,
      streamId: Number(streamIdStr),
    };
  }

  function el(tag, opts, children) {
    const node = document.createElement(tag);
    opts = opts || {};
    if (opts.className) node.className = opts.className;
    if (opts.text !== undefined) node.textContent = opts.text;
    if (opts.attrs) {
      for (const [k, v] of Object.entries(opts.attrs)) node.setAttribute(k, v);
    }
    if (opts.onClick) node.addEventListener("click", opts.onClick);
    (children || []).forEach((c) => c && node.appendChild(c));
    return node;
  }

  /** Fetches the user's `max_jitter_buffer_ms` preference once at startup
   * and passes it to `tile-video.js`'s `window.TileVideo.setMaxJitterBufferMs`
   * — mirrors `view.js`'s `fetchMaxJitterBufferMs` exactly (same command,
   * same setter). This window loads `tile-video.js` fresh in its own JS
   * context (see the module doc comment above), so without this call it
   * silently falls back to that module's own `DEFAULT_MAX_JITTER_BUFFER_MS`
   * instead of the user's configured ceiling. No-op on the mock branch (no
   * `window.__TAURI__`), matching `view.js`. */
  async function fetchMaxJitterBufferMs() {
    if (!window.__TAURI__) return;
    const settings = await window.__TAURI__.core.invoke("get_settings");
    window.TileVideo.setMaxJitterBufferMs(settings.max_jitter_buffer_ms);
  }

  /** Resolves each parsed stream key to a video URL via the same
   * `stream_video_url` command the View tab's `fetchWatchedStreams` calls,
   * and builds a `{ id, videoUrl }`-shaped tile — the minimal shape
   * `tile-video.js`'s `startTileVideo` needs (see its doc comment). Skips
   * (with a console warning) any key that fails to resolve rather than
   * failing the whole window over one bad stream. */
  async function resolveTiles(keys) {
    const tiles = [];
    for (const rawKey of keys) {
      const parsed = parseStreamKey(rawKey);
      if (!parsed) {
        console.error(`[viewer] malformed stream key, skipping: ${rawKey}`);
        continue;
      }
      if (!window.__TAURI__) {
        // No backend bridge (e.g. this page opened directly in a plain
        // browser for a quick visual check) — nothing real to resolve.
        continue;
      }
      let videoUrl = null;
      try {
        videoUrl = await window.__TAURI__.core.invoke("stream_video_url", {
          roomHandle: parsed.roomHandle,
          owner: parsed.owner,
          streamId: parsed.streamId,
        });
      } catch (e) {
        console.error(`[viewer] stream_video_url failed for ${rawKey}:`, e);
      }
      if (!videoUrl) {
        console.error(`[viewer] no video URL resolved for ${rawKey}, skipping tile`);
        continue;
      }
      tiles.push({ id: parsed.id, videoUrl });
    }
    return tiles;
  }

  function renderTile(tile) {
    const wrap = el("div", { className: "viewer-tile" });
    const canvas = el("canvas", { attrs: { role: "img", "aria-label": tile.id } });
    const errorPlaceholder = el("div", { className: "viewer-tile-placeholder", attrs: { hidden: "hidden" } }, [el("span", { text: "Stream error" })]);
    wrap.appendChild(canvas);
    wrap.appendChild(errorPlaceholder);
    window.TileVideo.startTileVideo(tile, canvas, errorPlaceholder);
    return wrap;
  }

  function renderGrid(tiles) {
    const root = document.getElementById("viewer-root");
    root.innerHTML = "";

    if (tiles.length === 0) {
      root.appendChild(el("div", { className: "viewer-empty", text: "No streams to show." }));
      return;
    }

    // ceil(sqrt(n)) columns, enough rows to fit the rest — a reasonable
    // general-purpose grid for N tiles with no per-stream aspect-ratio
    // awareness (matches the Stream Control Redesign bundle's Watch-tab
    // layout math in spirit; doesn't need to be pixel-perfect to that spec
    // yet).
    const cols = Math.ceil(Math.sqrt(tiles.length));
    const rows = Math.ceil(tiles.length / cols);

    const grid = el("div", { className: "viewer-grid" });
    grid.style.gridTemplateColumns = `repeat(${cols}, 1fr)`;
    grid.style.gridTemplateRows = `repeat(${rows}, 1fr)`;
    tiles.forEach((tile) => grid.appendChild(renderTile(tile)));
    root.appendChild(grid);
  }

  /** Explicit "close this pop-out window" action — the × button only.
   * Never called as a side effect of a fullscreen-exit; see the module doc
   * comment and `setupExitControls` below. */
  async function closeViewer() {
    if (!window.__ELECTRON__) return; // plain-browser preview — nothing to call
    try {
      await window.__ELECTRON__.closeViewer();
    } catch (e) {
      console.error("[viewer] closeViewer() failed:", e);
    }
  }

  /** F11's toggle. `next`: the fullscreen state to move to. Updates the
   * local mirror optimistically (there's no round-trip event for our own
   * `setFullscreen(true)` calls, only for an OS-initiated exit — see
   * `onViewerLeftFullscreen` below), and reverts it if the IPC call itself
   * rejects. */
  async function setFullscreen(next) {
    if (!window.__ELECTRON__) return; // plain-browser preview — nothing to call
    isFullscreen = next;
    try {
      await window.__ELECTRON__.setFullscreen(next);
    } catch (e) {
      console.error("[viewer] setFullscreen failed:", e);
      isFullscreen = !next;
    }
  }

  function setupExitControls() {
    document.getElementById("viewer-root").before(
      el("button", {
        className: "viewer-exit-btn",
        text: "×",
        attrs: { type: "button", title: "Exit viewer", "aria-label": "Exit viewer" },
        onClick: () => closeViewer(),
      })
    );

    document.addEventListener("keydown", (e) => {
      if (e.key === "F11") {
        // Real OS fullscreen toggle for this pop-out window (task brief:
        // "add F11 for Fullscreen to it"). Never closes the window, in
        // either direction.
        e.preventDefault();
        setFullscreen(!isFullscreen);
      } else if (e.key === "Escape") {
        // Escape only ever exits fullscreen — it must NOT close the window
        // (task brief: "dont close it, when its fullscreen state is being
        // exited"; this mirrors the OS-level exit path below). When not
        // fullscreen, Escape does nothing here: a normal windowed pop-out
        // already has its own close affordances (the × button above, or
        // the OS window frame's own close control), and Escape closing it
        // too would be a surprising extra way to lose the window.
        if (isFullscreen) setFullscreen(false);
      }
    });

    // OS-level fullscreen exit (an Escape/WM shortcut handled by the
    // compositor rather than through our own `setFullscreen(false)` call
    // above) fires this. By the time it fires, the OS has *already* taken
    // the window out of fullscreen — this only syncs local state, it must
    // NOT close the window. Conflating "no longer fullscreen" with "close
    // the window" was the pre-existing bug this consolidation fixes; see
    // `superdoc/features/viewer-window.md`'s 2026-07-28 entry.
    if (window.__ELECTRON__ && window.__ELECTRON__.onViewerLeftFullscreen) {
      window.__ELECTRON__.onViewerLeftFullscreen(() => {
        isFullscreen = false;
      });
    }
  }

  // No leaked decoders/fetches when this window closes — every tile
  // controller `startTileVideo` created gets torn down here, mirroring
  // `view.js`'s `render()` full-rebuild teardown but on window unload
  // instead of on re-render.
  window.addEventListener("beforeunload", () => {
    window.TileVideo.stopAllTileVideo();
  });

  async function init() {
    setupExitControls();
    await fetchMaxJitterBufferMs();
    const tiles = await resolveTiles(streamKeys);
    renderGrid(tiles);
  }

  document.addEventListener("DOMContentLoaded", init);
})();
