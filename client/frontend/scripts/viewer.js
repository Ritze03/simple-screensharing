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

  // Functional-parity metadata (2026-07-28) — `ownerLabel`/`roomLabel`/
  // `hasAudio`/`resolution` for the (exactly one, in practice) tile this
  // window shows, passed straight through by `view.js`'s `popOutWatch` via
  // `electron/main.js`'s `openPopoutWindow` rather than re-derived here
  // (that would mean re-fetching `room_detail` and re-joining against
  // `members`/`streams`, logic `view.js`'s `fetchWatchedStreams` already
  // did once). Absent entirely in a plain-browser preview (no query
  // params) or the mock branch, in which case the tile just renders with no
  // nameplate/overlay — see `renderTile`'s label guard below.
  const tileMeta = {
    ownerLabel: params.get("ownerLabel") || null,
    roomLabel: params.get("roomLabel") || null,
    hasAudio: params.get("hasAudio") === "1",
    resolution: params.get("resolution") || null,
  };

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

  /** A single Material Symbols glyph — mirrors `components.js`'s `icon()`.
   * Not `window.Components.icon` itself: this standalone window doesn't
   * load `components.js` (a whole app-shell component library for one
   * glyph helper is more than this lean window needs), just `fonts.css`
   * for the underlying `@font-face`. */
  function icon(name) {
    return el("span", { className: "material-symbols-outlined", text: name });
  }

  // Per-tile playback volume/mute — mirrors `view.js`'s own `tileVolume`/
  // `tileMuted`/`DEFAULT_VOLUME`/`pushVolume` exactly (same range, same
  // `set_volume` contract), kept as this window's own copy rather than a
  // shared module because there's only ever one tile here and no re-render
  // cadence to survive across (unlike `view.js`, this window never tears
  // down and rebuilds its tile on a timer).
  //
  // 2026-07-29: seeded from the backend's real state (`commands::
  // get_volume`) inside `resolveTiles` below, before this window ever
  // renders its tile — this is the pop-out-open half of the cross-window
  // volume sync fix; see `view.js`'s matching `tileVolume` doc comment for
  // the full picture (this window resuming from the View tab's tile) and
  // `ensureVolumeSeeded` there for the other direction (the View tab's tile
  // resuming from this window's changes once it closes).
  const tileVolume = new Map();
  const tileMuted = new Set();
  const DEFAULT_VOLUME = 100;

  /** Push this tile's volume/mute state to the backend. Sends the *desired*
   * slider value even while muted — see `view.js`'s `pushVolume` doc
   * comment on why `muted` is now its own explicit flag rather than folded
   * into a zeroed volume. */
  async function pushVolume(tile) {
    if (!window.__TAURI__ || tile.roomHandle == null) return;
    const muted = tileMuted.has(tile.id);
    const percent = tileVolume.get(tile.id) ?? DEFAULT_VOLUME;
    try {
      await window.__TAURI__.core.invoke("set_volume", {
        roomHandle: tile.roomHandle,
        owner: tile.owner,
        streamId: tile.streamId,
        volumePercent: percent,
        muted,
      });
    } catch (e) {
      console.error("[viewer] set_volume failed", e);
    }
  }

  /** Fetches the user's `max_jitter_buffer_ms`/`catch_up_speed_percent`
   * preferences once at startup and passes them to `tile-video.js`'s
   * `window.TileVideo.setMaxJitterBufferMs`/`setCatchUpSpeedPercent` —
   * mirrors `view.js`'s `fetchMaxJitterBufferMs` exactly (same commands,
   * same setters). This window loads `tile-video.js` fresh in its own JS
   * context (see the module doc comment above), so without this call it
   * silently falls back to that module's own
   * `DEFAULT_MAX_JITTER_BUFFER_MS`/`DEFAULT_CATCH_UP_SPEED_PERCENT` instead
   * of the user's configured values. No-op on the mock branch (no
   * `window.__TAURI__`), matching `view.js`. */
  async function fetchMaxJitterBufferMs() {
    if (!window.__TAURI__) return;
    const settings = await window.__TAURI__.core.invoke("get_settings");
    window.TileVideo.setMaxJitterBufferMs(settings.max_jitter_buffer_ms);
    window.TileVideo.setCatchUpSpeedPercent(settings.catch_up_speed_percent);
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
      // Seed this tile's volume/mute state from the backend's real value
      // (`commands::get_volume`) before `renderTile` ever builds its audio
      // pill — this is what shows e.g. 40% here instead of `DEFAULT_VOLUME`
      // when a tile already adjusted in the View tab gets popped out. Runs
      // before `renderGrid` (see `init()` below, which awaits this whole
      // function first), so there's no render-then-patch needed the way
      // `view.js`'s `ensureVolumeSeeded` needs one (that window's tiles can
      // already be on screen by the time a fetch resolves).
      if (tileMeta.hasAudio) {
        try {
          const state = await window.__TAURI__.core.invoke("get_volume", {
            roomHandle: parsed.roomHandle,
            owner: parsed.owner,
            streamId: parsed.streamId,
          });
          if (state) {
            tileVolume.set(parsed.id, state.volume_percent);
            if (state.muted) tileMuted.add(parsed.id);
          }
        } catch (e) {
          console.error(`[viewer] get_volume failed for ${rawKey}:`, e);
        }
      }
      tiles.push({
        id: parsed.id,
        videoUrl,
        roomHandle: parsed.roomHandle,
        owner: parsed.owner,
        streamId: parsed.streamId,
        // See `tileMeta`'s doc comment above — same fields for every tile in
        // this window since every pop-out window shows exactly one stream in
        // practice, even though this loop stays generic over N keys.
        ownerLabel: tileMeta.ownerLabel,
        roomLabel: tileMeta.roomLabel,
        hasAudio: tileMeta.hasAudio,
        resolution: tileMeta.resolution,
      });
    }
    return tiles;
  }

  /** Builds the whole `.media-tile-audio` mute+volume pill, appended into
   * `bottom` and swapped in place on every mute toggle — mirrors `view.js`'s
   * `renderTile`'s own `buildAudioGroup` closure exactly (same swap-in-
   * place-on-mute-toggle approach, same reason: only this pill needs to
   * change, not the whole tile/overlay; same `.media-tile-audio*` class
   * names too now — 2026-07-29, see viewer.css's header note — so this and
   * the View tab's version render identically). */
  function appendAudioGroup(tile, bottom) {
    const buildAudioGroup = (muted) => {
      const group = el("div", { className: "media-tile-audio" + (muted ? " media-tile-audio-muted" : "") });
      const muteBtn = el(
        "button",
        {
          className: "media-tile-audio-btn",
          attrs: { title: muted ? "Unmute" : "Mute", "aria-label": muted ? "Unmute" : "Mute" },
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
          attrs: { type: "range", min: "0", max: "200", step: "5", "aria-label": "Volume" },
        });
        slider.value = String(tileVolume.get(tile.id) ?? DEFAULT_VOLUME);
        slider.addEventListener("click", (e) => e.stopPropagation());
        slider.addEventListener("input", () => {
          tileVolume.set(tile.id, Number(slider.value));
          pushVolume(tile);
        });
        // Release focus once a mouse-driven drag ends (`mouseup`, not every
        // `input` tick) — this window's tile fills virtually the whole
        // viewport with nothing else to focus, so a slider left focused
        // after the mouse moves away keeps `.viewer-tile:focus-within`
        // (viewer.css) true forever, pinning `.media-tile-overlay`/
        // `-vignette` visible even once the mouse is nowhere near the tile
        // (confirmed live: `document.activeElement` stayed this slider,
        // both opacities stuck at 1, after a drag + mouse-away). `mouseup`
        // rather than blurring on every `input` event so a keyboard user
        // who tabs to the slider (no `mouseup` fires for that) keeps it
        // focused and visible — the accessibility case `:focus-within`
        // exists for in the first place is unaffected (no `mouseup` fires
        // for a keyboard interaction, so this listener never blurs a
        // tabbed-to slider); only the mouse-drag-then-walk-away case, which
        // has no such justification in a single-tile window, gets released.
        slider.addEventListener("mouseup", () => slider.blur());

        group.appendChild(slider);
      }
      return group;
    };
    let audioGroup = buildAudioGroup(tileMuted.has(tile.id));
    bottom.appendChild(audioGroup);
  }

  // `tile.id` -> its statbar's live fps/bitrate `<span>`s — mirrors
  // `view.js`'s own `fpsSpans`/`bitrateSpans` module-level maps exactly (see
  // `updateStats` below, fed the same `stream-stats-updated` event `view.js`
  // consumes). Only ever holds the one tile this window actually shows in
  // practice, but kept keyed by id rather than a single variable so it stays
  // consistent with `resolveTiles`'s generic-over-N-keys shape.
  const fpsSpans = new Map();
  const bitrateSpans = new Map();

  /** Same info this stream's tile shows in the main View tab: an
   * always-visible `.media-tile-statbar` (dot/name/resolution/fps/bitrate/
   * audio badge) plus, on hover, the `.media-tile-vignette` scrim and
   * `.media-tile-overlay`'s audio mute/volume control — the actual
   * `.media-tile-*` DOM `view.js`'s `renderTile` builds (see viewer.html's
   * header note on why: linking the real CSS instead of hand-copying it
   * only pays off if the DOM shape matches it too). Two deliberate
   * differences from the View tab's overlay-top, both because this window
   * IS the pop-out already:
   *   - no pop-out button (can't pop out again from inside the pop-out)
   *   - no "stop watching" button — closing this window (the × button, or
   *     the OS close affordance) already fully stops watching this stream,
   *     so a second control that does the same thing would be redundant.
   *     `overlay-top` is simply never appended here.
   * Renders nothing beyond the bare video when `tileMeta` came back empty
   * (plain-browser preview with no query params, or a malformed one) — same
   * guard `view.js` doesn't need (it always has real tile data) but this
   * window's bare-video fallback does. */
  function renderTile(tile) {
    const wrap = el("div", { className: "viewer-tile" });
    // Deliberately NOT `.media-tile-video` — that class is `object-fit:
    // cover` (fills the View tab's fixed-aspect grid cell, cropping).
    // `.viewer-tile canvas` below already gives this window's canvas its
    // own `object-fit: contain` letterbox treatment, which is the right one
    // for a full-bleed single-stream stage.
    const canvas = el("canvas", { attrs: { role: "img", "aria-label": tile.ownerLabel || tile.id } });
    const errorPlaceholder = el("div", { className: "viewer-tile-placeholder", attrs: { hidden: "hidden" } }, [el("span", { text: "Stream error" })]);
    wrap.appendChild(canvas);
    wrap.appendChild(errorPlaceholder);

    const label = tile.ownerLabel ? (tile.roomLabel ? `${tile.ownerLabel} · ${tile.roomLabel}` : tile.ownerLabel) : null;
    if (label) {
      const statbar = el("div", { className: "media-tile-statbar" });
      statbar.appendChild(el("span", { className: "media-tile-statbar-dot" }));
      statbar.appendChild(el("span", { className: "media-tile-statbar-name", text: label }));
      if (tile.resolution) {
        statbar.appendChild(el("span", { className: "media-tile-statbar-sep", text: "·" }));
        statbar.appendChild(el("span", { className: "media-tile-statbar-value", text: tile.resolution }));
      }
      const fpsSpan = el("span", { className: "media-tile-statbar-value", text: "" });
      statbar.appendChild(fpsSpan);
      fpsSpans.set(tile.id, fpsSpan);
      const bitrateSpan = el("span", { className: "media-tile-statbar-value", text: "" });
      statbar.appendChild(bitrateSpan);
      bitrateSpans.set(tile.id, bitrateSpan);
      if (tile.hasAudio) {
        const audioBadge = el("span", { className: "media-tile-statbar-audio-badge", attrs: { title: "Sharing audio", "aria-label": "Sharing audio" } });
        audioBadge.appendChild(icon("volume_up"));
        statbar.appendChild(audioBadge);
      }
      wrap.appendChild(statbar);

      const vignette = el("div", { className: "media-tile-vignette" });
      const overlay = el("div", { className: "media-tile-overlay" });

      // Empty top spacer, mirroring `view.js`'s `renderTile` exactly — with
      // only one flex child, `.media-tile-overlay`'s `justify-content:
      // space-between` collapses to `flex-start` and the lone child renders
      // at the TOP instead of the bottom. A second (empty) flex child
      // restores the two-item distribution this window has no other use
      // for (see the doc comment above: no pop-out/stop-watching buttons
      // belong in here), without needing to touch `view.css`'s shared rule.
      const top = el("div", { className: "media-tile-overlay-top" });
      const bottom = el("div", { className: "media-tile-overlay-bottom" });
      if (tile.hasAudio) appendAudioGroup(tile, bottom);
      overlay.appendChild(top);
      overlay.appendChild(bottom);

      wrap.appendChild(vignette);
      wrap.appendChild(overlay);

      tile.onFps = (fps) => {
        fpsSpan.textContent = `${Math.round(fps)}fps`;
      };
    }

    window.TileVideo.startTileVideo(tile, canvas, errorPlaceholder);
    return wrap;
  }

  /** Live bitrate for the statbar — mirrors `view.js`'s own `updateStats`
   * (same `stream-stats-updated` payload shape, same `room_handle:owner:
   * stream_id` id scheme). Unlike `view.js`, this window has no `shell.js`
   * loaded to fan the event out for it, so it listens for the Tauri event
   * itself (see `init` below) — both windows share the same `preload.js`,
   * so `window.__TAURI__.event` is available here too. Fps is not patched
   * here: `tile.onFps` above (fed by this window's own `TileVideo`
   * controller) already owns that span exclusively, same as `view.js`'s
   * `TileVideo.hasController` skip. No-op on the mock branch (no
   * `window.__TAURI__`, so this listener is never registered at all). */
  function updateStats(payload) {
    if (!payload || !Array.isArray(payload.watched)) return;
    for (const w of payload.watched) {
      const id = `${w.room_handle}:${w.owner}:${w.stream_id}`;
      const bitrateSpan = bitrateSpans.get(id);
      if (bitrateSpan) {
        bitrateSpan.textContent =
          w.received_bitrate_kbps >= 1000 ? `${(w.received_bitrate_kbps / 1000).toFixed(1)} Mbps` : `${Math.round(w.received_bitrate_kbps)} kbps`;
      }
    }
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

  /** Subscribes this window to the same `stream-stats-updated` event
   * `shell.js` fans out to `view.js`'s `updateStats` in the main window —
   * this window has no `shell.js`, so it listens directly. Both windows
   * share the same `preload.js` (see the module doc comment), so
   * `window.__TAURI__.event.listen` is available here unchanged. No-op on
   * the mock branch, matching every other `window.__TAURI__`-gated call in
   * this file. */
  function listenForStats() {
    if (!window.__TAURI__) return;
    window.__TAURI__.event.listen("stream-stats-updated", (event) => updateStats(event.payload));
  }

  async function init() {
    setupExitControls();
    listenForStats();
    await fetchMaxJitterBufferMs();
    const tiles = await resolveTiles(streamKeys);
    renderGrid(tiles);
  }

  document.addEventListener("DOMContentLoaded", init);
})();
