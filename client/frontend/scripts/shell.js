// shell.js — app shell behavior: nav tab switching, ROOMS/STREAMS rendering,
// room-row expand/collapse, "+ Add Room" inline card, leave-confirm modal.
//
// Scope note: this is the FRONTEND SHELL only (see
// superdoc/ui/APP-SHELL.md, stage 1). Stream Control / View / Settings tab
// *content* is other workers' scope — the panels below are placeholders
// that just prove nav-switching works. Nothing here calls a real Tauri
// command yet; every backend touchpoint is mocked with an in-memory array
// and flagged with a `// TODO: replace with invoke(...)` comment so it's
// obvious what the next integration pass needs to wire up.
//
// Per APP-SHELL.md's hard rule ("the nav is a pure view-switcher"): this
// script only ever toggles which tab panel is visible. It never starts or
// stops a share/watch/subscription as a side effect of switching tabs —
// there is no real backend state here yet to accidentally touch, but the
// structure (a single `setActiveTab` that only flips `hidden`/classes) is
// deliberately shaped so a later worker wiring real data can't easily
// violate that rule by accident.

(function () {
  "use strict";

  const { renderNavButton, renderRoomRow, renderStreamRow, renderEmptyState, secondaryButton, primaryButton, el } = window.Components;

  // ---------------------------------------------------------------------
  // Mock data — TODO: replace with invoke('list_saved_rooms') /
  // invoke('list_active_streams') once the Tauri command surface exists.
  // Shapes here are the frontend's best guess at the eventual command
  // response shape; adjust once the real backend worker lands its schema.
  // ---------------------------------------------------------------------

  const NAV_TABS = ["Stream Control", "View", "Settings"];

  // TODO: replace with invoke('list_saved_rooms') — a left join of the
  // persistent RoomStore with any live RoomSession, per APP-SHELL.md's
  // "ROOMS section" note.
  let mockRooms = [
    {
      id: "r1",
      name: "Movie Night",
      state: "joined",
      activeCount: 1,
      members: [{ name: "Ana", resolution: "1920x1080", fps: null, watching: false }],
    },
    {
      id: "r2",
      name: "Gaming Night",
      state: "joined",
      activeCount: 0,
      members: [],
    },
    {
      id: "r3",
      name: "Old Room",
      state: "disconnected",
      activeCount: 0,
      members: [],
    },
    {
      id: "r4",
      name: "Study Group",
      state: "reconnecting",
      activeCount: 0,
      members: [],
    },
  ];

  // TODO: replace with invoke('list_active_streams') — this client's own
  // outgoing destinations across every room (APP-SHELL.md's "STREAMS"
  // section: cross-room, not scoped to the active tab/room).
  let mockStreams = [{ roomLabel: "Movie Night", resolution: "1920x1080", fps: 30, isAudio: false }];

  // ---------------------------------------------------------------------
  // UI state (frontend-only; not persisted, matches the "no cross-session
  // persistence of active state" rule from the multi-room brainstorm doc).
  // ---------------------------------------------------------------------

  const state = {
    activeTab: "Stream Control",
    expandedRoomIds: new Set(["r1"]), // Movie Night starts expanded, matching the reference shot.
    addRoomOpen: false,
    pendingLeaveRoomId: null,
  };

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
          expanded: state.expandedRoomIds.has(room.id),
          onToggleExpand: (id) => {
            if (state.expandedRoomIds.has(id)) state.expandedRoomIds.delete(id);
            else state.expandedRoomIds.add(id);
            renderRooms();
          },
          onReconnect: (id) => {
            // TODO: replace with invoke('reconnect_room', { id }).
            const r = mockRooms.find((x) => x.id === id);
            if (r) r.state = "reconnecting";
            renderRooms();
          },
          onLeave: (id) => openLeaveConfirm(id),
          onWatch: (roomId, memberName) => {
            // TODO: replace with invoke('toggle_watch', { roomId, memberName }).
            const r = mockRooms.find((x) => x.id === roomId);
            const m = r && r.members.find((mm) => mm.name === memberName);
            if (m) m.watching = !m.watching;
            renderRooms();
          },
        })
      );
    });
  }

  function openLeaveConfirm(roomId) {
    const room = mockRooms.find((r) => r.id === roomId);
    if (!room) return;

    // Per APP-SHELL.md: leaving raises a confirm dialog iff the room has an
    // outgoing destination or a live watch; a leave with neither is
    // immediate. Mock approximation: joined rooms with activeCount > 0, or
    // any watched member, require confirmation.
    const hasStake = room.activeCount > 0 || (room.members || []).some((m) => m.watching);
    if (room.state === "joined" && hasStake) {
      state.pendingLeaveRoomId = roomId;
      renderLeaveModal();
    } else {
      leaveRoom(roomId);
    }
  }

  function leaveRoom(roomId) {
    // TODO: replace with invoke('leave_room', { roomId }) /
    // invoke('forget_room', { roomId }) depending on room.state.
    mockRooms = mockRooms.filter((r) => r.id !== roomId);
    renderRooms();
  }

  function renderLeaveModal() {
    const overlay = document.getElementById("modal-overlay");
    overlay.innerHTML = "";
    if (!state.pendingLeaveRoomId) {
      overlay.hidden = true;
      return;
    }
    const room = mockRooms.find((r) => r.id === state.pendingLeaveRoomId);
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
        onClick: () => {
          leaveRoom(state.pendingLeaveRoomId);
          state.pendingLeaveRoomId = null;
          renderLeaveModal();
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

    const actions = el("div", { className: "add-room-actions" });
    actions.appendChild(
      primaryButton("Join", () => {
        // TODO: replace with invoke('join_room', { displayName, joinLink }).
        // On success: upsert into RoomStore, appears live in the list (per
        // APP-SHELL.md — "no staging step, and why success saves").
        if (linkInput.value.trim()) {
          mockRooms.push({
            id: "r" + Date.now(),
            name: nameInput.value.trim() || "Room " + Math.random().toString(16).slice(2, 6),
            state: "joined",
            activeCount: 0,
            members: [],
          });
          state.addRoomOpen = false;
          renderRooms();
          renderAddRoomCard();
        }
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
  // real feature — remove once real invoke() data drives this section.
  function wireStreamsDemoToggle() {
    const toggle = document.getElementById("streams-demo-toggle");
    toggle.addEventListener("click", () => {
      mockStreams = mockStreams.length === 0 ? [{ roomLabel: "Movie Night", resolution: "1920x1080", fps: 30, isAudio: false }] : [];
      renderStreams();
    });
  }

  // ---------------------------------------------------------------------
  // Init
  // ---------------------------------------------------------------------

  function init() {
    renderNav();
    renderRooms();
    renderAddRoomCard();
    renderStreams();
    wireStreamsDemoToggle();

    document.getElementById("add-room-btn").addEventListener("click", () => {
      state.addRoomOpen = true;
      renderAddRoomCard();
    });

    document.querySelectorAll(".tab-panel").forEach((panel) => {
      panel.hidden = panel.dataset.tab !== state.activeTab;
    });
  }

  document.addEventListener("DOMContentLoaded", init);
})();
