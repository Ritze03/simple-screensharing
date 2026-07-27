// shell.js — app shell behavior: nav tab switching, ROOMS/STREAMS rendering,
// room-row expand/collapse, "+ Add Room" inline card, leave-confirm modal.
//
// Scope note: this is the FRONTEND SHELL (see superdoc/ui/APP-SHELL.md, stage
// 1) plus its Tauri wiring. Every backend touchpoint below is a small
// `async function` that checks `window.__TAURI__` and calls the real
// `#[tauri::command]` on one branch, falling back to mutating an in-memory
// mock array on the other — see `superdoc/ui/FRONTEND-SHELL.md`'s "mock/real
// fallback pattern". The mock branch's data shapes deliberately mirror the
// real DTOs field-for-field (snake_case `join_string`/`room_handle`/
// `active_stream_count`/…, matching `dto::SavedRoomRow`) so `renderRoomRow`/
// `renderStreamRow` work identically fed either source.
//
// Per APP-SHELL.md's hard rule ("the nav is a pure view-switcher"): this
// script only ever toggles which tab panel is visible. It never starts or
// stops a share/watch/subscription as a side effect of switching tabs.
//
// This module also owns the one place every Tauri event listener is
// registered (`wireEvents`, called once from `init`) — `room-connected`,
// `room-connect-failed`, `room-disconnected`, `presence-updated`,
// `destination-state-changed`, `bandwidth-overshoot`, `stream-stats-updated`
// — since the sidebar's ROOMS/STREAMS sections are the shell's own state and
// the other tabs (`stream-control.js`/`view.js`) expose a small `refresh()`
// this module calls into rather than duplicating seven `listen()` calls per
// file.

