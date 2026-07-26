// view.js — View tab content: the auto-tiled grid of watched streams.
//
// Mock-data-first, same `if (window.__TAURI__) {...} else {...}` pattern as
// stream-control.js: every data touchpoint is an `async function` with a
// real `invoke(...)` stub (not implemented — command names/signatures don't
// exist yet) on one branch and a mock-state fallback on the other, so this
// renders and is interactive in a plain browser with no Tauri shell present.
//
// Simplification vs. the full design handoff: the plan's tiered
// capture-restart placeholder (<800ms nothing / 800ms-6s spinner / >6s
// "Waiting for capture…" message) is collapsed to one placeholder state
// (spinner + message) shown whenever a tile's mock state is 'stalled'. See
// `superdoc/ui/APP-SHELL.md`'s update for the full `Why:`.

(function () {
  "use strict";

  const { el, icon, renderEmptyState } = window.Components;

  const MOCK_WATCHED = [
    { id: "w1", ownerLabel: "Ana", roomLabel: "Movie Night", resolution: "1920x1080", fps: 30, state: "live", hue: 220 },
    { id: "w2", ownerLabel: "Priya", roomLabel: "Book Club", resolution: "1280x720", fps: 60, state: "live", hue: 160 },
    { id: "w3", ownerLabel: "Sam", roomLabel: "Gaming Night", resolution: "1920x1080", fps: null, state: "stalled", hue: 0 },
  ];

  let mockWatched = [];

  async function fetchWatchedStreams() {
    if (window.__TAURI__) {
      return await window.__TAURI__.core.invoke("list_watched_streams"); // TODO: not implemented yet — backend worker's scope.
    }
    return MOCK_WATCHED.slice();
  }

  async function popOutWatch(id) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("pop_out_watch", { id }); // TODO: not implemented yet.
      return;
    }
    // Mock: no real second OS window in a plain browser; flag it locally so
    // a Playwright test can assert the click was registered.
    const tile = mockWatched.find((t) => t.id === id);
    if (tile) tile.poppedOut = true;
  }

  function render() {
    const root = document.getElementById("view-root");
    if (!root) return;
    root.innerHTML = "";

    const controls = el("div", { className: "view-controls" });
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
    root.appendChild(controls);

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
    } else {
      // Mock "video": a gradient standing in for a decoded frame, so a
      // plain-browser Playwright run has something visually distinct per
      // tile without a real capture/decoder.
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
    const labelText = tile.fps ? `${tile.ownerLabel} · ${tile.roomLabel} · ${tile.resolution}@${tile.fps}` : `${tile.ownerLabel} · ${tile.roomLabel}`;
    top.appendChild(el("span", { className: "media-tile-label", text: labelText }));
    top.appendChild(
      el(
        "button",
        {
          className: "media-tile-popout-btn",
          attrs: { title: "Pop out", "aria-label": "Pop out" },
          onClick: () => popOutWatch(tile.id),
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

  document.addEventListener("DOMContentLoaded", init);
})();
