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
    icon,
    el,
    statusDot,
    hatchedBanner,
    dropdown,
    pillSwitch,
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
      auto_connect: false,
      members: [{ conn_id: 1, display_name: "Ana", owner: 1, stream_id: 0, resolution: "1920x1080", watching: false }],
      // Mock stand-in for `room_detail`'s `streams` (`RoomDetail.streams` /
      // `StreamInfoDto`, `src-tauri/src/dto.rs`) — the room panel's "Live
      // streams" section reads this directly on the mock branch since
      // `loadRoomMembers` (which populates it on the real branch) short-circuits
      // without `window.__TAURI__`. Kept in sync with `members[0]` above (same
      // owner/resolution) so both sections describe the same stream.
      streams: [{ owner: 1, stream_id: 0, kind: "Video", label: "Screen", width: 1920, height: 1080 }],
    },
    {
      join_string: "server|22222222|gaming-night-key",
      name: "Gaming Night",
      state: "joined",
      room_handle: 2,
      active_stream_count: 2,
      auto_connect: true,
      // Mixed LIVE/not-LIVE roster — exercises the sidebar member/LIVE tree
      // (`renderRoomRow`, components.js) with more than one member and more
      // than one simultaneous stream, unlike Movie Night's single-member
      // case above.
      members: [
        { conn_id: 10, display_name: "Ben", owner: 10, stream_id: 0, resolution: "1920x1080", watching: false },
        { conn_id: 11, display_name: "Cleo", owner: 11, stream_id: 0, resolution: "1280x720", watching: false },
        { conn_id: 12, display_name: "Sam", owner: 12, stream_id: 0, resolution: null, watching: false },
      ],
      streams: [
        { owner: 10, stream_id: 0, kind: "Video", label: "Game", width: 1920, height: 1080 },
        { owner: 11, stream_id: 0, kind: "Video", label: "Webcam", width: 1280, height: 720 },
      ],
    },
    {
      join_string: "server|33333333|old-room-key",
      name: "Old Room",
      state: "disconnected",
      room_handle: null,
      active_stream_count: 0,
      auto_connect: false,
      members: [],
    },
    {
      join_string: "server|44444444|study-group-key",
      name: "Study Group",
      state: "reconnecting",
      room_handle: null,
      active_stream_count: 0,
      auto_connect: false,
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
    // Which tab the "+ Create/Join a Room" card shows — "join" is the
    // default/pre-selected tab (reset here whenever the card is opened, see
    // the `add-room-btn` click listener below), "create" mints a brand-new
    // room via the `create_room` command. Segmented control is `pillSwitch`,
    // the same component `bandwidthModeToggle` uses.
    addRoomTab: "join",
    pendingLeaveRoomId: null,
    // Own display name, for the MEMBERS panel's self row (see
    // `renderRoomPanel`) — fetched lazily from `get_settings` the first
    // time a room panel opens and cached here for the session (it only
    // changes via a fresh `join_room` call, which is rare enough not to
    // warrant re-fetching on every panel open). Null until fetched (or on
    // the mock branch, where there's no backend `AppState::display_name`
    // to read — falls back to "You" in that case, same as before).
    myDisplayName: null,
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
    // `detail.streams` as-is (every announced stream, not just one per
    // member) plus the same `watched` set, both read by the room panel's
    // "Live streams" section (`renderRoomPanel`/`renderRoomPanelStreamRow`
    // below). `room.members` above only ever surfaces one Video stream per
    // member (a pre-existing limitation, see its own inline comment); this
    // is the fuller per-stream view the STREAMS tree needs — see
    // `superdoc/features/stream-control.md`'s 2026-07-28 note for why a
    // per-stream *watcher count* can't be added alongside it yet.
    room.streams = detail.streams;
    room.watchedKeys = watchedOwners;
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
          onJoinedRowClick: (clickedRoom) => jumpToViewForRoom(clickedRoom),
        })
      );
    });
  }

  /** ROOMS row left-click on a *joined* room (`renderRoomRow`'s
   * `onJoinedRowClick`, components.js) — per the owner's explicit request:
   * jump to the View tab, and filter it to just this room's streams UNLESS
   * this is the only currently-connected room (in which case "all rooms" is
   * already exactly this room, so no filter chip is needed). Reuses
   * `ViewTab.setRoomFilter` (view.js) rather than reaching into `roomFilter`
   * directly — that variable is view.js-internal. */
  function jumpToViewForRoom(room) {
    setActiveTab("View");
    if (!window.ViewTab) return;
    const joinedCount = mockRooms.filter((r) => r.state === "joined").length;
    window.ViewTab.setRoomFilter(joinedCount > 1 ? room.name : null);
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

  /** Watch-toggle for the room-detail panel's "Live streams" section — same
   * `toggle_watch` command `toggleWatch` (MEMBERS section) and `view.js`'s
   * `playNotOnScreen`/`stopWatchingTile` already call, generalized to any
   * stream row (`{owner, stream_id, watching}`) rather than assuming one
   * Video stream per member. Unlike `toggleWatch`, this also switches the
   * user to the View tab on a *start* (not a stop) — the owner's request was
   * explicitly "so i can connect to them from there", and `setActiveTab` is
   * a pure view-switcher per this file's header note, so flipping tabs here
   * is a safe, side-effect-free UI move layered on top of the real
   * side-effecting `toggle_watch` call above it. */
  async function toggleStreamWatch(joinString, streamRow) {
    const room = mockRooms.find((x) => x.join_string === joinString);
    if (!room) return;
    const wasWatching = !!streamRow.watching;
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("toggle_watch", { roomHandle: room.room_handle, owner: streamRow.owner, streamId: streamRow.stream_id });
      await loadRoomMembers(room);
      if (window.ViewTab) window.ViewTab.refresh();
    } else {
      room.watchedKeys = room.watchedKeys || new Set();
      const key = `${streamRow.owner}:${streamRow.stream_id}`;
      if (room.watchedKeys.has(key)) room.watchedKeys.delete(key);
      else room.watchedKeys.add(key);
    }
    if (!wasWatching) setActiveTab("View");
    renderRoomPanel();
  }

  /** One row of the room-detail panel's "Live streams" section — every
   * announced stream in the room (`room.streams`, from `room_detail`'s
   * `RoomDetail.streams` / `StreamInfoDto`), not just the one-video-per-member
   * view `renderRoomPanelMemberRow` above shows. No watcher-count column:
   * the relay only reports `SubscriberCount` to a stream's own owner (spec
   * §9 — "other members never receive another stream's subscriber count"),
   * so this client has no way to know how many people are watching someone
   * else's stream without a wire-protocol change. See
   * `superdoc/features/stream-control.md`'s 2026-07-28 note. */
  function renderRoomPanelStreamRow(room, s) {
    const row = el("div", { className: "room-panel-member-row" });
    const nameText = s.label ? `${s.display_name} — ${s.label}` : s.display_name;
    row.appendChild(el("span", { className: "room-panel-member-name", text: nameText }));
    const metaParts = [s.kind === "Video" ? "Video" : "Audio"];
    if (s.resolution) metaParts.push(s.resolution);
    row.appendChild(el("span", { className: "room-panel-member-status", text: metaParts.join(" · ") }));
    row.appendChild(
      iconButton(s.watching ? "visibility_off" : "visibility", () => toggleStreamWatch(room.join_string, s), {
        title: s.watching ? "Stop watching" : "Watch",
        className: s.watching ? "watch-active" : "",
      })
    );
    return row;
  }

  /** Standardized empty state for the ROOMS section (first-launch/no-rooms
   * case) — per the redesign, this one specifically shows the join-string
   * input INLINE rather than gating it behind the "+ Add Room" button
   * below (which stays for every non-empty case). Shares `submitJoin` with
   * `renderAddRoomCard`'s own Join button so the two entry points can't
   * drift into two slightly different validation/error paths. */
  function renderRoomsEmptyState() {
    const linkInput = el("input", { className: "field-input field-input-mono", attrs: { type: "text", placeholder: "server | room-id | key" } });
    const errorText = el("div", { className: "add-room-error", text: "" });
    errorText.hidden = true;
    const inline = el("div", { className: "empty-state-inline" }, [linkInput, errorText]);

    return renderEmptyState("Paste a join link from whoever invited you to get started.", "Join", () => {
      const joinString = linkInput.value.trim();
      if (!joinString) return;
      // No room-name field here (see this function's own doc comment on
      // why it's out of scope for the "+ Add Room" card's new field) —
      // `""` preserves this entry point's existing id-derived-name-on-join
      // behavior unchanged.
      submitJoin(joinString, "", errorText);
    }, { icon: "meeting_room", headline: "No rooms yet", inline, className: "empty-state-rooms" });
  }

  /** Shared join flow for both the ROOMS empty state's inline form and the
   * "+ Add Room" card's Join button below — same real/mock branches either
   * used to duplicate inline.
   *
   * No display-name parameter: display name is now one global Settings-tab
   * value (`settings.js`, `set_display_name`), not asked per join/create —
   * see `superdoc/features/stream-control.md`'s 2026-07-28 note. `join_room`
   * still takes a `displayName` argument (unchanged Tauri command signature,
   * kept for its Rust test call sites), but this always passes `""`, the
   * same "leave it, backend already knows" call the sidebar's silent
   * auto-reconnect and `submitCreate` below already made — `join_room_impl`
   * only overwrites `AppState::display_name` when given a non-empty value,
   * so `""` here just means "use whatever's currently set", which is now
   * always the persisted global setting (loaded into `AppState::display_name`
   * at startup, kept live-current by `set_display_name`).
   *
   * `roomName` is optional (defaults to `""` at every call site below except
   * the "+ Add Room" card's Join tab, which now has its own field for it —
   * see `renderAddRoomCard`'s doc comment on that field): an empty string
   * means "no local nickname typed", same "leave it, backend already knows"
   * convention as `displayName` above — `join_room_impl` only pre-seeds
   * `room_store` when `roomName` is non-empty, otherwise the existing
   * id-derived fallback naming in `poll_connect_result` applies unchanged. */
  async function submitJoin(joinString, roomName, errorEl) {
    if (window.__TAURI__) {
      if (errorEl) errorEl.hidden = true;
      try {
        await window.__TAURI__.core.invoke("join_room", { joinString, displayName: "", roomName: roomName || "" });
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
      // Typed name wins, same as the real `join_room_impl` pre-seed; falls
      // back to the id-derived-style random name when left blank, matching
      // `poll_connect_result`'s real fallback behavior.
      name: (roomName && roomName.trim()) || "Room " + Math.random().toString(16).slice(2, 6),
      state: "joined",
      room_handle: Math.floor(Math.random() * 100000),
      active_stream_count: 0,
      members: [],
    });
    state.addRoomOpen = false;
    renderRooms();
    renderAddRoomCard();
  }

  /** Create tab's submit — mints a brand-new room. Unlike `submitJoin`, this
   * has no display-name field: `create_room` (`commands.rs`) passes an empty
   * display name through to the same `join_room_impl` `submitJoin` uses,
   * silently reusing whatever display name the last join set (or this app's
   * OS-username default) — same "leave it, backend already knows" pattern
   * the sidebar's own silent auto-reconnect join call uses. The room-id/key
   * pair backing the new room is generated server-side (well, client-Rust-
   * side) with the `rand` crate, not here in JS — see `create_room`'s doc
   * comment for why the key can't be minted with client-side JS randomness. */
  async function submitCreate(server, roomName, errorEl) {
    if (window.__TAURI__) {
      if (errorEl) errorEl.hidden = true;
      try {
        await window.__TAURI__.core.invoke("create_room", { server, roomName });
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

    const mockRoomId = Array.from({ length: 32 }, () => Math.floor(Math.random() * 16).toString(16)).join("");
    const mockKey = Array.from({ length: 16 }, () => Math.floor(Math.random() * 16).toString(16)).join("");
    mockRooms.push({
      join_string: `${server}|${mockRoomId}|${mockKey}`,
      name: roomName,
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
        // Solid red, per the prototype's leave-confirm: Cancel is the
        // outlined/neutral option on the left and the destructive action is
        // the filled one on the right, "where the eye lands last". Both
        // being outlines gave them equal weight and left the dialog with no
        // visual primary at all.
        className: "btn btn-danger-solid",
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
  // `Why` no AUDIO section here (there used to be one): audio sharing is
  // handled directly on the video destination's own card in Stream Control
  // (the "+ Add audio" row) — a second, room-scoped audio-source picker
  // living in this panel duplicated that with no real purpose, and its
  // "Auto-Connect" checkbox was originally miswired to mean "auto-start
  // audio sharing" (a misreading of the design handoff — see this file's
  // git history, 2026-07-28). The real "Auto-Connect" checkbox below (room
  // name section) means what the name says: auto-join this room on app
  // startup.
  // ---------------------------------------------------------------------

  /** Opens the panel for a room, any state. `Why:` (2026-07-28, room-row
   * redesign) this used to require `state === "joined"` — `renderRoomRow`'s
   * row click only reached `onOpenRoom` for joined rooms, so this guard was
   * mostly decorative. Now every row's click opens the panel (see
   * `renderRoomRow`'s doc comment, components.js), so `renderRoomPanel`
   * itself is what degrades: it skips the Members/Live-streams sections
   * (which need a live `room_handle`/roster) for a disconnected/reconnecting
   * room rather than crashing on data that doesn't exist yet. */
  async function openRoomPanel(joinString) {
    const room = mockRooms.find((r) => r.join_string === joinString);
    if (!room) return;

    state.activeRoomPanel = joinString;

    if (state.myDisplayName == null && window.__TAURI__) {
      try {
        const settings = await window.__TAURI__.core.invoke("get_settings");
        state.myDisplayName = settings.display_name || null;
      } catch {
        // Leave null — renderRoomPanel falls back to "You".
      }
    }

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

  /** The room panel header's title: click-to-edit in place, replacing the
   * old separate "ROOM NAME" form section that used to sit further down
   * the panel body (removed 2026-07-28 — see
   * `superdoc/features/stream-control.md` for the rationale). Clicking the
   * plain-text title swaps it for a pre-filled `<input>`, reusing the same
   * `renamePanelRoom` call the old "Save" button used. Commit/cancel
   * behavior: Enter commits, Escape reverts without saving, and blur
   * (clicking away) commits too — chosen over blur-discards because users
   * who click away from an inline-edit field generally expect their typed
   * text to stick, not vanish silently.
   *
   * The privacy note ("Only you see this name...") that used to be a
   * permanent fixture of the old section now only appears while actively
   * editing, right under the input — keeps the panel's resting state
   * clean per the "looks cleaner" ask, without dropping the disclosure.
   * Verified against `rename_saved_room` (src/session.rs): it only ever
   * touches the local `RoomStore` file (`room_store.upsert` + `.save()`)
   * — never sends the new name over the wire — so the claim still holds. */
  function renderRoomPanelTitle(room) {
    const wrap = el("span", { className: "room-panel-title-wrap" });
    const title = el("span", { className: "room-panel-title", text: room.name });
    title.tabIndex = 0;
    title.title = "Click to rename";

    const startEdit = () => {
      wrap.innerHTML = "";
      const input = el("input", {
        className: "field-input room-panel-title-input",
        attrs: { type: "text", value: room.name },
      });
      const note = el("div", {
        className: "room-panel-privacy-note room-panel-title-privacy-note",
        text: "Only you see this name. It never leaves your machine.",
      });

      // Enter, Escape, and blur can each fire in sequence (e.g. Enter
      // blurs the input as a side effect) — `settled` makes sure only the
      // first of them actually commits/cancels.
      let settled = false;
      const commit = () => {
        if (settled) return;
        settled = true;
        renamePanelRoom(room.join_string, input.value.trim() || room.name);
      };
      const cancel = () => {
        if (settled) return;
        settled = true;
        renderRoomPanel();
      };

      input.addEventListener("keydown", (e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          commit();
        } else if (e.key === "Escape") {
          e.preventDefault();
          cancel();
        }
      });
      input.addEventListener("blur", commit);

      wrap.appendChild(input);
      wrap.appendChild(note);
      input.focus();
      input.select();
    };

    title.addEventListener("click", startEdit);
    title.addEventListener("keydown", (e) => {
      if (e.key === "Enter" || e.key === " ") {
        e.preventDefault();
        startEdit();
      }
    });

    wrap.appendChild(title);
    return wrap;
  }

  /** "Copy join link" icon-button for the room panel header — copies
   * `room.join_string` (the full `server|room-id|key` string, same value
   * the "Join a room" dialog's join-link field uses) to the clipboard via
   * `navigator.clipboard.writeText`. Flashes the icon to a checkmark for
   * 1.5s as click confirmation, then reverts — this file has no existing
   * "brief confirmation" component to reuse, so this is the minimal
   * self-contained version of that pattern. */
  function renderCopyJoinLinkButton(room) {
    let revertTimer = null;
    const btn = iconButton(
      "content_copy",
      () => {
        navigator.clipboard.writeText(room.join_string);
        btn.innerHTML = "";
        btn.appendChild(icon("check"));
        btn.title = "Copied!";
        btn.setAttribute("aria-label", "Copied!");
        clearTimeout(revertTimer);
        revertTimer = setTimeout(() => {
          btn.innerHTML = "";
          btn.appendChild(icon("content_copy"));
          btn.title = "Copy join link";
          btn.setAttribute("aria-label", "Copy join link");
        }, 1500);
      },
      { title: "Copy join link", className: "room-panel-copy-link" }
    );
    return btn;
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
    // `Why:` (2026-07-28) the dot used to be hardcoded `status-dot-joined`
    // since the panel only ever opened for joined rooms; now it reflects
    // the room's real state via the same `statusDot` helper the sidebar row
    // uses (components.js), so a disconnected/reconnecting room's panel
    // doesn't lie about being connected.
    const header = el("div", { className: "room-panel-header" });
    const top = el("div", { className: "room-panel-header-top" });
    top.appendChild(statusDot(room.state));
    top.appendChild(renderRoomPanelTitle(room));
    top.appendChild(renderCopyJoinLinkButton(room));
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
    // `Why:` (2026-07-28) meta line, and the Members/Live-streams sections
    // below, only make sense for a joined room — there's no live
    // `room_handle`/roster for a disconnected/reconnecting one (see this
    // function's other 2026-07-28 note, and `openRoomPanel`'s doc comment).
    // A disconnected row gets a plain one-line status instead; reconnecting
    // gets its own "reconnecting…" line — both replace the whole
    // members/streams body rather than rendering it empty/broken.
    const isJoined = room.state === "joined";
    const includesSelf = !!window.__TAURI__;
    const members = room.members || [];
    if (isJoined) {
      const memberCount = includesSelf ? members.length : members.length + 1;
      const sendingCount = myStreamCountForRoom(room);
      const metaText = `${memberCount} member${memberCount === 1 ? "" : "s"} · you're sending ${sendingCount} stream${sendingCount === 1 ? "" : "s"} here`;
      header.appendChild(el("div", { className: "room-panel-meta", text: metaText }));
    } else {
      const metaText = room.state === "reconnecting" ? "Reconnecting…" : "Not connected";
      header.appendChild(el("div", { className: "room-panel-meta", text: metaText }));
    }
    panel.appendChild(header);

    const body = el("div", { className: "room-panel-body" });

    if (isJoined) {
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
        youRow.appendChild(el("span", { className: "room-panel-member-name", text: state.myDisplayName || "You" }));
        youRow.appendChild(el("span", { className: "room-panel-you-badge", text: "YOU" }));
        memberList.appendChild(youRow);
        members.forEach((m) => memberList.appendChild(renderRoomPanelMemberRow(room, m)));
      } else if (members.length === 1) {
        const youRow = el("div", { className: "room-panel-member-row room-panel-member-row-you" });
        youRow.appendChild(el("span", { className: "room-panel-member-name", text: state.myDisplayName || "You" }));
        youRow.appendChild(el("span", { className: "room-panel-you-badge", text: "YOU" }));
        memberList.appendChild(youRow);
      } else {
        members.forEach((m) => memberList.appendChild(renderRoomPanelMemberRow(room, m)));
      }
      membersSection.appendChild(memberList);
      body.appendChild(membersSection);

      // ---- LIVE STREAMS ----
      // Per-stream view of `room.streams` (set by `loadRoomMembers` on the real
      // branch, hardcoded on the mock branch — see the mock `streams` array's
      // own comment). Owner display name is looked up in `members`; falls back
      // to a short id if the owner isn't in the roster for some reason (should
      // not happen in practice — a stream's owner is always a room member).
      const watchedKeys = room.watchedKeys || new Set();
      const streamRows = (room.streams || []).map((s) => {
        const ownerMember = members.find((m) => m.conn_id === s.owner);
        return {
          owner: s.owner,
          stream_id: s.stream_id,
          kind: s.kind,
          label: s.label,
          resolution: s.kind === "Video" ? `${s.width}x${s.height}` : null,
          display_name: ownerMember ? ownerMember.display_name : `Stream ${String(s.owner).slice(-4)}`,
          watching: watchedKeys.has(`${s.owner}:${s.stream_id}`),
        };
      });
      const streamsSection = el("div", { className: "room-panel-members-section" });
      streamsSection.appendChild(el("div", { className: "room-panel-section-label", text: "Live streams" }));
      const streamList = el("div", { className: "room-panel-members" });
      if (streamRows.length === 0) {
        streamList.appendChild(el("span", { className: "room-panel-member-status", text: "No one is sharing right now." }));
      } else {
        streamRows.forEach((s) => streamList.appendChild(renderRoomPanelStreamRow(room, s)));
      }
      streamsSection.appendChild(streamList);
      // `Why:` no watcher-count column — see `renderRoomPanelStreamRow`'s doc
      // comment; this is a genuine relay protocol gap, not an oversight.
      streamsSection.appendChild(
        el("div", { className: "room-panel-privacy-note", text: "Watcher counts aren't shown yet — the relay only reports them to a stream's own owner." })
      );
      body.appendChild(streamsSection);
    }

    // ---- AUTO-CONNECT ----
    // Join this room automatically at app startup — a persisted per-room
    // preference (`rooms::SavedRoom::auto_connect`, `commands::
    // set_room_auto_connect`). This replaces the AUDIO section's old
    // (incorrectly-wired) checkbox of the same name — audio sharing lives on
    // the destination card itself now (Stream Control's "+ Add audio"), so a
    // standalone audio-source picker here had no destination to belong to;
    // see `superdoc/features/stream-control.md` (2026-07-28). Reuses that old
    // checkbox's visual treatment (`room-panel-checkbox-*` in shell.css)
    // rather than a bare native input, and the "Room name" section it used to
    // live in is gone too — the panel title is click-to-edit now (see
    // `renderRoomPanelTitle`) — so this gets its own small section.
    const autoConnectSection = el("div", { className: "room-panel-autoconnect-section" });
    const autoConnectLabel = el("label", { className: "room-panel-autoconnect-row" });
    const autoConnectCheckbox = el("input", {
      className: "room-panel-checkbox-input",
      attrs: { type: "checkbox" },
    });
    autoConnectCheckbox.checked = !!room.auto_connect;
    autoConnectCheckbox.addEventListener("change", () => {
      setRoomAutoConnect(room.join_string, autoConnectCheckbox.checked);
    });
    autoConnectLabel.appendChild(autoConnectCheckbox);
    autoConnectLabel.appendChild(el("span", { className: "room-panel-checkbox-box" }));
    autoConnectLabel.appendChild(el("span", { className: "room-panel-checkbox-label", text: "Auto-Connect" }));
    autoConnectSection.appendChild(autoConnectLabel);
    body.appendChild(autoConnectSection);

    panel.appendChild(body);

    // ---- Footer: divider (via border-top in CSS) + two outline actions. ----
    // `Why:` (2026-07-28, room-row redesign) this used to be a single
    // "Leave this room" button shown only for joined rooms — the only state
    // that ever reached the panel before. Two changes now that the panel
    // opens for every state:
    //  - Relabeled to "Disconnect" (`dangerButton` never rendered an icon of
    //    its own, so there was none to remove in code — the rename itself
    //    is the ask: "Leave this room" read oddly once the panel could also
    //    represent a room that's not currently joined).
    //  - A second, separate delete/forget button sits next to it, reusing
    //    the exact same handler the sidebar row's own "Forget room" trash
    //    icon uses (`openLeaveConfirm` -> `leaveRoom` -> `forget_room` once
    //    `room.room_handle == null`, see `leaveRoom`'s doc comment) — so
    //    forgetting a saved room no longer requires closing the panel and
    //    going back to the sidebar row's own delete icon.
    // Both buttons are always present (per the design ask), but each is
    // disabled outside the one state it's actually valid for: "Disconnect"
    // needs either a live connection to drop (joined) or an in-flight
    // connect to cancel (reconnecting — see below), and "Delete room" needs
    // `room.room_handle == null` for `forget_room` to apply (disconnected
    // only — mirrors the sidebar row's own action slot). Disabling rather
    // than hiding keeps both discoverable in one place instead of the
    // footer's shape shifting per room state.
    //
    // `Why:` (2026-07-29) "reconnecting" used to disable Disconnect
    // entirely — there was no way to cancel an in-flight connect attempt.
    // Now that `cancel_connect` exists (aborts the pending connect task
    // outright, see `session::AppState::cancel_connect`), Disconnect is
    // enabled for that state too, but takes a different branch: there's no
    // live connection/destinations/watches to lose, so the "are you sure"
    // confirm dialog (`openLeaveConfirm`) doesn't apply — cancel directly.
    const footer = el("div", { className: "room-panel-footer" });
    footer.appendChild(
      dangerButton(
        "Disconnect",
        () => {
          if (room.state === "reconnecting") {
            cancelConnect(room.join_string);
            return;
          }
          const joinString = room.join_string;
          closeRoomPanel();
          openLeaveConfirm(joinString);
        },
        { className: "room-panel-leave-btn", disabled: room.state !== "joined" && room.state !== "reconnecting" }
      )
    );
    footer.appendChild(
      dangerButton(
        "Delete room",
        () => {
          const joinString = room.join_string;
          closeRoomPanel();
          openLeaveConfirm(joinString);
        },
        { className: "room-panel-leave-btn", disabled: room.state !== "disconnected" }
      )
    );
    panel.appendChild(footer);
  }

  /** Cancels an in-flight connect attempt (`commands::cancel_connect`) —
   * the room-detail panel's Disconnect button while `room.state ===
   * "reconnecting"`. Closes the panel immediately rather than waiting for a
   * round trip: `cancel_connect` never fails (no-op if nothing is pending),
   * and `refreshRooms` (driven by the sidebar's existing poll) picks up the
   * row flipping to "disconnected" the same way any other state change does.
   */
  async function cancelConnect(joinString) {
    closeRoomPanel();
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("cancel_connect");
      await refreshRooms();
      return;
    }
    const room = mockRooms.find((r) => r.join_string === joinString);
    if (room) room.state = "disconnected";
    renderRooms();
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

  /** Persists the room-detail panel's "Auto-Connect" checkbox
   * (`commands::set_room_auto_connect`) and optimistically updates the local
   * room so the checkbox doesn't visually snap back while the round trip is
   * in flight. */
  async function setRoomAutoConnect(joinString, autoConnect) {
    const room = mockRooms.find((r) => r.join_string === joinString);
    if (room) room.auto_connect = autoConnect;
    if (window.__TAURI__) {
      try {
        await window.__TAURI__.core.invoke("set_room_auto_connect", { joinString, autoConnect });
      } catch (e) {
        console.error("set_room_auto_connect failed", e);
      }
      await refreshRooms();
      return;
    }
    renderRoomPanel();
  }

  // ---------------------------------------------------------------------
  // "+ Add Room" inline card
  // ---------------------------------------------------------------------

  /** Renders the "+ Create/Join a Room" expanded card: a `pillSwitch`
   * segmented control (Join first/default, Create second — same visual
   * pattern as `bandwidthModeToggle`'s "Absolute Maximum"/"Per-Stream
   * Maximum" toggle, reusing the shared component rather than a hand-rolled
   * tab strip) over one of two field sets, sharing the same
   * `.add-room-actions` button-row styling either way. */
  function renderAddRoomCard() {
    const card = document.getElementById("add-room-card");
    const btn = document.getElementById("add-room-btn");
    card.hidden = !state.addRoomOpen;
    btn.hidden = state.addRoomOpen;
    if (!state.addRoomOpen) return;

    card.innerHTML = "";
    card.appendChild(el("div", { className: "section-label add-room-label", text: "Create / Join a Room" }));
    // `.add-room-tabs` mirrors `settings.js`'s `.settings-segmented` modifier
    // on this same shared `pillSwitch` — a scoped equal-width override (see
    // `shell.css`) rather than a change to `pillSwitch`/`.mode-toggle`
    // itself, which other call sites (bandwidth mode toggle, language
    // switch, encoder preset picker) still rely on sizing to content.
    const tabs = pillSwitch(
      [
        { value: "join", label: "Join" },
        { value: "create", label: "Create" },
      ],
      state.addRoomTab,
      (tab) => {
        state.addRoomTab = tab;
        renderAddRoomCard();
      }
    );
    tabs.classList.add("add-room-tabs");
    card.appendChild(tabs);

    const errorText = el("div", { className: "add-room-error", text: "" });
    errorText.hidden = true;
    const actions = el("div", { className: "add-room-actions" });

    if (state.addRoomTab === "create") {
      // Room Name + Server IP only — no display-name field, see
      // `submitCreate`'s doc comment for why. Room Name first, Server IP
      // second: local-nickname field before connection-detail field, same
      // order as the Join tab below.
      card.appendChild(el("label", { className: "field-label", text: "Room Name" }));
      const roomNameInput = el("input", { className: "field-input", attrs: { type: "text" } });
      card.appendChild(roomNameInput);

      card.appendChild(el("label", { className: "field-label", text: "Server IP" }));
      const serverInput = el("input", { className: "field-input", attrs: { type: "text" } });
      card.appendChild(serverInput);

      card.appendChild(errorText);

      actions.appendChild(
        primaryButton("Create", () => {
          const server = serverInput.value.trim();
          const roomName = roomNameInput.value.trim();
          if (!server || !roomName) return;
          submitCreate(server, roomName, errorText);
        })
      );
      actions.appendChild(
        secondaryButton("Cancel", () => {
          state.addRoomOpen = false;
          renderAddRoomCard();
        })
      );
      card.appendChild(actions);
      return;
    }

    // Join tab (default). No "Display name" field — display name is now one
    // global Settings-tab value, not asked per join; see `submitJoin`'s doc
    // comment.
    //
    // "Room name" is this room's LOCAL-only nickname (same concept as
    // Create's own "Room Name" field, and `SavedRoomRow.name`/`rooms.rs`'s
    // `RoomStore` — never sent over the wire), shown above the join-link
    // field so both tabs lead with the local-nickname field before the
    // connection-detail field. Deliberately optional (unlike Create's Room
    // Name, which is required): leaving it blank preserves today's
    // join-then-auto-name-from-id behavior (`join_room_impl` only pre-seeds
    // `room_store` when this is non-empty; `poll_connect_result`'s
    // id-derived fallback naming still applies otherwise). `!joinString`
    // stays the only required-field gate on the Join button.
    card.appendChild(el("label", { className: "field-label", text: "Room Name" }));
    const roomNameInput = el("input", { className: "field-input", attrs: { type: "text" } });
    card.appendChild(roomNameInput);

    card.appendChild(el("label", { className: "field-label", text: "Join link" }));
    const linkInput = el("input", { className: "field-input field-input-mono", attrs: { type: "text" } });
    card.appendChild(linkInput);

    card.appendChild(errorText);

    actions.appendChild(
      primaryButton("Join", () => {
        const joinString = linkInput.value.trim();
        if (!joinString) return;
        submitJoin(joinString, roomNameInput.value.trim(), errorText);
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
  // APP-SHELL.md's STREAMS section doc.
  //
  // A video destination with audio attached used to render as two separate
  // rows (one `kind: "video"`, one `kind: "audio"`). It's now folded into
  // one combined row: the audio row's `volume_up` glyph moves onto the end
  // of its paired video row instead of getting a row of its own. Pairing is
  // matched the same way `stream-control.js`'s `destinationCard` already
  // matches an audio row to its video card — `paired_capture_id ===
  // capture_id` *and* same `room_handle` (one capture can fan out to
  // several rooms, each with its own audio) — see that file's `audioFor`.
  // An audio destination with no matching video row present in the list
  // (audio-only share, or the video side already tore down) still renders
  // as its own row, unchanged. The combined row's "Stop" still stops only
  // the VIDEO destination (`stop_destination` operates per-destination and
  // the two captures are independent — stopping the video destination does
  // not touch the paired audio destination server/backend-side), so the
  // audio keeps flowing on its own row/destination if the caller wants it
  // stopped separately.
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
    const videoRows = mockStreams.filter((s) => s.kind !== "audio");
    const audioFor = (videoRow) =>
      mockStreams.find(
        (s) => s.kind === "audio" && s.paired_capture_id === videoRow.capture_id && s.room_handle === videoRow.room_handle
      ) || null;
    const pairedAudioIds = new Set();
    const rows = videoRows.map((videoRow) => {
      const audioRow = audioFor(videoRow);
      if (audioRow) pairedAudioIds.add(audioRow.destination_id);
      return { videoRow, audioRow };
    });
    // Unpaired audio (audio-only shares, or a stranded audio row) still gets
    // its own row, same as before.
    mockStreams.filter((s) => s.kind === "audio" && !pairedAudioIds.has(s.destination_id)).forEach((audioRow) => rows.push({ videoRow: audioRow, audioRow: null }));
    rows.forEach(({ videoRow, audioRow }) =>
      list.appendChild(renderStreamRow(videoRow, { onStop: (destinationId) => stopDestination(destinationId), audioRow }))
    );
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
    // roster (see `loadRoomMembers`'s own doc comment) — every fresh object
    // needs its `.members`/`.streams` re-fetched via `room_detail`.
    //
    // `Why:` (2026-07-28) this used to fetch only the currently-open room
    // panel's roster, because only the panel rendered a member list. The
    // sidebar ROOMS list's inline member/LIVE tree (`renderRoomRow`,
    // components.js) now needs the same data for *every visible joined
    // room*, not just the open one — so this fetches all of them, in
    // parallel. This is a real behavior/cost change: one extra
    // `room_detail` round-trip per joined room, on every event that calls
    // `refreshRooms()` (including `presence-updated`, which fires
    // routinely — see this file's header comment). Accepted rather than
    // building a cheaper "roster summary" endpoint: this app's room/member
    // counts are small (a friends screensharing app), so the added chatter
    // is a handful of extra calls, not a scaling risk. See
    // `superdoc/features/stream-control.md`'s 2026-07-28 note for the full
    // tradeoff writeup.
    await Promise.all(
      mockRooms
        .filter((r) => r.state === "joined" && r.room_handle != null)
        .map((r) => loadRoomMembers(r))
    );
    // `Why:` (2026-07-28, room-row redesign) this used to close the panel
    // whenever its room wasn't `"joined"` — back when `openRoomPanel` only
    // ever opened for a joined room, so "not joined anymore" and "nothing
    // left to show" were the same condition. They aren't anymore:
    // `renderRoomPanel` now degrades cleanly for a disconnected/reconnecting
    // room (see its own doc comment), so a room dropping out of `joined`
    // (e.g. the user's own "Disconnect" click, which calls `refreshRooms`
    // as part of `leaveRoom`) should re-render the panel in its
    // disconnected form, not slam it shut out from under the user who just
    // clicked the button that caused it. Verified against the real app: the
    // old `&& r.state === "joined"` clause here closed the panel the
    // instant `refreshRooms()` ran after ANY change while a
    // disconnected/reconnecting room's panel was open (Auto-Connect toggle
    // included, since `setRoomAutoConnect` also calls `refreshRooms`) —
    // invisible under mock-only testing because the mock branch's
    // `setRoomAutoConnect` never calls `refreshRooms` at all. The only
    // remaining reason to auto-close is the room genuinely disappearing
    // from the saved-room list (forgotten from elsewhere, e.g. another
    // window) — checked by join_string alone now, regardless of state.
    if (state.activeRoomPanel && !mockRooms.some((r) => r.join_string === state.activeRoomPanel)) {
      closeRoomPanel();
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
    // The "Go live" flow's staged card (see `superdoc/features/
    // stream-control.md`'s 2026-07-28 note): fires when a staged capture's
    // portal/pipeline finishes coming up (`starting` -> `ready`), or when it
    // disappears (its first destination just went live, so it's no longer
    // "staged" — see `dto::staged_capture_rows`'s zero-destinations filter —
    // or "Cancel" tore it down). Same "ignore the payload, just re-fetch"
    // shape as `destination-state-changed` above.
    listen("staged-capture-changed", () => {
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
      state.addRoomTab = "join"; // Join is always the pre-selected tab on open.
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

    initRangeFill();
  }

  // -------------------------------------------------------------------
  // Slider fill (design-fidelity pass, 2026-07-28).
  //
  // The prototype's sliders show an accent GRADIENT filling the track up to
  // the thumb. Chromium — the engine behind both the Tauri webview target
  // and Electron — has no `::-moz-range-progress` equivalent and cannot
  // paint that natively, so CSS alone leaves every slider as a flat grey
  // groove (which is what shipped). Firefox-family engines get the fill for
  // free from the `::-moz-range-progress` rule in shell.css.
  //
  // Rather than hand-roll a div-based slider — losing keyboard support, the
  // native drag behaviour and the a11y semantics of a real `input[range]` —
  // publish the value as a percentage in `--range-pct` and let CSS draw the
  // fill as a hard-stop gradient on the track. Delegated at the document,
  // and re-applied on each `render()`, because most of these inputs are
  // re-created wholesale by their tab.
  // -------------------------------------------------------------------

  function paintRange(input) {
    const min = Number(input.min || 0);
    const max = Number(input.max || 100);
    const span = max - min;
    const pct = span > 0 ? ((Number(input.value) - min) / span) * 100 : 0;
    input.style.setProperty("--range-pct", pct.toFixed(2) + "%");
  }

  let rangeDelegated = false;

  function initRangeFill() {
    document.querySelectorAll('input[type="range"]').forEach(paintRange);
    if (rangeDelegated) return;
    rangeDelegated = true;

    // Drag/keyboard updates.
    document.addEventListener("input", (e) => {
      if (e.target && e.target.matches && e.target.matches('input[type="range"]')) paintRange(e.target);
    });

    // Initial paint for sliders that appear later. Stream Control and
    // Settings re-create their inputs inside their own `render()`, which
    // this module neither calls nor is called by — an observer keeps the
    // fill correct without every tab having to remember to ask for it.
    new MutationObserver((records) => {
      for (const rec of records) {
        for (const node of rec.addedNodes) {
          if (node.nodeType !== 1) continue;
          if (node.matches('input[type="range"]')) paintRange(node);
          node.querySelectorAll && node.querySelectorAll('input[type="range"]').forEach(paintRange);
        }
      }
    }).observe(document.body, { childList: true, subtree: true });
  }

  window.Shell = { refreshRooms, refreshStreams, refreshAll, paintRanges: initRangeFill };

  document.addEventListener("DOMContentLoaded", init);
})();