(function () {
  "use strict";

  const { renderNavButton, renderRoomRow, renderStreamRow, renderEmptyState, secondaryButton, primaryButton, el } = window.Components;

  const NAV_TABS = ["Stream Control", "View", "Settings"];

  // ---------------------------------------------------------------------
  // Mock data — shapes match the real DTOs (`dto::SavedRoomRow` /
  // `dto::DestinationRow`) field-for-field, per FRONTEND-SHELL.md's mock/real
  // parity rule, plus a frontend-only `members` array on room objects (no
  // single DTO inlines room + roster + live watch state — that's a client-side
  // join of `room_detail`'s `members`/`streams`/`watched`, done in
  // `loadRoomMembers` below for the real branch).
  // ---------------------------------------------------------------------

  let mockRooms = [
    {
      join_string: "server|11111111|movie-night-key",
      name: "Movie Night",
      state: "joined",
      room_handle: 1,
      active_stream_count: 1,
      members: [{ conn_id: 1, display_name: "Ana", owner: 1, stream_id: 0, resolution: "1920x1080", watching: false }],
    },
    {
      join_string: "server|22222222|gaming-night-key",
      name: "Gaming Night",
      state: "joined",
      room_handle: 2,
      active_stream_count: 0,
      members: [],
    },
    {
      join_string: "server|33333333|old-room-key",
      name: "Old Room",
      state: "disconnected",
      room_handle: null,
      active_stream_count: 0,
      members: [],
    },
    {
      join_string: "server|44444444|study-group-key",
      name: "Study Group",
      state: "reconnecting",
      room_handle: null,
      active_stream_count: 0,
      members: [],
    },
  ];

  let mockStreams = [{ destination_id: 1, capture_id: 1, room_handle: 1, kind: "video", room_label: "Movie Night", width: 1920, height: 1080, framerate: 30 }];

  // ---------------------------------------------------------------------
  // UI state (frontend-only; not persisted, matches the "no cross-session
  // persistence of active state" rule from the multi-room brainstorm doc).
  // ---------------------------------------------------------------------

  const state = {
    activeTab: "Stream Control",
    expandedRoomIds: new Set([mockRooms[0].join_string]), // Movie Night starts expanded, matching the reference shot.
    addRoomOpen: false,
    pendingLeaveRoomId: null,
  };

  // ---------------------------------------------------------------------
  // Data touchpoints
  // ---------------------------------------------------------------------

  async function fetchRooms() {
    if (window.__TAURI__) {
      return await window.__TAURI__.core.invoke("list_saved_rooms");
    }
    return mockRooms;
  }

  async function fetchStreams() {
    if (window.__TAURI__) {
      const rows = await window.__TAURI__.core.invoke("list_active_streams");
      rows.forEach((row) => (row.room_label = roomLabelFor(row.room_handle)));
      return rows;
    }
    return mockStreams;
  }

  function roomLabelFor(roomHandle) {
    const room = mockRooms.find((r) => r.room_handle === roomHandle);
    return room ? room.name : String(roomHandle);
  }

  /** Populates one room's `members` from `room_detail` — the real DTO
   * doesn't inline a roster onto `list_saved_rooms`, so this is a lazy,
   * per-room fetch on expand (mirrors `stream-control.js`'s room-scoped
   * members card, which does the same join). */
  async function loadRoomMembers(room) {
    if (!window.__TAURI__ || room.state !== "joined" || room.room_handle == null) return;
    const detail = await window.__TAURI__.core.invoke("room_detail", { roomHandle: room.room_handle });
    if (!detail) return;
    const watchedOwners = new Set(detail.watched.map((k) => `${k.owner}:${k.stream_id}`));
    room.members = detail.members.map((m) => {
      const videoStream = detail.streams.find((s) => s.owner === m.conn_id && s.kind === "Video");
      return {
        conn_id: m.conn_id,
        display_name: m.display_name,
        owner: m.conn_id,
        stream_id: videoStream ? videoStream.stream_id : 0,
        resolution: videoStream ? `${videoStream.width}x${videoStream.height}` : null,
        watching: videoStream ? watchedOwners.has(`${videoStream.owner}:${videoStream.stream_id}`) : false,
      };
    });
  }

  // ---------------------------------------------------------------------
  // Nav
  // ---------------------------------------------------------------------

  function renderNav() {
    const nav = document.getElementById("nav");
    nav.innerHTML = "";
    NAV_TABS.forEach((tab) => {
      nav.appendChild(renderNavButton(tab, state.activeTab === tab, () => setActiveTab(tab)));
    });
  }

  function setActiveTab(tab) {
    state.activeTab = tab;
    renderNav();
    document.querySelectorAll(".tab-panel").forEach((panel) => {
      panel.hidden = panel.dataset.tab !== tab;
    });
  }

  // ---------------------------------------------------------------------
  // ROOMS section
  // ---------------------------------------------------------------------

  function renderRooms() {
    const list = document.getElementById("rooms-list");
    list.innerHTML = "";

    if (mockRooms.length === 0) {
      list.appendChild(el("p", { className: "rooms-empty-hint", text: "No rooms yet — add one below." }));
      return;
    }

    mockRooms.forEach((room) => {
      list.appendChild(
        renderRoomRow(room, {
          expanded: state.expandedRoomIds.has(room.join_string),
          onToggleExpand: async (joinString) => {
            if (state.expandedRoomIds.has(joinString)) {
              state.expandedRoomIds.delete(joinString);
            } else {
              state.expandedRoomIds.add(joinString);
              const r = mockRooms.find((x) => x.join_string === joinString);
              if (r) await loadRoomMembers(r);
            }
            renderRooms();
          },
          onReconnect: async (joinString) => {
            if (window.__TAURI__) {
              await window.__TAURI__.core.invoke("join_room", { joinString, displayName: "" });
              await refreshRooms();
              return;
            }
            const r = mockRooms.find((x) => x.join_string === joinString);
            if (r) r.state = "reconnecting";
            renderRooms();
          },
          onLeave: (joinString) => openLeaveConfirm(joinString),
          onWatch: async (joinString, member) => {
            const room = mockRooms.find((x) => x.join_string === joinString);
            if (!room) return;
            if (window.__TAURI__) {
              await window.__TAURI__.core.invoke("toggle_watch", { roomHandle: room.room_handle, owner: member.owner, streamId: member.stream_id });
              await loadRoomMembers(room);
              if (window.ViewTab) window.ViewTab.refresh();
            } else {
              const m = room.members.find((mm) => mm.conn_id === member.conn_id);
              if (m) m.watching = !m.watching;
            }
            renderRooms();
          },
        })
      );
    });
  }

  function openLeaveConfirm(joinString) {
    const room = mockRooms.find((r) => r.join_string === joinString);
    if (!room) return;

    // Per APP-SHELL.md: leaving raises a confirm dialog iff the room has an
    // outgoing destination or a live watch; a leave with neither is
    // immediate. Mock approximation for the no-Tauri path: joined rooms with
    // active_stream_count > 0, or any watched member, require confirmation.
    // The real path asks the backend directly via `leave_needs_confirm`,
    // which is the source of truth (see `commands.rs`'s doc comment on that
    // command: exposed as its own query precisely so the frontend can decide
    // whether to show a dialog *before* calling `leave_room`).
    if (window.__TAURI__) {
      if (room.state !== "joined" || room.room_handle == null) {
        leaveRoom(joinString);
        return;
      }
      window.__TAURI__.core.invoke("leave_needs_confirm", { roomHandle: room.room_handle }).then((needsConfirm) => {
        if (needsConfirm) {
          state.pendingLeaveRoomId = joinString;
          renderLeaveModal();
        } else {
          leaveRoom(joinString);
        }
      });
      return;
    }

    const hasStake = room.active_stream_count > 0 || (room.members || []).some((m) => m.watching);
    if (room.state === "joined" && hasStake) {
      state.pendingLeaveRoomId = joinString;
      renderLeaveModal();
    } else {
      leaveRoom(joinString);
    }
  }

  /** Leave with no confirm needed (`leave_needs_confirm` already said `false`,
   * or there's no Tauri backend at all). */
  async function leaveRoom(joinString) {
    const room = mockRooms.find((r) => r.join_string === joinString);
    if (window.__TAURI__ && room && room.room_handle != null) {
      // `leave_room` returns `false` if a confirm was actually needed and it
      // did *not* tear the room down. Every call site here only reaches
      // `leaveRoom` after `leave_needs_confirm` already said `false` (or for
      // a not-yet-joined row, where there's nothing to lose), so that return
      // value is not acted on — the modal's own "Leave" button calls
      // `forceLeaveRoom` (`force_leave_room`) instead, unconditionally.
      await window.__TAURI__.core.invoke("leave_room", { roomHandle: room.room_handle });
      await refreshRooms();
      return;
    }
    mockRooms = mockRooms.filter((r) => r.join_string !== joinString);
    renderRooms();
  }

  /** Leave after the confirm modal was shown and the user confirmed —
   * unconditional teardown via `force_leave_room`. */
  async function forceLeaveRoom(joinString) {
    const room = mockRooms.find((r) => r.join_string === joinString);
    if (window.__TAURI__ && room && room.room_handle != null) {
      await window.__TAURI__.core.invoke("force_leave_room", { roomHandle: room.room_handle });
      await refreshRooms();
      return;
    }
    mockRooms = mockRooms.filter((r) => r.join_string !== joinString);
    renderRooms();
  }

  function renderLeaveModal() {
    const overlay = document.getElementById("modal-overlay");
    overlay.innerHTML = "";
    if (!state.pendingLeaveRoomId) {
      overlay.hidden = true;
      return;
    }
    const room = mockRooms.find((r) => r.join_string === state.pendingLeaveRoomId);
    if (!room) {
      overlay.hidden = true;
      return;
    }

    const watchingCount = (room.members || []).filter((m) => m.watching).length;
    const modal = el("div", { className: "modal" });
    modal.appendChild(el("div", { className: "modal-title", text: "Leave room" }));
    modal.appendChild(el("div", { className: "modal-heading", text: `Leave ${room.name}?` }));
    modal.appendChild(
      el("div", {
        className: "modal-body",
        text:
          watchingCount > 0
            ? `You are watching ${watchingCount} stream(s) here — leaving closes them.`
            : `You have an active outgoing share in this room — leaving stops it.`,
      })
    );
    const actions = el("div", { className: "modal-actions" });
    actions.appendChild(
      el("button", {
        className: "btn btn-danger",
        text: "Leave",
        onClick: async () => {
          const joinString = state.pendingLeaveRoomId;
          state.pendingLeaveRoomId = null;
          renderLeaveModal();
          await forceLeaveRoom(joinString);
        },
      })
    );
    actions.appendChild(
      secondaryButton("Cancel", () => {
        state.pendingLeaveRoomId = null;
        renderLeaveModal();
      })
    );
    modal.appendChild(actions);
    overlay.appendChild(modal);
    overlay.hidden = false;
  }

  // ---------------------------------------------------------------------
  // "+ Add Room" inline card
  // ---------------------------------------------------------------------

  function renderAddRoomCard() {
    const card = document.getElementById("add-room-card");
    const btn = document.getElementById("add-room-btn");
    card.hidden = !state.addRoomOpen;
    btn.hidden = state.addRoomOpen;
    if (!state.addRoomOpen) return;

    card.innerHTML = "";
    card.appendChild(el("div", { className: "section-label add-room-label", text: "Add a room" }));

    card.appendChild(el("label", { className: "field-label", text: "Display name" }));
    const nameInput = el("input", { className: "field-input", attrs: { type: "text", placeholder: "e.g. mo" } });
    card.appendChild(nameInput);

    card.appendChild(el("label", { className: "field-label", text: "Join link (server|room-id|key)" }));
    const linkInput = el("input", { className: "field-input", attrs: { type: "text", placeholder: "" } });
    card.appendChild(linkInput);

    const errorText = el("div", { className: "add-room-error", text: "" });
    errorText.hidden = true;
    card.appendChild(errorText);

    const actions = el("div", { className: "add-room-actions" });
    actions.appendChild(
      primaryButton("Join", async () => {
        const joinString = linkInput.value.trim();
        if (!joinString) return;

        if (window.__TAURI__) {
          errorText.hidden = true;
          try {
            await window.__TAURI__.core.invoke("join_room", { joinString, displayName: nameInput.value.trim() });
            // On success: `join_room` upserts into RoomStore and the room
            // appears live via the `room-connected` event (no staging step,
            // per APP-SHELL.md — "success saves"). `refreshRooms` here is a
            // belt-and-braces immediate paint in case the event hasn't
            // landed yet by the time this await resolves.
            state.addRoomOpen = false;
            await refreshRooms();
            renderAddRoomCard();
          } catch (e) {
            errorText.textContent = String(e);
            errorText.hidden = false;
          }
          return;
        }

        mockRooms.push({
          join_string: joinString,
          name: nameInput.value.trim() || "Room " + Math.random().toString(16).slice(2, 6),
          state: "joined",
          room_handle: Math.floor(Math.random() * 100000),
          active_stream_count: 0,
          members: [],
        });
        state.addRoomOpen = false;
        renderRooms();
        renderAddRoomCard();
      })
    );
    actions.appendChild(
      secondaryButton("Cancel", () => {
        state.addRoomOpen = false;
        renderAddRoomCard();
      })
    );
    card.appendChild(actions);
  }

  // ---------------------------------------------------------------------
  // STREAMS section
  // ---------------------------------------------------------------------

  function renderStreams() {
    const list = document.getElementById("streams-list");
    list.innerHTML = "";
    if (mockStreams.length === 0) {
      list.appendChild(
        renderEmptyState("You're not sharing anything.", "+ Start a Stream", () => {
          setActiveTab("Stream Control");
        })
      );
      return;
    }
    mockStreams.forEach((s) => list.appendChild(renderStreamRow(s)));
  }

  // Dev-only affordance so both the populated and empty STREAMS states are
  // reachable for visual review without editing mock data by hand. Not a
  // real feature — only wired in the mock (no-Tauri) path.
  function wireStreamsDemoToggle() {
    const toggle = document.getElementById("streams-demo-toggle");
    if (window.__TAURI__) {
      toggle.hidden = true;
      return;
    }
    toggle.addEventListener("click", () => {
      mockStreams = mockStreams.length === 0 ? [{ destination_id: 1, capture_id: 1, room_handle: 1, kind: "video", room_label: "Movie Night", width: 1920, height: 1080, framerate: 30 }] : [];
      renderStreams();
    });
  }

  // ---------------------------------------------------------------------
  // Refresh — re-fetch + re-render ROOMS/STREAMS. Exposed on `window.Shell`
  // so the event listeners below (and other tabs, e.g. after starting a
  // share) can trigger a sidebar repaint without duplicating the fetch call.
  // ---------------------------------------------------------------------

  async function refreshRooms() {
    mockRooms = await fetchRooms();
    // `list_saved_rooms`/`fetchRooms()` returns bare `SavedRoomRow`s with no
    // roster (see `loadRoomMembers`'s own doc comment) — every already-
    // expanded room's `.members` from the previous render is gone from these
    // fresh objects. Without this, any event that calls `refreshRooms()`
    // (this fires on `presence-updated`, i.e. routinely) would blank an
    // expanded room's member list back to "No other members yet." even
    // though the roster hasn't actually changed. Re-fetch it for exactly the
    // rooms currently expanded so the visible roster survives the refresh.
    await Promise.all(
      mockRooms.filter((r) => state.expandedRoomIds.has(r.join_string) && r.state === "joined").map((r) => loadRoomMembers(r))
    );
    renderRooms();
  }

  async function refreshStreams() {
    mockStreams = await fetchStreams();
    renderStreams();
  }

  async function refreshAll() {
    await refreshRooms();
    await refreshStreams();
  }

  // ---------------------------------------------------------------------
  // Event listeners — set up once, only when a real Tauri backend is
  // present (see FRONTEND-SHELL.md's mock/real fallback pattern: these
  // simply don't exist in a plain-browser Playwright run). Every one of
  // `driver.rs`'s 6 events is handled here; the ones that affect other tabs'
  // own rendered state are forwarded through the small `window.StreamControl`
  // surface those scripts expose, rather than this file reaching into their
  // internals directly.
  // ---------------------------------------------------------------------

  function wireEvents() {
    if (!window.__TAURI__) return;
    const { listen } = window.__TAURI__.event;

    listen("room-connected", () => {
      refreshAll();
      // Without this, a room that finishes joining/reconnecting while the
      // user is already on the Stream Control tab never appears in its
      // "START A STREAM" room dropdown (stream-control.js's own
      // `mockJoinedRooms` cache stays stale) until some unrelated event
      // (`presence-updated`/`destination-state-changed`) happens to fire —
      // which for a lone joiner in an otherwise-empty room may never
      // happen. Root cause of "I can't select a room to stream in".
      if (window.StreamControl) window.StreamControl.refresh();
      if (window.ViewTab) window.ViewTab.refresh();
    });
    listen("room-disconnected", () => {
      refreshAll();
      // See `room-connected` above — a disconnected room must drop out of
      // the same dropdown/cards just as promptly.
      if (window.StreamControl) window.StreamControl.refresh();
      if (window.ViewTab) window.ViewTab.refresh();
    });
    listen("room-connect-failed", (event) => {
      console.error("room-connect-failed", event.payload);
      refreshRooms();
    });
    listen("presence-updated", () => {
      // Payload is a `RoomDetail` for the room whose roster/streams changed —
      // cheapest correct response is a full ROOMS refetch (picks up the new
      // `active_stream_count` too) rather than trying to patch just one row.
      refreshRooms();
      if (window.StreamControl) window.StreamControl.refresh();
      if (window.ViewTab) window.ViewTab.refresh();
    });
    listen("destination-state-changed", () => {
      refreshStreams();
      if (window.StreamControl) window.StreamControl.refresh();
    });
    listen("bandwidth-overshoot", (event) => {
      if (window.StreamControl) window.StreamControl.setOvershoot(event.payload.overshootKbps);
    });
    listen("stream-stats-updated", (event) => {
      // Fires unconditionally ~4x/sec (see `dto::StreamStatsUpdated`'s doc
      // comment), not diffed — this must stay cheap. `destinations` is
      // `stream-control.js`'s concern, `watched` (per-tile received fps) is
      // the View tab's; each owner patches just its own DOM in place via
      // direct `textContent` updates on already-rendered elements rather
      // than going through a refresh/re-render, so this tick never causes a
      // flicker or a full rebuild.
      if (window.StreamControl && window.StreamControl.updateStats) window.StreamControl.updateStats(event.payload);
      if (window.ViewTab && window.ViewTab.updateStats) window.ViewTab.updateStats(event.payload);
    });
  }

  // ---------------------------------------------------------------------
  // Init
  // ---------------------------------------------------------------------

  async function init() {
    renderNav();
    await refreshRooms();
    renderAddRoomCard();
    await refreshStreams();
    wireStreamsDemoToggle();
    wireEvents();

    document.getElementById("add-room-btn").addEventListener("click", () => {
      state.addRoomOpen = true;
      renderAddRoomCard();
    });

    document.querySelectorAll(".tab-panel").forEach((panel) => {
      panel.hidden = panel.dataset.tab !== state.activeTab;
    });
  }

  window.Shell = { refreshRooms, refreshStreams, refreshAll };

  document.addEventListener("DOMContentLoaded", init);
})();
