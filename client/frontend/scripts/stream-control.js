// stream-control.js — Stream Control tab content.
//
// Mock-data-first, per the port's Playwright-testability requirement: every
// backend touchpoint below is a small `async function` that checks
// `window.__TAURI__` and calls the (not-yet-implemented) real command on one
// branch, falling back to mutating an in-memory mock array/object on the
// other. That keeps this file rendering and fully interactive in a plain
// browser with no Tauri shell present, *and* keeps that property once a
// later worker fills in the `invoke(...)` branches — nothing here has to be
// restructured to add the real call, only the branch body gets filled in.
// See `superdoc/ui/APP-SHELL.md` / `superdoc/features/stream-control.md` for
// the eventual command shapes this guesses at.
//
// Layout, top to bottom (the redesign this fixes vs. the old egui tab, see
// stream-control.css's header comment):
//   1. Bandwidth — compact summary strip, mode toggle, overshoot banner.
//   2. Active shares — the prominent, equal-width destination grid.
//   3. Start a stream — compact inline form.
//   4. Room-scoped: Members/rename + Share audio, for one selected room.

(function () {
  "use strict";

  const { el, card, warningBanner, bandwidthModeToggle, destinationCard, secondaryButton, primaryButton, renderEmptyState } = window.Components;

  // ---------------------------------------------------------------------
  // Mock data (fallback bodies for the fetch/action functions below).
  // ---------------------------------------------------------------------

  const MOCK_JOINED_ROOMS = [
    { id: "r1", name: "Movie Night" },
    { id: "r2", name: "Gaming Night" },
    { id: "r5", name: "Book Club" },
  ];

  // `captureId` groups destinations sharing one screen capture, for
  // fan-out's "disable rooms this capture already feeds" rule.
  const MOCK_DESTINATIONS = [
    {
      id: "d1",
      captureId: "cap1",
      roomId: "r1",
      roomLabel: "Movie Night",
      state: "live",
      resolution: "1920x1080",
      fps: 30,
      bitrateKbps: 6000,
      isAudio: false,
      pendingRoleText: null,
    },
    {
      id: "d2",
      captureId: "cap2",
      roomId: "r2",
      roomLabel: "Gaming Night",
      state: "rebuilding",
      resolution: "1280x720",
      fps: 60,
      bitrateKbps: 4000,
      isAudio: false,
      pendingRoleText: null,
    },
    {
      id: "d3",
      captureId: "cap1",
      roomId: "r5",
      roomLabel: "Book Club",
      state: "stalled",
      resolution: "1920x1080",
      fps: 30,
      bitrateKbps: 3000,
      isAudio: false,
      pendingRoleText: null,
    },
  ];

  // Default mode here is deliberately "absolute" (production default is
  // Per-Stream per stream-control.md) so the overshoot warning banner is
  // visible on first load without an extra click — a demoability deviation,
  // documented in APP-SHELL.md's doc update.
  const MOCK_BANDWIDTH = { mode: "absolute", absoluteTotalKbps: 8000, absoluteFloorKbps: 3000 };

  const MOCK_MEMBERS_BY_ROOM = {
    r1: [{ name: "Ana", resolution: "1920x1080", fps: null, connId: "conn_id 7b29f8d3ccf8faee" }],
    r2: [],
    r5: [{ name: "Priya", resolution: "1280x720", fps: null, connId: "conn_id a1029f8d3ccf8f11" }],
  };

  const MOCK_AUDIO_SOURCES = ["Default speakers", "Game (app audio)", "Microphone"];

  // ---------------------------------------------------------------------
  // Data touchpoints — every one of these is the `window.__TAURI__` presence
  // check the port's Playwright-testability requirement calls for: a real
  // `invoke(...)` stub on one branch (not implemented by this worker — the
  // command names/signatures don't exist yet), a mock-state fallback on the
  // other. In-memory mock arrays are captured by closure and mutated
  // in-place so `render()` always reads current state after any of these
  // resolve.
  // ---------------------------------------------------------------------

  let mockJoinedRooms = MOCK_JOINED_ROOMS;
  let mockDestinations = MOCK_DESTINATIONS.slice();
  let bandwidth = Object.assign({}, MOCK_BANDWIDTH);
  let mockMembersByRoom = MOCK_MEMBERS_BY_ROOM;
  let mockAudioSources = MOCK_AUDIO_SOURCES;

  async function fetchJoinedRooms() {
    if (window.__TAURI__) {
      return await window.__TAURI__.core.invoke("list_joined_rooms"); // TODO: not implemented yet — backend worker's scope.
    }
    return MOCK_JOINED_ROOMS;
  }

  async function fetchActiveDestinations() {
    if (window.__TAURI__) {
      return await window.__TAURI__.core.invoke("list_active_destinations"); // TODO: not implemented yet.
    }
    return MOCK_DESTINATIONS.slice();
  }

  async function fetchBandwidthSettings() {
    if (window.__TAURI__) {
      return await window.__TAURI__.core.invoke("get_bandwidth_settings"); // TODO: not implemented yet.
    }
    return Object.assign({}, MOCK_BANDWIDTH);
  }

  async function fetchRoomMembers(roomId) {
    if (window.__TAURI__) {
      return await window.__TAURI__.core.invoke("list_room_members", { roomId }); // TODO: not implemented yet.
    }
    return MOCK_MEMBERS_BY_ROOM[roomId] || [];
  }

  async function fetchAudioSources() {
    if (window.__TAURI__) {
      return await window.__TAURI__.core.invoke("list_audio_sources"); // TODO: not implemented yet.
    }
    return MOCK_AUDIO_SOURCES;
  }

  async function setBandwidthMode(mode) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_bandwidth_mode", { mode }); // TODO: not implemented yet.
    } else {
      bandwidth.mode = mode;
    }
    render();
  }

  async function setAbsoluteTotal(kbps) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_absolute_total", { kbps }); // TODO: not implemented yet.
    } else {
      bandwidth.absoluteTotalKbps = kbps;
    }
    render();
  }

  async function setAbsoluteFloor(kbps) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_absolute_floor", { kbps }); // TODO: not implemented yet.
    } else {
      bandwidth.absoluteFloorKbps = kbps;
    }
    render();
  }

  async function applyDestinationEdit(id, patch) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("change_destination", Object.assign({ id }, patch)); // TODO: not implemented yet.
    } else {
      const d = mockDestinations.find((x) => x.id === id);
      if (d) Object.assign(d, patch);
    }
    render();
  }

  async function stopDestination(id) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("stop_destination", { id }); // TODO: not implemented yet.
      // Real branch would then refetch; mock branch mutates directly below.
    }
    mockDestinations = mockDestinations.filter((x) => x.id !== id);
    render();
  }

  async function transferDestination(id, targetRoomId) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("transfer_destination", { id, targetRoomId }); // TODO: not implemented yet.
      return;
    }
    mockTransfer(id, targetRoomId);
  }

  async function fanoutDestination(id, targetRoomId) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("add_destination", { fromDestinationId: id, targetRoomId }); // TODO: not implemented yet.
      return;
    }
    mockFanout(id, targetRoomId);
  }

  async function startVideoShare(roomId, fps, bitrateKbps) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("start_video_share", { roomId, fps, bitrateKbps }); // TODO: not implemented yet.
      return;
    }
    mockStartStream(roomId, fps, bitrateKbps);
  }

  async function startAudioShare(roomId, source) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("start_audio_share", { roomId, source }); // TODO: not implemented yet.
      return;
    }
    const room = mockJoinedRooms.find((r) => r.id === roomId);
    mockDestinations.push({
      id: "d" + destIdSeq++,
      captureId: "audio-" + destIdSeq,
      roomId,
      roomLabel: room ? room.name : roomId,
      state: "live",
      resolution: "audio",
      fps: "—",
      bitrateKbps: 128,
      isAudio: true,
      pendingRoleText: null,
    });
    render();
  }

  async function renameRoom(roomId, name) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("rename_room", { roomId, name }); // TODO: not implemented yet.
    } else {
      state.roomName = name;
    }
    state.savedHint = true;
    render();
  }

  const state = {
    scopedRoomId: "r1",
    roomName: "Movie Night",
    savedHint: false,
    startForm: { roomId: "r1", fps: 30, bitrateKbps: 8000 },
    startInFlight: false,
    startError: null,
    audioSource: "Default speakers",
  };

  let destIdSeq = 100;

  // ---------------------------------------------------------------------
  // Bandwidth allocation (mock, mirrors `bandwidth::allocate`'s shape at a
  // toy level — real weighting/hysteresis lives server-side in `App`).
  // ---------------------------------------------------------------------

  function computeAllocation() {
    // Every non-audio destination reserves its floor, including a Stalled
    // one — its capture is temporarily down, not gone, so its bandwidth
    // budget isn't freed for others. Mirrors stream-control.md's allocator
    // note: only destinations with *no branch at all yet* (still starting)
    // are excluded.
    const videoDests = mockDestinations.filter((d) => !d.isAudio && d.state !== "starting");
    const n = videoDests.length || 1;
    const floorTotal = bandwidth.absoluteFloorKbps * n;
    const overshootKbps = Math.max(0, floorTotal - bandwidth.absoluteTotalKbps);
    const perDest = Math.floor(bandwidth.absoluteTotalKbps / n);
    const share = Math.max(bandwidth.absoluteFloorKbps, perDest);
    return { overshootKbps, share, n };
  }

  // ---------------------------------------------------------------------
  // Render
  // ---------------------------------------------------------------------

  function render() {
    const root = document.getElementById("stream-control-root");
    if (!root) return;
    root.innerHTML = "";

    const grid = el("div", { className: "sc-grid" });
    grid.appendChild(renderBandwidthCard());

    const alloc = computeAllocation();
    if (bandwidth.mode === "absolute" && alloc.overshootKbps > 0) {
      grid.appendChild(
        warningBanner(
          `Over budget by ${alloc.overshootKbps} kbps: the guaranteed floors need ${
            bandwidth.absoluteFloorKbps * alloc.n
          } kbps but the maximum is ${bandwidth.absoluteTotalKbps} kbps. No stream is starved below its floor.`
        )
      );
    }

    grid.appendChild(renderActiveSharesCard(alloc));
    grid.appendChild(renderStartStreamCard());
    grid.appendChild(renderRoomScopedRow());

    root.appendChild(grid);
  }

  // ---------------------------------------------------------------------
  // Bandwidth card
  // ---------------------------------------------------------------------

  function renderBandwidthCard() {
    const top = el("div", { className: "sc-bandwidth-top" });
    top.appendChild(bandwidthModeToggle(bandwidth.mode, (mode) => setBandwidthMode(mode)));

    const liveCount = mockDestinations.filter((d) => !d.isAudio).length;
    const roomsFed = new Set(mockDestinations.map((d) => d.roomId)).size;
    const summaryParts = [`${liveCount} destination${liveCount === 1 ? "" : "s"}`];
    if (roomsFed > 1) summaryParts.push(`streaming to ${roomsFed} rooms — about ${roomsFed}x the outgoing bandwidth of one stream`);
    top.appendChild(el("span", { className: "sc-bandwidth-summary", text: summaryParts.join(" · ") }));

    const controls = el("div", { className: "sc-bandwidth-controls" });
    if (bandwidth.mode === "absolute") {
      controls.appendChild(bandwidthField("Total maximum", bandwidth.absoluteTotalKbps, 500, 40000, (v) => setAbsoluteTotal(v)));
      controls.appendChild(bandwidthField("Floor per stream", bandwidth.absoluteFloorKbps, 100, 20000, (v) => setAbsoluteFloor(v)));
      controls.appendChild(
        el("span", {
          className: "sc-fanout-note",
          text: "Split by resolution × framerate across every active stream, recomputed about once a second.",
        })
      );
    } else {
      controls.appendChild(el("span", { className: "sc-fanout-note", text: "Each stream below has its own bitrate — set it on its card." }));
    }

    return card("BANDWIDTH", [top, controls], { className: "sc-bandwidth-card" });
  }

  function bandwidthField(label, value, min, max, onChange) {
    const wrap = el("div", { className: "sc-bandwidth-field" });
    wrap.appendChild(el("label", { className: "field-label", text: label }));
    const slider = el("input", { attrs: { type: "range", min: String(min), max: String(max), step: "100", value: String(value) } });
    const out = el("span", { className: "sc-bandwidth-field-value", text: value + " kbps" });
    slider.addEventListener("input", () => (out.textContent = slider.value + " kbps"));
    slider.addEventListener("change", () => onChange(Number(slider.value)));
    wrap.appendChild(slider);
    wrap.appendChild(out);
    return wrap;
  }

  // ---------------------------------------------------------------------
  // Active shares card
  // ---------------------------------------------------------------------

  function renderActiveSharesCard(alloc) {
    let body;
    if (mockDestinations.length === 0) {
      body = renderEmptyState("No active shares — start one below.", null, null);
    } else {
      const gridEl = el("div", { className: "sc-active-shares-grid" });
      mockDestinations.forEach((dest) => {
        const roomsAlreadyFedByCapture = mockDestinations.filter((d) => d.captureId === dest.captureId).map((d) => d.roomId);
        const withShare =
          bandwidth.mode === "absolute" && !dest.isAudio && dest.state !== "starting"
            ? Object.assign({}, dest, { computedShareKbps: alloc.share })
            : dest;
        gridEl.appendChild(
          destinationCard(
            withShare,
            {
              onEditApply: (id, patch) => applyDestinationEdit(id, patch),
              onTransfer: (id, targetRoomId) => transferDestination(id, targetRoomId),
              onFanout: (id, targetRoomId) => fanoutDestination(id, targetRoomId),
              onStop: (id) => stopDestination(id),
            },
            {
              roomOptions: mockJoinedRooms,
              perStreamBitrateEditable: bandwidth.mode === "per-stream" && !dest.isAudio,
              roomsAlreadyFedByCapture,
            }
          )
        );
      });
      body = gridEl;
    }
    return card("ACTIVE SHARES", [body], { className: "sc-active-shares-card" });
  }

  function mockTransfer(sourceId, targetRoomId) {
    const source = mockDestinations.find((d) => d.id === sourceId);
    if (!source) return;
    const targetRoom = mockJoinedRooms.find((r) => r.id === targetRoomId);
    source.pendingRoleText = `Transferring to ${targetRoom ? targetRoom.name : targetRoomId}…`;

    const newDest = {
      id: "d" + destIdSeq++,
      captureId: source.captureId,
      roomId: targetRoomId,
      roomLabel: targetRoom ? targetRoom.name : targetRoomId,
      state: "starting",
      resolution: source.resolution,
      fps: source.fps,
      bitrateKbps: source.bitrateKbps,
      isAudio: source.isAudio,
      pendingRoleText: `Transferring from ${source.roomLabel}…`,
    };
    mockDestinations.push(newDest);
    render();

    // Mock "make-before-break": once the new branch's first IDR would land,
    // drop the old destination and clear the new one's pending label.
    setTimeout(() => {
      newDest.state = "live";
      newDest.pendingRoleText = null;
      mockDestinations = mockDestinations.filter((d) => d.id !== sourceId);
      render();
    }, 1200);
  }

  function mockFanout(sourceId, targetRoomId) {
    const source = mockDestinations.find((d) => d.id === sourceId);
    if (!source) return;
    const targetRoom = mockJoinedRooms.find((r) => r.id === targetRoomId);
    const newDest = {
      id: "d" + destIdSeq++,
      captureId: source.captureId,
      roomId: targetRoomId,
      roomLabel: targetRoom ? targetRoom.name : targetRoomId,
      state: "starting",
      resolution: source.resolution,
      fps: source.fps,
      bitrateKbps: source.bitrateKbps,
      isAudio: source.isAudio,
      pendingRoleText: "Also sending here…",
    };
    mockDestinations.push(newDest);
    render();
    setTimeout(() => {
      newDest.state = "live";
      newDest.pendingRoleText = null;
      render();
    }, 900);
  }

  // ---------------------------------------------------------------------
  // Start a stream card
  // ---------------------------------------------------------------------

  function renderStartStreamCard() {
    const form = el("div", { className: "sc-start-form" });

    const roomField = el("div", { className: "sc-start-field" });
    roomField.appendChild(el("label", { className: "field-label", text: "Room" }));
    const roomSelect = el("select");
    mockJoinedRooms.forEach((r) => roomSelect.appendChild(el("option", { text: r.name, attrs: { value: r.id } })));
    roomSelect.value = state.startForm.roomId;
    roomSelect.addEventListener("change", () => (state.startForm.roomId = roomSelect.value));
    roomField.appendChild(roomSelect);
    form.appendChild(roomField);

    const fpsField = el("div", { className: "sc-start-field" });
    fpsField.appendChild(el("label", { className: "field-label", text: "Framerate" }));
    const fpsSelect = el("select");
    [15, 30, 60].forEach((fps) => fpsSelect.appendChild(el("option", { text: fps + " fps", attrs: { value: String(fps) } })));
    fpsSelect.value = String(state.startForm.fps);
    fpsSelect.addEventListener("change", () => (state.startForm.fps = Number(fpsSelect.value)));
    fpsField.appendChild(fpsSelect);
    form.appendChild(fpsField);

    // Offering a manual bitrate in Absolute mode would be a lie — the
    // allocator overwrites it within a second of the stream going live
    // (APP-SHELL.md's Stream Control section, item 2).
    if (bandwidth.mode === "per-stream") {
      const bitrateField = el("div", { className: "sc-start-field" });
      bitrateField.appendChild(el("label", { className: "field-label", text: "Bitrate (kbps)" }));
      const bitrateInput = el("input", {
        attrs: { type: "number", min: "200", max: "40000", step: "100", value: String(state.startForm.bitrateKbps) },
      });
      bitrateInput.addEventListener("change", () => (state.startForm.bitrateKbps = Number(bitrateInput.value) || 200));
      bitrateField.appendChild(bitrateInput);
      form.appendChild(bitrateField);
    }

    form.appendChild(
      primaryButton("Share Screen", () => {
        if (state.startInFlight) return;
        state.startError = null;
        state.startInFlight = true;
        render();
        startVideoShare(state.startForm.roomId, state.startForm.fps, state.startForm.bitrateKbps).finally(() => {
          state.startInFlight = false;
        });
      })
    );

    if (state.startError) form.appendChild(el("div", { className: "sc-start-error", text: state.startError }));
    if (state.startInFlight) form.appendChild(el("div", { className: "sc-start-progress", text: "Waiting for screen-share permission…" }));

    return card("START A STREAM", [form], { className: "sc-start-stream-card" });
  }

  function mockStartStream(roomId, fps, bitrateKbps) {
    const room = mockJoinedRooms.find((r) => r.id === roomId);
    const newDest = {
      id: "d" + destIdSeq++,
      captureId: "cap" + destIdSeq,
      roomId,
      roomLabel: room ? room.name : roomId,
      state: "starting",
      resolution: "1920x1080",
      fps,
      bitrateKbps,
      isAudio: false,
      pendingRoleText: null,
    };

    // Mimics the design handoff's "Waiting for screen-share permission…" ->
    // Starting -> Live progression (shot 09) at toy timescales.
    setTimeout(() => {
      mockDestinations.push(newDest);
      render();
      setTimeout(() => {
        newDest.state = "live";
        render();
      }, 800);
    }, 700);
  }

  // ---------------------------------------------------------------------
  // Room-scoped row: Members/rename + Share audio.
  // ---------------------------------------------------------------------

  function renderRoomScopedRow() {
    const header = el("div", { className: "sc-room-scoped-header" });
    header.appendChild(el("span", { text: "Room-scoped settings for:" }));
    const select = el("select");
    mockJoinedRooms.forEach((r) => select.appendChild(el("option", { text: r.name, attrs: { value: r.id } })));
    select.value = state.scopedRoomId;
    select.addEventListener("change", async () => {
      state.scopedRoomId = select.value;
      state.roomName = mockJoinedRooms.find((r) => r.id === select.value).name;
      state.savedHint = false;
      mockMembersByRoom[state.scopedRoomId] = await fetchRoomMembers(state.scopedRoomId);
      render();
    });
    header.appendChild(select);

    const row = el("div", { className: "sc-room-scoped-row" });
    row.appendChild(renderMembersCard());
    row.appendChild(renderShareAudioCard());

    const wrap = el("div", { className: "sc-room-scoped-wrap" });
    wrap.appendChild(header);
    wrap.appendChild(row);
    return wrap;
  }

  function renderMembersCard() {
    const members = mockMembersByRoom[state.scopedRoomId] || [];
    const body = [];
    if (members.length === 0) {
      body.push(el("div", { className: "sc-member-row-conn", text: "No other members yet." }));
    } else {
      members.forEach((m) => {
        const row = el("div", { className: "sc-member-row" });
        row.appendChild(el("span", { text: m.name }));
        row.appendChild(el("span", { className: "sc-member-row-conn", text: m.connId }));
        body.push(row);
      });
    }

    const renameRow = el("div", { className: "sc-rename-row" });
    const nameInput = el("input", { attrs: { type: "text", value: state.roomName } });
    renameRow.appendChild(nameInput);
    renameRow.appendChild(
      secondaryButton("Save", () => renameRoom(state.scopedRoomId, nameInput.value.trim() || state.roomName))
    );

    const children = [el("div", { className: "field-label", text: "Room name" }), renameRow];
    if (state.savedHint) children.push(el("div", { className: "sc-saved-hint", text: "Saved." }));

    return card("MEMBERS", body.concat(children), { className: "sc-members-card" });
  }

  function renderShareAudioCard() {
    const row = el("div", { className: "sc-share-audio-row" });
    row.appendChild(
      secondaryButton("Refresh", async () => {
        mockAudioSources = await fetchAudioSources();
        render();
      })
    );
    const select = el("select");
    mockAudioSources.forEach((s) => select.appendChild(el("option", { text: s, attrs: { value: s } })));
    select.value = state.audioSource;
    select.addEventListener("change", () => (state.audioSource = select.value));
    row.appendChild(select);
    row.appendChild(primaryButton("Share Audio", () => startAudioShare(state.scopedRoomId, state.audioSource)));
    return card("SHARE AUDIO", [row], { className: "sc-share-audio-card" });
  }

  // ---------------------------------------------------------------------
  // Init — fetches every data touchpoint (mock branch by default in a plain
  // browser), then does the first render.
  // ---------------------------------------------------------------------

  async function init() {
    mockJoinedRooms = await fetchJoinedRooms();
    mockDestinations = await fetchActiveDestinations();
    bandwidth = await fetchBandwidthSettings();
    mockAudioSources = await fetchAudioSources();

    if (mockJoinedRooms.length > 0) {
      state.scopedRoomId = mockJoinedRooms[0].id;
      state.roomName = mockJoinedRooms[0].name;
      state.startForm.roomId = mockJoinedRooms[0].id;
    }
    if (mockAudioSources.length > 0) state.audioSource = mockAudioSources[0];
    mockMembersByRoom[state.scopedRoomId] = await fetchRoomMembers(state.scopedRoomId);

    render();
  }

  document.addEventListener("DOMContentLoaded", init);
})();
