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
// Known gap: no DTO carries a per-watched-stream "is this actually
// decoding right now" signal (that's the separate MJPEG-over-HTTP frame
// path, explicitly out of scope for this command/event wiring pass — see
// `driver.rs`'s module doc). Every real tile therefore renders in the
// `'live'` visual state; the `'stalled'` placeholder is only reachable via
// the mock branch's demo toggle until a later worker wires a real signal
// for it.

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
      const rooms = await window.__TAURI__.core.invoke("list_saved_rooms");
      const joined = rooms.filter((r) => r.state === "joined" && r.room_handle != null);
      const tiles = [];
      for (const room of joined) {
        const detail = await window.__TAURI__.core.invoke("room_detail", { roomHandle: room.room_handle });
        if (!detail) continue;
        for (const key of detail.watched) {
          const streamInfo = detail.streams.find((s) => s.owner === key.owner && s.stream_id === key.stream_id);
          const member = detail.members.find((m) => m.conn_id === key.owner);
          tiles.push({
            id: `${room.room_handle}:${key.owner}:${key.stream_id}`,
            room_handle: room.room_handle,
            owner: key.owner,
            stream_id: key.stream_id,
            ownerLabel: member ? member.display_name : String(key.owner),
            roomLabel: room.name,
            resolution: streamInfo ? `${streamInfo.width}x${streamInfo.height}` : null,
            fps: null,
            // See this file's header note: no real per-stream decode-health
            // signal is wired up yet, so every real tile is 'live'.
            state: "live",
            hue: Number(BigInt(key.owner) % 360n),
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
      // tile without a real capture/decoder. Real tiles render the same
      // placeholder gradient today — decoding actual frames onto this tile
      // is the separate MJPEG-over-HTTP mechanism (`driver.rs`'s module doc)
      // and is explicitly out of scope for this command/event wiring pass.
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

  window.ViewTab = { refresh: init };

  document.addEventListener("DOMContentLoaded", init);
})();
