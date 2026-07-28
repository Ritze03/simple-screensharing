// viewer.js — standalone fullscreen/borderless viewer window content.
//
// Loaded by `frontend/viewer.html`, which `electron/main.js`'s
// `open-viewer-window` IPC handler (`openViewerWindow`/`loadViewerContents`)
// points a dedicated `BrowserWindow` at via `win.loadFile(VIEWER_HTML, {
// search })`. Reads its stream list and mode off that query string rather
// than an IPC message so it's available synchronously to this script's own
// startup, with no race against when this script attaches a listener (see
// `loadViewerContents`'s doc comment in main.js).
//
// Deliberately not part of `frontend/index.html`'s tabbed app shell — this
// is a separate, focused page for exactly one purpose (show N streams full
// screen), loaded into its own `BrowserWindow`. It reuses `tile-video.js`'s
// `window.TileVideo` for the actual decode pipeline (same module the View
// tab's `view.js` uses) so the two never drift out of sync, but owns its
// own grid layout / DOM / lifecycle independently of `view.js`.
//
// `window.__TAURI__`/`window.__ELECTRON__` are both available here — this
// window shares the exact same `preload.js` as the main window (see
// `main.js`'s `openViewerWindow`, which passes the same `PRELOAD` path).

(function () {
  "use strict";

  const params = new URLSearchParams(location.search);
  const mode = params.get("mode") || "borderless"; // 'fullscreen' | 'borderless'
  let streamKeys = [];
  try {
    streamKeys = JSON.parse(params.get("streamKeys") || "[]");
  } catch (e) {
    console.error("[viewer] failed to parse streamKeys query param:", e);
  }

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

  /** Escape / exit-button / OS-fullscreen-exit all funnel through here.
   * `fromOsFullscreenExit`: true when called from `onViewerLeftFullscreen`
   * — the OS has *already* taken the window out of fullscreen by the time
   * that event fires, so calling `setFullscreen(false)` again would be a
   * redundant no-op; skip straight to closing. */
  async function exitViewer(fromOsFullscreenExit) {
    if (!window.__ELECTRON__) {
      // No Electron bridge (plain-browser preview) — nothing to call.
      return;
    }
    if (mode === "fullscreen" && !fromOsFullscreenExit) {
      try {
        await window.__ELECTRON__.setFullscreen(false);
      } catch (e) {
        console.error("[viewer] setFullscreen(false) failed:", e);
      }
    }
    try {
      await window.__ELECTRON__.closeViewer();
    } catch (e) {
      console.error("[viewer] closeViewer() failed:", e);
    }
  }

  function setupExitControls() {
    document.getElementById("viewer-root").before(
      el("button", {
        className: "viewer-exit-btn",
        text: "×",
        attrs: { type: "button", title: "Exit viewer", "aria-label": "Exit viewer" },
        onClick: () => exitViewer(false),
      })
    );

    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape") exitViewer(false);
    });

    if (window.__ELECTRON__ && window.__ELECTRON__.onViewerLeftFullscreen) {
      window.__ELECTRON__.onViewerLeftFullscreen(() => exitViewer(true));
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
    const tiles = await resolveTiles(streamKeys);
    renderGrid(tiles);
  }

  document.addEventListener("DOMContentLoaded", init);
})();
