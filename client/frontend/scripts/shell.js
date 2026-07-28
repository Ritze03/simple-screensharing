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

  const {
    renderNavButton,
    renderRoomRow,
    renderStreamRow,
    renderEmptyState,
    secondaryButton,
    primaryButton,
    dangerButton,
    iconButton,
    el,
    hatchedBanner,
  } = window.Components;

  const NAV_TABS = ["Stream Control", "View", "Settings"];

  // ---------------------------------------------------------------------
  // Mock data — shapes match the real DTOs (`dto::SavedRoomRow` /
  // `dto::DestinationRow`) field-for-field, per FRONTEND-SHELL.md's mock/real
  // parity rule, plus a frontend-only `members` array on room objects (no
  // single DTO inlines room + roster + live watch state — that's a client-side
  // join of `room_detail`'s `members`/`streams`/`watched`, done in
  // `loadRoomMembers` below for the real branch).
  // ---------------------------------------------------------------------

  // `?emptyRooms` dev-only query flag reaches the ROOMS section's
  // first-launch/no-saved-rooms empty state (see `renderRoomsEmptyState`)
  // without hand-editing this array — same spirit as `streams-demo-toggle`/
  // `view-demo-toggle`'s runtime toggles elsewhere in this file, just a URL
  // flag instead of a button since this one needs to be true from the very
  // first render (before any toggle could fire).
  let mockRooms = new URLSearchParams(location.search).has("emptyRooms") ? [] : [
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

  let mockStreams = [
    { destination_id: 1, capture_id: 1, room_handle: 1, kind: "video", room_label: "Movie Night", state: "live", width: 1920, height: 1080, framerate: 30, bitrate_kbps: 6000 },
  ];

  // ---------------------------------------------------------------------
  // UI state (frontend-only; not persisted, matches the "no cross-session
  // persistence of active state" rule from the multi-room brainstorm doc).
  // ---------------------------------------------------------------------

  const state = {
    activeTab: "Stream Control",
    // `join_string` of the room whose detail slide-over is currently open
    // (or null). Replaces the old `expandedRoomIds` set — see
    // `renderRoomRow`'s `Why:` note in components.js: a joined room row no
    // longer expands inline, it opens this panel instead. Starts closed
    // (the old inline-expand's "Movie Night starts expanded, matching the
    // reference shot" default doesn't carry over — there's no equivalent
    // "start open" cue in the 1f handoff for a panel the user hasn't
    // clicked into yet).
    activeRoomPanel: null,
    addRoomOpen: false,
    pendingLeaveRoomId: null,
  };

  // Room-panel-local audio-source picker state (design handoff's AUDIO
  // section). Kept separate from `stream-control.js`'s own room-scoped
  // audio row (`state.audioSource`/`mockAudioSources` there) rather than
  // shared: that row is reached via a room *dropdown* (any joined room, one
  // at a time, chosen independently of the sidebar) and is a whole-tab
  // affordance, not scoped to whichever room this panel happens to have
  // open — see the shell.js audio-integration note below `openRoomPanel`.
  let panelAudioSources = [];
  let panelAudioSourceId = null;

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
    // Toggle the active class on the existing buttons rather than calling
    // `renderNav()` (which does `nav.innerHTML = ""` and rebuilds fresh
    // elements): the CSS active-pill is a `transition:` on `.nav-btn`'s
    // background-image/color, and a transition never plays on a brand-new
    // element that's inserted already in its final state — only on a class
    // change to an element already in the DOM. Rebuilding on every switch
    // silently made the "sliding pill" transition a no-op.
    document.querySelectorAll("#nav .nav-btn").forEach((btn) => {
      btn.classList.toggle("nav-btn-active", btn.dataset.tab === tab);
    });
    document.querySelectorAll(".tab-panel").forEach((panel) => {
      panel.hidden = panel.dataset.tab !== tab;
    });
  }

  // ---------------------------------------------------------------------
  // ROOMS section
  // ---------------------------------------------------------------------

  function renderRooms() {
    const list = document.getElementById("rooms-list");
    const count = document.getElementById("rooms-count");
    if (count) count.textContent = mockRooms.length > 0 ? String(mockRooms.length) : "";
    list.innerHTML = "";

    if (mockRooms.length === 0) {
      list.appendChild(renderRoomsEmptyState());
      return;
    }

    mockRooms.forEach((room) => {
      list.appendChild(
        renderRoomRow(room, {
          onOpenRoom: (joinString) => openRoomPanel(joinString),
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
        })
      );
    });
  }

  /** Shared watch-toggle used by the room-detail panel's MEMBERS section —
   * same real `toggle_watch` command the old inline row's watch icon used
   * to call directly. */
  async function toggleWatch(joinString, member) {
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
    renderRoomPanel();
  }

  /** Standardized empty state for the ROOMS section (first-launch/no-rooms
   * case) — per the redesign, this one specifically shows the join-string
   * input INLINE rather than gating it behind the "+ Add Room" button
   * below (which stays for every non-empty case). Shares `submitJoin` with
   * `renderAddRoomCard`'s own Join button so the two entry points can't
   * drift into two slightly different validation/error paths. */
  function renderRoomsEmptyState() {
    const linkInput = el("input", { className: "field-input", attrs: { type: "text", placeholder: "server | room-id | key" } });
    const nameInput = el("input", { className: "field-input", attrs: { type: "text", placeholder: "Display name (optional)" } });
    const errorText = el("div", { className: "add-room-error", text: "" });
    errorText.hidden = true;
    const inline = el("div", { className: "empty-state-inline" }, [linkInput, nameInput, errorText]);

    return renderEmptyState("Paste a join link from whoever invited you to get started.", "Join", () => {
      const joinString = linkInput.value.trim();
      if (!joinString) return;
      submitJoin(joinString, nameInput.value.trim(), errorText);
    }, { icon: "meeting_room", headline: "No rooms yet", inline, className: "empty-state-rooms" });
  }

  /** Shared join flow for both the ROOMS empty state's inline form and the
   * "+ Add Room" card's Join button below — same real/mock branches either
   * used to duplicate inline. */
  async function submitJoin(joinString, displayName, errorEl) {
    if (window.__TAURI__) {
      if (errorEl) errorEl.hidden = true;
      try {
        await window.__TAURI__.core.invoke("join_room", { joinString, displayName });
        // Same "success saves" note as `renderAddRoomCard`'s own Join
        // handler: `join_room` upserts + emits `room-connected`; this is a
        // belt-and-braces immediate paint in case that event hasn't landed
        // yet by the time this await resolves.
        state.addRoomOpen = false;
        await refreshRooms();
        renderAddRoomCard();
      } catch (e) {
        if (errorEl) {
          errorEl.textContent = String(e);
          errorEl.hidden = false;
        }
      }
      return;
    }

    mockRooms.push({
      join_string: joinString,
      name: displayName || "Room " + Math.random().toString(16).slice(2, 6),
      state: "joined",
      room_handle: Math.floor(Math.random() * 100000),
      active_stream_count: 0,
      members: [],
    });
    state.addRoomOpen = false;
    renderRooms();
    renderAddRoomCard();
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

    const hasStake = myStreamCountForRoom(room) > 0 || (room.members || []).some((m) => m.watching);
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
    if (window.__TAURI__ && room && room.room_handle == null) {
      // Disconnected (saved but not joined) row — the ✕ here is "forget",
      // not "leave": there's no live room_handle to leave, so persist the
      // removal via `forget_room` instead.
      await window.__TAURI__.core.invoke("forget_room", { joinString: room.join_string });
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

  /**
   * Leave-room confirm dialog. Per the design handoff: 440px, a red
   * `logout` icon + "Leave {RoomName}?" title, a "you can rejoin any time"
   * reassurance line, up to 2 stacked amber hatched-warning banners (only
   * the ones that actually apply — a room can have neither, either, or
   * both of "you're watching a stream here" / "you're sending streams
   * here"), and a footer with an "ESC to cancel" hint, Cancel on the left,
   * "Leave room" (danger) on the right. Danger-on-the-right is deliberate
   * here, not the more common danger-on-left convention — a documented
   * choice from the original handoff, not a bug to "fix".
   */
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
    // Bug: this used to read `room.active_stream_count`, which is the ROOM's
    // count of *other* members' streams (see `myStreamCountForRoom`'s doc
    // comment) — a pure watcher sending nothing themselves could still see
    // "you're sending N streams" here whenever anyone else in the room was
    // sharing. Use the current user's own outgoing-destination count instead.
    const sendingCount = myStreamCountForRoom(room);

    const modal = el("div", { className: "modal leave-modal" });

    const head = el("div", { className: "leave-modal-head" });
    head.appendChild(el("span", { className: "material-symbols-outlined leave-modal-icon", text: "logout" }));
    const headText = el("div", { className: "leave-modal-head-text" });
    headText.appendChild(el("div", { className: "leave-modal-title", text: `Leave ${room.name}?` }));
    headText.appendChild(el("div", { className: "leave-modal-subtext", text: "You can rejoin any time with the same join link." }));
    head.appendChild(headText);
    modal.appendChild(head);

    const banners = el("div", { className: "leave-modal-banners" });
    if (watchingCount > 0) {
      banners.appendChild(hatchedBanner(`You're watching ${watchingCount} stream${watchingCount === 1 ? "" : "s"} here — leaving closes ${watchingCount === 1 ? "it" : "them"}.`));
    }
    if (sendingCount > 0) {
      banners.appendChild(hatchedBanner(`You're sending ${sendingCount} stream${sendingCount === 1 ? "" : "s"} here — leaving stops ${sendingCount === 1 ? "it" : "them"}.`));
    }
    if (banners.children.length > 0) modal.appendChild(banners);

    const footer = el("div", { className: "leave-modal-footer" });
    footer.appendChild(el("span", { className: "leave-modal-esc-hint", text: "ESC to cancel" }));
    const actions = el("div", { className: "leave-modal-actions" });
    actions.appendChild(
      secondaryButton("Cancel", () => {
        state.pendingLeaveRoomId = null;
        renderLeaveModal();
      })
    );
    actions.appendChild(
      el("button", {
        className: "btn btn-danger",
        text: "Leave room",
        onClick: async () => {
          const joinString = state.pendingLeaveRoomId;
          state.pendingLeaveRoomId = null;
          renderLeaveModal();
          await forceLeaveRoom(joinString);
        },
      })
    );
    footer.appendChild(actions);
    modal.appendChild(footer);

    overlay.appendChild(modal);
    overlay.hidden = false;
  }

  // ---------------------------------------------------------------------
  // Room-detail slide-over (design handoff option 1f) — replaces the
  // sidebar's old inline expand-to-show-members interaction (see
  // `renderRoomRow`'s `Why:` note in components.js for the row-click UX
  // decision). One panel at a time, keyed by `state.activeRoomPanel`
  // (a `join_string`, or null when closed).
  //
  // AUDIO section integration note: `stream-control.js` already has a
  // room-scoped "Share Audio" row (`renderShareAudioCard`), but it's reached
  // through its OWN room dropdown (`state.scopedRoomHandle`) — a whole-tab
  // affordance for "pick any joined room, then act on it", independent of
  // whichever sidebar row was clicked. It is not itself scoped to "the room
  // this panel is currently showing". Rather than wire this panel to that
  // dropdown's selection (which would silently desync the two — opening
  // Room A's panel would have no effect on the Stream Control tab's
  // separately-chosen scoped room), this panel gets its own minimal,
  // genuinely room-scoped entry point onto the same three real commands
  // (`list_audio_sources`/`select_audio_source`/`start_audio_share`) fixed
  // to `state.activeRoomPanel`'s `room_handle` — see `fetchPanelAudioSources`/
  // `startPanelAudioShare` below. Small, deliberate duplication of a ~15-line
  // fetch/select/start sequence, not a second data model.
  // ---------------------------------------------------------------------

  /** Opens the panel for a joined room. Disconnected/reconnecting rows never
   * reach this (see `renderRoomRow`'s `onOpenRoom` wiring — only attached
   * for `state === "joined"`), but this guard stays as the defensive
   * single source of truth: there's no live `room_handle`/roster to show
   * for a room that isn't actually joined. */
  async function openRoomPanel(joinString) {
    const room = mockRooms.find((r) => r.join_string === joinString);
    if (!room || room.state !== "joined" || room.room_handle == null) return;

    state.activeRoomPanel = joinString;
    panelAudioSources = [];
    panelAudioSourceId = null;

    const scrim = document.getElementById("room-panel-scrim");
    scrim.hidden = false;
    renderRoomPanel();
    // Two nested rAFs — same "already-in-the-DOM before the class flips"
    // trick `setActiveTab` uses for the nav pill: a transition never plays
    // on a class added in the same frame an element's `hidden` was cleared,
    // only on a class change to an element the browser has already painted
    // once (hence one rAF to get past this frame, a second to be sure the
    // first has been laid out).
    requestAnimationFrame(() => requestAnimationFrame(() => scrim.classList.add("room-panel-scrim-open")));

    await loadRoomMembers(room);
    if (state.activeRoomPanel === joinString) renderRoomPanel();

    panelAudioSources = await fetchPanelAudioSources();
    if (panelAudioSources.length > 0) panelAudioSourceId = panelAudioSources[0].id;
    if (state.activeRoomPanel === joinString) renderRoomPanel();
  }

  /** Closes the panel (scrim click, close icon, ESC, or the room it was
   * showing disappearing out from under it). Animates the transform/opacity
   * back out, then hides the scrim once the transition ends (with a timeout
   * fallback in case `transitionend` never fires — e.g. the panel was
   * already at 0 width for some reason — so the scrim can never get stuck
   * invisible-but-still-blocking-clicks). */
  function closeRoomPanel() {
    state.activeRoomPanel = null;
    const scrim = document.getElementById("room-panel-scrim");
    const panel = document.getElementById("room-panel");
    scrim.classList.remove("room-panel-scrim-open");
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      scrim.hidden = true;
      panel.removeEventListener("transitionend", finish);
    };
    panel.addEventListener("transitionend", finish);
    setTimeout(finish, 300);
  }

  /** One non-self member row: name + either a watch toggle (they're
   * sharing) or a "not sharing" status. Factored out of `renderRoomPanel` so
   * both the mock ("other members") and real (`room_detail`-as-is) branches
   * render rows identically. */
  function renderRoomPanelMemberRow(room, m) {
    const row = el("div", { className: "room-panel-member-row" });
    row.appendChild(el("span", { className: "room-panel-member-name", text: m.display_name }));
    if (m.resolution) {
      // Sharing: a stop-watching/watch icon toggles our own subscription
      // to their stream, reusing the real `toggle_watch` command.
      row.appendChild(
        iconButton(m.watching ? "visibility_off" : "visibility", () => toggleWatch(room.join_string, m), {
          title: m.watching ? "Stop watching" : "Watch",
          className: m.watching ? "watch-active" : "",
        })
      );
    } else {
      row.appendChild(el("span", { className: "room-panel-member-status", text: "not sharing" }));
    }
    return row;
  }

  /** Renders the panel's content for `state.activeRoomPanel`. A no-op when
   * the panel is closed — `closeRoomPanel` owns hiding the scrim itself, so
   * this only needs to (re)build content while it's open, called after
   * every data change that could affect it (member list, active-stream
   * count, rename, refresh events). */
  function renderRoomPanel() {
    const panel = document.getElementById("room-panel");
    if (!state.activeRoomPanel) return;
    const room = mockRooms.find((r) => r.join_string === state.activeRoomPanel);
    if (!room) return;
    panel.innerHTML = "";

    // ---- Header: status dot + name + close icon, meta line below. ----
    const header = el("div", { className: "room-panel-header" });
    const top = el("div", { className: "room-panel-header-top" });
    top.appendChild(el("span", { className: "status-dot status-dot-joined" }));
    top.appendChild(el("span", { className: "room-panel-title", text: room.name }));
    top.appendChild(
      iconButton("close", () => closeRoomPanel(), { title: "Close", className: "room-panel-close" })
    );
    header.appendChild(top);

    // `Why:` no "joined {duration} ago" clause — neither `SavedRoomRow` nor
    // `RoomDetail` (`src-tauri/src/dto.rs`) nor `RoomSession`
    // (`src/session.rs`) tracks a join timestamp anywhere in the data
    // model, and the design handoff explicitly says to omit rather than
    // fabricate one. Adding that plumbing is backend scope this pass
    // deliberately stays out of (see the task's SCOPE note) — a real gap,
    // not an oversight.
    // `Why:` `room.members` means two different things depending on branch
    // — both intentional, not something to unify away here:
    //  - Real (`window.__TAURI__`): `loadRoomMembers` -> `room_detail` ->
    //    the relay's `PresenceSnapshot`, which legitimately *includes the
    //    caller's own connection* (correct, by-design server behavior — see
    //    `simple-screensharing-server`). This code used to assume `members`
    //    was "other members only" and add a synthetic self row + `+1` on
    //    top regardless, which double-counted/duplicated whenever there was
    //    exactly one real connection (yourself, alone): "2 members" with
    //    two rows for the same person. `stream-control.js`'s MEMBERS card
    //    (`renderMembersCard`) never had this bug because it never adds a
    //    synthetic row on top of `room_detail`'s list — same fix applied
    //    below.
    //  - Mock (no Tauri): the hardcoded arrays at the top of this file model
    //    "other members" only (e.g. `Gaming Night: members: []` means "just
    //    me, alone" — see this file's own header comment on mock/real
    //    parity) and were never updated to include a self entry, so the old
    //    synthetic-"You"-row/`+1` behavior is still correct there.
    const includesSelf = !!window.__TAURI__;
    const members = room.members || [];
    const memberCount = includesSelf ? members.length : members.length + 1;
    const sendingCount = myStreamCountForRoom(room);
    const metaText = `${memberCount} member${memberCount === 1 ? "" : "s"} · you're sending ${sendingCount} stream${sendingCount === 1 ? "" : "s"} here`;
    header.appendChild(el("div", { className: "room-panel-meta", text: metaText }));
    panel.appendChild(header);

    const body = el("div", { className: "room-panel-body" });

    // ---- MEMBERS ----
    const membersSection = el("div", { className: "room-panel-members-section" });
    membersSection.appendChild(el("div", { className: "room-panel-section-label", text: "Members" }));
    const memberList = el("div", { className: "room-panel-members" });

    // No DTO exposes the local client's own `conn_id`/display name to the
    // frontend (`dto::SettingsDto`/`RoomDetail`/`MemberDto` have no such
    // field — see `components.js`'s existing note on this same gap), so
    // there is no general way to pick "which member is me" out of a roster
    // of 2+ real entries. `stream-control.js`'s `renderMembersCard` avoids
    // this by never trying: it just renders `room_detail`'s member list
    // as-is, with no synthetic self row and no self label. Applied here the
    // same way for the real (`includesSelf`) branch — the one exception is
    // the single-member case, which is unambiguous without any new
    // mechanism: a room you're viewing `room_detail` for is always one
    // you've joined, so if the roster has exactly one entry, that member can
    // only be you.
    if (!includesSelf) {
      const youRow = el("div", { className: "room-panel-member-row room-panel-member-row-you" });
      youRow.appendChild(el("span", { className: "room-panel-member-name", text: "You" }));
      youRow.appendChild(el("span", { className: "room-panel-you-badge", text: "YOU" }));
      memberList.appendChild(youRow);
      members.forEach((m) => memberList.appendChild(renderRoomPanelMemberRow(room, m)));
    } else if (members.length === 1) {
      const youRow = el("div", { className: "room-panel-member-row room-panel-member-row-you" });
      youRow.appendChild(el("span", { className: "room-panel-member-name", text: "You" }));
      youRow.appendChild(el("span", { className: "room-panel-you-badge", text: "YOU" }));
      memberList.appendChild(youRow);
    } else {
      members.forEach((m) => memberList.appendChild(renderRoomPanelMemberRow(room, m)));
    }
    membersSection.appendChild(memberList);
    body.appendChild(membersSection);

    // ---- AUDIO ----
    const audioSection = el("div", { className: "room-panel-audio-section" });
    audioSection.appendChild(el("div", { className: "room-panel-section-label", text: "Audio" }));
    const audioRow = el("div", { className: "room-panel-audio-row" });
    if (panelAudioSources.length === 0) {
      audioRow.appendChild(el("span", { className: "room-panel-member-status", text: "No audio sources found." }));
    } else {
      const select = el("select");
      panelAudioSources.forEach((s) => select.appendChild(el("option", { text: s.name, attrs: { value: s.id } })));
      select.value = panelAudioSourceId;
      select.addEventListener("change", () => {
        panelAudioSourceId = Number(select.value);
      });
      audioRow.appendChild(select);
    }
    audioRow.appendChild(
      primaryButton("Share Audio", () => {
        const source = panelAudioSources.find((s) => s.id === panelAudioSourceId);
        if (source) startPanelAudioShare(room.room_handle, source);
      }, { disabled: panelAudioSources.length === 0, className: "room-panel-share-audio-btn" })
    );
    audioSection.appendChild(audioRow);
    body.appendChild(audioSection);

    // ---- ROOM NAME ----
    const nameSection = el("div", { className: "room-panel-name-section" });
    nameSection.appendChild(el("div", { className: "room-panel-section-label", text: "Room name" }));
    const renameRow = el("div", { className: "room-panel-rename-row" });
    const nameInput = el("input", { className: "field-input", attrs: { type: "text", value: room.name } });
    renameRow.appendChild(nameInput);
    renameRow.appendChild(
      secondaryButton("Save", () => renamePanelRoom(room.join_string, nameInput.value.trim() || room.name))
    );
    nameSection.appendChild(renameRow);
    // Verified against `rename_saved_room` (src/session.rs): it only ever
    // touches the local `RoomStore` file (`room_store.upsert` + `.save()`)
    // — never sends the new name over the wire — so this claim holds.
    nameSection.appendChild(
      el("div", { className: "room-panel-privacy-note", text: "Only you see this name. It never leaves your machine." })
    );
    body.appendChild(nameSection);

    panel.appendChild(body);

    // ---- Footer: divider (via border-top in CSS) + outline "Leave" ----
    const footer = el("div", { className: "room-panel-footer" });
    footer.appendChild(
      dangerButton(
        "Leave this room",
        () => {
          const joinString = room.join_string;
          closeRoomPanel();
          openLeaveConfirm(joinString);
        },
        { className: "room-panel-leave-btn" }
      )
    );
    panel.appendChild(footer);
  }

  async function renamePanelRoom(joinString, newName) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("rename_room", { joinString, newName });
      await refreshRooms();
      return;
    }
    const room = mockRooms.find((r) => r.join_string === joinString);
    if (room) room.name = newName;
    renderRooms();
    renderRoomPanel();
  }

  async function fetchPanelAudioSources() {
    if (window.__TAURI__) {
      try {
        return await Promise.race([
          window.__TAURI__.core.invoke("list_audio_sources"),
          new Promise((_, reject) => setTimeout(() => reject(new Error("timed out")), 3000)),
        ]);
      } catch (e) {
        console.error("list_audio_sources failed or timed out", e);
        return [];
      }
    }
    return [
      { id: 1, name: "System Output" },
      { id: 2, name: "Game (app audio)" },
    ];
  }

  async function startPanelAudioShare(roomHandle, source) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("select_audio_source", { id: source.id, name: source.name });
      await window.__TAURI__.core.invoke("start_audio_share", { roomHandle });
      await refreshStreams();
      if (window.StreamControl) window.StreamControl.refresh();
      return;
    }
    mockStreams.push({
      destination_id: Math.floor(Math.random() * 100000),
      capture_id: Math.floor(Math.random() * 100000),
      room_handle: roomHandle,
      room_label: roomLabelFor(roomHandle),
      kind: "audio",
      state: "live",
      width: 0,
      height: 0,
      framerate: 0,
      bitrate_kbps: 128,
    });
    renderStreams();
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
      primaryButton("Join", () => {
        const joinString = linkInput.value.trim();
        if (!joinString) return;
        submitJoin(joinString, nameInput.value.trim(), errorText);
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
  // STREAMS / "OUTGOING" section — sidebar-redesign (option 1c) restyled
  // this in place rather than building a new section: it already sourced
  // its data from the real `list_active_streams` command (the same DTO
  // `stream-control.js`'s Active Shares grid reads via its own
  // `fetchActiveDestinations()`), already returned one row per active
  // outgoing DESTINATION (not per capture — a capture fanned out to N rooms
  // already produced N rows here), and was already cross-room per
  // APP-SHELL.md's STREAMS section doc. Only the row markup/labels changed.
  // ---------------------------------------------------------------------

  function renderStreams() {
    const list = document.getElementById("streams-list");
    list.innerHTML = "";
    if (mockStreams.length === 0) {
      list.appendChild(
        renderEmptyState("Start a stream from the Stream Control tab to see it here.", "+ Start a Stream", () => {
          setActiveTab("Stream Control");
        }, { icon: "cast", headline: "You're not sharing anything" })
      );
      return;
    }
    mockStreams.forEach((s) => list.appendChild(renderStreamRow(s, { onStop: (destinationId) => stopDestination(destinationId) })));
  }

  /** Sidebar's lighter-weight entry point onto the same real `stop_destination`
   * command `stream-control.js`'s own Stop button calls (see that file's
   * `stopDestination`) — not a second copy of any stop logic, just another
   * caller of the one command. */
  async function stopDestination(destinationId) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("stop_destination", { destinationId });
      await refreshStreams();
      if (window.StreamControl) window.StreamControl.refresh();
      return;
    }
    mockStreams = mockStreams.filter((s) => s.destination_id !== destinationId);
    renderStreams();
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
      mockStreams =
        mockStreams.length === 0
          ? [{ destination_id: 1, capture_id: 1, room_handle: 1, kind: "video", room_label: "Movie Night", state: "live", width: 1920, height: 1080, framerate: 30, bitrate_kbps: 6000 }]
          : [];
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
    // roster (see `loadRoomMembers`'s own doc comment) — the open panel's
    // `.members` from the previous render is gone from these fresh objects.
    // Without this, any event that calls `refreshRooms()` (this fires on
    // `presence-updated`, i.e. routinely) would blank the open panel's
    // member list back to empty even though the roster hasn't actually
    // changed. Re-fetch it for exactly the room the panel currently has
    // open (if any) so the visible roster survives the refresh.
    if (state.activeRoomPanel) {
      const openRoom = mockRooms.find((r) => r.join_string === state.activeRoomPanel);
      if (openRoom && openRoom.state === "joined" && openRoom.room_handle != null) {
        await loadRoomMembers(openRoom);
      } else {
        // The room the panel was showing disconnected/vanished out from
        // under it (left elsewhere, connection dropped) — there's no live
        // detail left to show, so close it rather than render a stale or
        // broken panel.
        closeRoomPanel();
      }
    }
    renderRooms();
    renderRoomPanel();
  }

  async function refreshStreams() {
    mockStreams = await fetchStreams();
    renderStreams();
  }

  /** How many streams *the current user* is personally sending into `room`
   * right now — i.e. this client's own outgoing destinations targeting
   * `room.room_handle`, not `room.active_stream_count` (which is the
   * opposite: `RoomSession::active_stream_count` in `src/session.rs`
   * explicitly *excludes* the caller's own streams — see its doc comment,
   * "how many *other* people are streaming"). `mockStreams` (kept fresh by
   * `refreshStreams`/`fetchStreams`, real branch = `list_active_streams`) is
   * always this client's own destinations — a destination is only ever
   * created locally — so filtering it by `room_handle` is the correct,
   * already-existing source for "am I sending here", reused rather than
   * inventing a new one. */
  function myStreamCountForRoom(room) {
    if (!room || room.room_handle == null) return 0;
    return mockStreams.filter((s) => s.room_handle === room.room_handle).length;
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

    // Room-detail panel: clicking the scrim closes it. `#room-panel` sits
    // inside `#room-panel-scrim` in the DOM, so a click anywhere inside the
    // panel bubbles up to this same listener — the `e.target.id` check is
    // what limits "closes" to a click that actually landed on the scrim's
    // own background, not on the panel or anything in it (standard
    // "click outside" pattern).
    document.getElementById("room-panel-scrim").addEventListener("click", (e) => {
      if (e.target.id === "room-panel-scrim") closeRoomPanel();
    });

    // "ESC to cancel" — the leave-confirm modal's footer hint (see
    // `renderLeaveModal`) — and the same key closes the room-detail panel
    // when it's the frontmost thing open (the leave modal, if also open,
    // takes priority since it renders on top of the panel). Wired
    // unconditionally (not gated behind `window.__TAURI__`) so it works the
    // same in a mock/browser-preview session as in the real app.
    document.addEventListener("keydown", (e) => {
      if (e.key !== "Escape") return;
      if (state.pendingLeaveRoomId) {
        state.pendingLeaveRoomId = null;
        renderLeaveModal();
      } else if (state.activeRoomPanel) {
        closeRoomPanel();
      }
    });

    document.querySelectorAll(".tab-panel").forEach((panel) => {
      panel.hidden = panel.dataset.tab !== state.activeTab;
    });
  }

  window.Shell = { refreshRooms, refreshStreams, refreshAll };

  document.addEventListener("DOMContentLoaded", init);
})();
