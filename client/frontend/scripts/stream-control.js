// stream-control.js — Stream Control tab content.
//
// Mock/real fallback pattern (see `superdoc/ui/FRONTEND-SHELL.md`): every
// backend touchpoint below is a small `async function` that checks
// `window.__TAURI__` and calls the real `#[tauri::command]` on one branch,
// falling back to mutating an in-memory mock array/object on the other. Mock
// data shapes mirror the real DTOs (`dto::DestinationRow`/`dto::SettingsDto`/
// `dto::AudioSourceDto`) field-for-field — snake_case `destination_id`/
// `capture_id`/`room_handle`/`bitrate_kbps`/…, `kind: 'video'|'audio'`
// instead of a boolean `isAudio`, bandwidth mode `'per_stream'` (underscore,
// matching `dto::bandwidth_mode_str`) instead of `'per-stream'` — so
// `Components.destinationCard`/`bandwidthModeToggle` render identically fed
// either source.
//
// Real command surface used here (see `src-tauri/src/commands.rs`):
//   list_saved_rooms, list_active_streams, get_settings, room_detail,
//   list_audio_sources, select_audio_source, set_bandwidth_mode,
//   set_bandwidth_total, set_bandwidth_floor, edit_destination,
//   stop_destination, transfer_destination, start_destination,
//   start_video_share, set_bitrate, set_framerate, start_audio_share,
//   rename_room.
// Notably `start_video_share`/`start_audio_share` only take a `room_handle`
// — framerate/bitrate/audio-source are process-wide settings applied via
// `set_framerate`/`set_bitrate`/`select_audio_source` *before* starting the
// share, not per-call arguments (this file's mock-era stubs guessed
// otherwise; fixed here to match the real signatures in `commands.rs`).
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
    { id: 1, name: "Movie Night", join_string: "server|11111111|movie-night-key" },
    { id: 2, name: "Gaming Night", join_string: "server|22222222|gaming-night-key" },
    { id: 5, name: "Book Club", join_string: "server|55555555|book-club-key" },
  ];

  // `capture_id` groups destinations sharing one screen capture, for
  // fan-out's "disable rooms this capture already feeds" rule.
  const MOCK_DESTINATIONS = [
    {
      destination_id: 1,
      capture_id: 1,
      room_handle: 1,
      room_label: "Movie Night",
      kind: "video",
      state: "live",
      width: 1920,
      height: 1080,
      framerate: 30,
      bitrate_kbps: 6000,
      pending: null,
    },
    {
      destination_id: 2,
      capture_id: 2,
      room_handle: 2,
      room_label: "Gaming Night",
      kind: "video",
      state: "rebuilding",
      width: 1280,
      height: 720,
      framerate: 60,
      bitrate_kbps: 4000,
      pending: null,
    },
    {
      destination_id: 3,
      capture_id: 1,
      room_handle: 5,
      room_label: "Book Club",
      kind: "video",
      state: "stalled",
      width: 1920,
      height: 1080,
      framerate: 30,
      bitrate_kbps: 3000,
      pending: null,
    },
  ];

  // Default mode here is deliberately "absolute" (production default is
  // per_stream per stream-control.md) so the overshoot warning banner is
  // visible on first load without an extra click — a demoability deviation,
  // documented in APP-SHELL.md's doc update. Field names mirror
  // `dto::SettingsDto`'s bandwidth-related fields exactly.
  const MOCK_BANDWIDTH = { bandwidth_mode: "absolute", absolute_total_kbps: 8000, destination_floor_kbps: 3000 };

  const MOCK_MEMBERS_BY_ROOM = {
    // Kept within `Number.MAX_SAFE_INTEGER` — a real `conn_id` is a `u64` and
    // can exceed that, which is a wire-format limitation of JSON numbers
    // shared by every u64 field this frontend receives, not something fixable
    // here; these mock values are just picked to render exactly.
    1: [{ conn_id: 0x7b29f8d3cc00, display_name: "Ana", resolution: "1920x1080", fps: null }],
    2: [],
    5: [{ conn_id: 0xa1029f8d3c00, display_name: "Priya", resolution: "1280x720", fps: null }],
  };

  const MOCK_AUDIO_SOURCES = [
    { id: 1, name: "Default speakers" },
    { id: 2, name: "Game (app audio)" },
    { id: 3, name: "Microphone" },
  ];

  // ---------------------------------------------------------------------
  // Data touchpoints
  // ---------------------------------------------------------------------

  let mockJoinedRooms = MOCK_JOINED_ROOMS;
  let mockDestinations = MOCK_DESTINATIONS.slice();
  let bandwidth = Object.assign({}, MOCK_BANDWIDTH);
  let mockMembersByRoom = MOCK_MEMBERS_BY_ROOM;
  let mockAudioSources = MOCK_AUDIO_SOURCES;
  // Real-backend-only: the last `bandwidth-overshoot` event's amount (there's
  // no local allocator to compute this from in the real branch — the
  // session-side `apply_bandwidth_allocation` owns that, and reports it live
  // via the event `driver.rs` emits). `null`/`0` both mean "no overshoot
  // currently reported".
  let realOvershootKbps = null;

  function pendingRoleText(pending) {
    switch (pending) {
      case "fan_out":
        return "Also sending here…";
      case "transfer_target":
        return "Transferring in…";
      case "transfer_source":
        return "Transferring out…";
      default:
        return null;
    }
  }

  async function fetchJoinedRooms() {
    if (window.__TAURI__) {
      const rows = await window.__TAURI__.core.invoke("list_saved_rooms");
      return rows.filter((r) => r.state === "joined").map((r) => ({ id: r.room_handle, name: r.name, join_string: r.join_string }));
    }
    return MOCK_JOINED_ROOMS;
  }

  async function fetchActiveDestinations() {
    if (window.__TAURI__) {
      const rows = await window.__TAURI__.core.invoke("list_active_streams");
      rows.forEach((d) => {
        const room = mockJoinedRooms.find((r) => r.id === d.room_handle);
        d.room_label = room ? room.name : String(d.room_handle);
        d.pendingRoleText = pendingRoleText(d.pending);
      });
      return rows;
    }
    return MOCK_DESTINATIONS.slice();
  }

  async function fetchBandwidthSettings() {
    if (window.__TAURI__) {
      const settings = await window.__TAURI__.core.invoke("get_settings");
      return { bandwidth_mode: settings.bandwidth_mode, absolute_total_kbps: settings.absolute_total_kbps, destination_floor_kbps: settings.destination_floor_kbps };
    }
    return Object.assign({}, MOCK_BANDWIDTH);
  }

  async function fetchRoomMembers(roomHandle) {
    if (window.__TAURI__) {
      const detail = await window.__TAURI__.core.invoke("room_detail", { roomHandle });
      if (!detail) return [];
      return detail.members.map((m) => {
        const videoStream = detail.streams.find((s) => s.owner === m.conn_id && s.kind === "Video");
        return {
          conn_id: m.conn_id,
          display_name: m.display_name,
          resolution: videoStream ? `${videoStream.width}x${videoStream.height}` : null,
          fps: null,
        };
      });
    }
    return mockMembersByRoom[roomHandle] || [];
  }

  async function fetchAudioSources() {
    if (window.__TAURI__) {
      // `list_audio_sources` queries the real system audio stack and
      // rejects (`Result::Err` on the Rust side) if that's unavailable —
      // e.g. no PulseAudio/PipeWire session at all. That's a legitimate
      // runtime state (headless/CI environments, a machine with no audio
      // server), not a bug, and must not take the whole tab down with it —
      // caught here so "Share Audio" just shows no sources instead of
      // `init()` never reaching `render()`.
      //
      // It can also *hang* rather than reject: `spawn_blocking`ing straight
      // into a system audio client (PulseAudio/PipeWire) can block
      // indefinitely trying to connect when there's no audio server running
      // at all (a real condition — headless machines, minimal containers —
      // not just a test-only edge case), and a hung promise is worse than a
      // rejected one here: nothing ever catches it, so `await`ing it inline
      // would silently freeze the *entire* tab's first paint forever, not
      // just the audio picker. `withTimeout` bounds that wait so a slow/dead
      // audio backend degrades to "no sources found" instead.
      try {
        return await withTimeout(window.__TAURI__.core.invoke("list_audio_sources"), 3000);
      } catch (e) {
        console.error("list_audio_sources failed or timed out", e);
        return [];
      }
    }
    return MOCK_AUDIO_SOURCES;
  }

  function withTimeout(promise, ms) {
    return Promise.race([promise, new Promise((_, reject) => setTimeout(() => reject(new Error(`timed out after ${ms}ms`)), ms))]);
  }

  async function setBandwidthMode(mode) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_bandwidth_mode", { mode });
    } else {
      bandwidth.bandwidth_mode = mode;
    }
    render();
  }

  async function setAbsoluteTotal(kbps) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_bandwidth_total", { totalKbps: kbps });
    } else {
      bandwidth.absolute_total_kbps = kbps;
    }
    render();
  }

  async function setAbsoluteFloor(kbps) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_bandwidth_floor", { floorKbps: kbps });
    } else {
      bandwidth.destination_floor_kbps = kbps;
    }
    render();
  }

  async function applyDestinationEdit(destinationId, patch) {
    if (window.__TAURI__) {
      const dest = mockDestinations.find((d) => d.destination_id === destinationId);
      // `edit_destination` takes a full edit (scale/framerate/bitrate), not a
      // sparse patch — this tab only offers a bitrate slider, so scale stays
      // at 100% and framerate stays whatever the destination already has.
      await window.__TAURI__.core.invoke("edit_destination", {
        destinationId,
        scalePercent: 100,
        framerate: dest ? dest.framerate : 30,
        bitrateKbps: patch.bitrateKbps,
      });
      return;
    }
    const d = mockDestinations.find((x) => x.destination_id === destinationId);
    if (d) Object.assign(d, { bitrate_kbps: patch.bitrateKbps });
    render();
  }

  async function stopDestination(destinationId) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("stop_destination", { destinationId });
      return;
    }
    mockDestinations = mockDestinations.filter((x) => x.destination_id !== destinationId);
    render();
  }

  async function transferDestination(destinationId, targetRoomHandle) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("transfer_destination", { destinationId, targetRoom: targetRoomHandle });
      return;
    }
    mockTransfer(destinationId, targetRoomHandle);
  }

  async function fanoutDestination(captureId, targetRoomHandle) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("start_destination", { captureId, roomHandle: targetRoomHandle });
      return;
    }
    mockFanout(captureId, targetRoomHandle);
  }

  async function startVideoShare(roomHandle, fps, bitrateKbps) {
    if (window.__TAURI__) {
      // Framerate/bitrate are process-wide settings applied before the share
      // starts (`start_video_share` itself only takes a room handle — see
      // this file's header note).
      await window.__TAURI__.core.invoke("set_framerate", { fps });
      await window.__TAURI__.core.invoke("set_bitrate", { kbps: bitrateKbps });
      await window.__TAURI__.core.invoke("start_video_share", { roomHandle });
      return;
    }
    mockStartStream(roomHandle, fps, bitrateKbps);
  }

  async function startAudioShare(roomHandle, source) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("select_audio_source", { id: source.id, name: source.name });
      await window.__TAURI__.core.invoke("start_audio_share", { roomHandle });
      return;
    }
    const room = mockJoinedRooms.find((r) => r.id === roomHandle);
    mockDestinations.push({
      destination_id: destIdSeq++,
      capture_id: destIdSeq,
      room_handle: roomHandle,
      room_label: room ? room.name : String(roomHandle),
      kind: "audio",
      state: "live",
      width: 0,
      height: 0,
      framerate: 0,
      bitrate_kbps: 128,
      pending: null,
    });
    render();
  }

  async function renameRoom(joinString, name) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("rename_room", { joinString, newName: name });
      if (window.Shell) window.Shell.refreshRooms();
    } else {
      state.roomName = name;
    }
    state.savedHint = true;
    render();
  }

  const state = {
    scopedRoomHandle: 1,
    roomName: "Movie Night",
    savedHint: false,
    startForm: { roomHandle: 1, fps: 30, bitrateKbps: 8000 },
    startInFlight: false,
    startError: null,
    audioSource: MOCK_AUDIO_SOURCES[0],
  };

  let destIdSeq = 100;

  // ---------------------------------------------------------------------
  // Bandwidth allocation (mock, mirrors `bandwidth::allocate`'s shape at a
  // toy level — real weighting/hysteresis lives in `AppState::
  // apply_bandwidth_allocation`/`bandwidth.rs`, and its real overshoot is
  // reported live via the `bandwidth-overshoot` event, not recomputed here —
  // see `setOvershoot`).
  // ---------------------------------------------------------------------

  function computeAllocation() {
    // Every non-audio destination reserves its floor, including a Stalled
    // one — its capture is temporarily down, not gone, so its bandwidth
    // budget isn't freed for others. Mirrors stream-control.md's allocator
    // note: only destinations with *no branch at all yet* (still starting)
    // are excluded.
    const videoDests = mockDestinations.filter((d) => d.kind !== "audio" && d.state !== "starting");
    const n = videoDests.length || 1;
    const floorTotal = bandwidth.destination_floor_kbps * n;
    const overshootKbps = Math.max(0, floorTotal - bandwidth.absolute_total_kbps);
    const perDest = Math.floor(bandwidth.absolute_total_kbps / n);
    const share = Math.max(bandwidth.destination_floor_kbps, perDest);
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
    // Real branch: only the backend's own reported overshoot (via the
    // `bandwidth-overshoot` event) drives the banner. Mock branch: the local
    // toy allocator above.
    const overshootKbps = window.__TAURI__ ? realOvershootKbps || 0 : alloc.overshootKbps;
    if (bandwidth.bandwidth_mode === "absolute" && overshootKbps > 0) {
      grid.appendChild(
        warningBanner(
          window.__TAURI__
            ? `Over budget by ${overshootKbps} kbps: the guaranteed floors don't fit the maximum. No stream is starved below its floor.`
            : `Over budget by ${overshootKbps} kbps: the guaranteed floors need ${
                bandwidth.destination_floor_kbps * alloc.n
              } kbps but the maximum is ${bandwidth.absolute_total_kbps} kbps. No stream is starved below its floor.`
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
    top.appendChild(bandwidthModeToggle(bandwidth.bandwidth_mode, (mode) => setBandwidthMode(mode)));

    const liveCount = mockDestinations.filter((d) => d.kind !== "audio").length;
    const roomsFed = new Set(mockDestinations.map((d) => d.room_handle)).size;
    const summaryParts = [`${liveCount} destination${liveCount === 1 ? "" : "s"}`];
    if (roomsFed > 1) summaryParts.push(`streaming to ${roomsFed} rooms — about ${roomsFed}x the outgoing bandwidth of one stream`);
    top.appendChild(el("span", { className: "sc-bandwidth-summary", text: summaryParts.join(" · ") }));

    const controls = el("div", { className: "sc-bandwidth-controls" });
    if (bandwidth.bandwidth_mode === "absolute") {
      controls.appendChild(bandwidthField("Total maximum", bandwidth.absolute_total_kbps, 500, 40000, (v) => setAbsoluteTotal(v)));
      controls.appendChild(bandwidthField("Floor per stream", bandwidth.destination_floor_kbps, 100, 20000, (v) => setAbsoluteFloor(v)));
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
        const roomsAlreadyFedByCapture = mockDestinations.filter((d) => d.capture_id === dest.capture_id).map((d) => d.room_handle);
        const withShare =
          bandwidth.bandwidth_mode === "absolute" && dest.kind !== "audio" && dest.state !== "starting"
            ? Object.assign({}, dest, { computedShareKbps: alloc.share })
            : dest;
        gridEl.appendChild(
          destinationCard(
            withShare,
            {
              onEditApply: (id, patch) => applyDestinationEdit(id, patch),
              onTransfer: (id, targetRoomHandle) => transferDestination(id, targetRoomHandle),
              onFanout: (captureId, targetRoomHandle) => fanoutDestination(captureId, targetRoomHandle),
              onStop: (id) => stopDestination(id),
            },
            {
              roomOptions: mockJoinedRooms,
              perStreamBitrateEditable: bandwidth.bandwidth_mode === "per_stream" && dest.kind !== "audio",
              roomsAlreadyFedByCapture,
            }
          )
        );
      });
      body = gridEl;
    }
    return card("ACTIVE SHARES", [body], { className: "sc-active-shares-card" });
  }

  function mockTransfer(sourceId, targetRoomHandle) {
    const source = mockDestinations.find((d) => d.destination_id === sourceId);
    if (!source) return;
    const targetRoom = mockJoinedRooms.find((r) => r.id === targetRoomHandle);
    source.pendingRoleText = `Transferring to ${targetRoom ? targetRoom.name : targetRoomHandle}…`;

    const newDest = Object.assign({}, source, {
      destination_id: destIdSeq++,
      room_handle: targetRoomHandle,
      room_label: targetRoom ? targetRoom.name : String(targetRoomHandle),
      state: "starting",
      pendingRoleText: `Transferring from ${source.room_label}…`,
    });
    mockDestinations.push(newDest);
    render();

    // Mock "make-before-break": once the new branch's first IDR would land,
    // drop the old destination and clear the new one's pending label.
    setTimeout(() => {
      newDest.state = "live";
      newDest.pendingRoleText = null;
      mockDestinations = mockDestinations.filter((d) => d.destination_id !== sourceId);
      render();
    }, 1200);
  }

  function mockFanout(sourceCaptureId, targetRoomHandle) {
    const source = mockDestinations.find((d) => d.capture_id === sourceCaptureId);
    if (!source) return;
    const targetRoom = mockJoinedRooms.find((r) => r.id === targetRoomHandle);
    const newDest = Object.assign({}, source, {
      destination_id: destIdSeq++,
      room_handle: targetRoomHandle,
      room_label: targetRoom ? targetRoom.name : String(targetRoomHandle),
      state: "starting",
      pendingRoleText: "Also sending here…",
    });
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
    roomSelect.value = state.startForm.roomHandle;
    roomSelect.addEventListener("change", () => (state.startForm.roomHandle = Number(roomSelect.value)));
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
    if (bandwidth.bandwidth_mode === "per_stream") {
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
        startVideoShare(state.startForm.roomHandle, state.startForm.fps, state.startForm.bitrateKbps)
          .catch((e) => {
            state.startError = String(e);
          })
          .finally(() => {
            state.startInFlight = false;
            render();
          });
      })
    );

    if (state.startError) form.appendChild(el("div", { className: "sc-start-error", text: state.startError }));
    if (state.startInFlight) form.appendChild(el("div", { className: "sc-start-progress", text: "Waiting for screen-share permission…" }));

    return card("START A STREAM", [form], { className: "sc-start-stream-card" });
  }

  function mockStartStream(roomHandle, fps, bitrateKbps) {
    const room = mockJoinedRooms.find((r) => r.id === roomHandle);
    const newDest = {
      destination_id: destIdSeq,
      capture_id: destIdSeq++,
      room_handle: roomHandle,
      room_label: room ? room.name : String(roomHandle),
      kind: "video",
      state: "starting",
      width: 1920,
      height: 1080,
      framerate: fps,
      bitrate_kbps: bitrateKbps,
      pending: null,
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
    select.value = state.scopedRoomHandle;
    select.addEventListener("change", async () => {
      state.scopedRoomHandle = Number(select.value);
      const room = mockJoinedRooms.find((r) => r.id === state.scopedRoomHandle);
      state.roomName = room ? room.name : "";
      state.savedHint = false;
      mockMembersByRoom[state.scopedRoomHandle] = await fetchRoomMembers(state.scopedRoomHandle);
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
    const members = mockMembersByRoom[state.scopedRoomHandle] || [];
    const body = [];
    if (members.length === 0) {
      body.push(el("div", { className: "sc-member-row-conn", text: "No other members yet." }));
    } else {
      members.forEach((m) => {
        const row = el("div", { className: "sc-member-row" });
        row.appendChild(el("span", { text: m.display_name }));
        row.appendChild(el("span", { className: "sc-member-row-conn", text: "conn_id " + m.conn_id.toString(16).padStart(16, "0") }));
        body.push(row);
      });
    }

    const scopedRoom = mockJoinedRooms.find((r) => r.id === state.scopedRoomHandle);
    const renameRow = el("div", { className: "sc-rename-row" });
    const nameInput = el("input", { attrs: { type: "text", value: state.roomName } });
    renameRow.appendChild(nameInput);
    renameRow.appendChild(
      secondaryButton("Save", () => renameRoom(scopedRoom ? scopedRoom.join_string : "", nameInput.value.trim() || state.roomName))
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
        if (!mockAudioSources.some((s) => s.id === state.audioSource.id)) state.audioSource = mockAudioSources[0] || state.audioSource;
        render();
      })
    );
    const select = el("select");
    mockAudioSources.forEach((s) => select.appendChild(el("option", { text: s.name, attrs: { value: s.id } })));
    select.value = state.audioSource.id;
    select.addEventListener("change", () => {
      state.audioSource = mockAudioSources.find((s) => String(s.id) === select.value) || state.audioSource;
    });
    row.appendChild(select);
    row.appendChild(primaryButton("Share Audio", () => startAudioShare(state.scopedRoomHandle, state.audioSource)));
    return card("SHARE AUDIO", [row], { className: "sc-share-audio-card" });
  }

  // ---------------------------------------------------------------------
  // Init — fetches every data touchpoint (mock branch by default in a plain
  // browser), then does the first render. Also the "refresh" this tab
  // exposes on `window.StreamControl` for `shell.js`'s event listeners to
  // call after a `presence-updated`/`destination-state-changed` event.
  // ---------------------------------------------------------------------

  async function init() {
    mockJoinedRooms = await fetchJoinedRooms();
    mockDestinations = await fetchActiveDestinations();
    bandwidth = await fetchBandwidthSettings();
    mockAudioSources = await fetchAudioSources();

    if (mockJoinedRooms.length > 0 && !mockJoinedRooms.some((r) => r.id === state.scopedRoomHandle)) {
      state.scopedRoomHandle = mockJoinedRooms[0].id;
      state.roomName = mockJoinedRooms[0].name;
      state.startForm.roomHandle = mockJoinedRooms[0].id;
    }
    if (mockAudioSources.length > 0 && !mockAudioSources.some((s) => s.id === state.audioSource.id)) state.audioSource = mockAudioSources[0];
    mockMembersByRoom[state.scopedRoomHandle] = await fetchRoomMembers(state.scopedRoomHandle);

    render();
  }

  function setOvershoot(kbps) {
    realOvershootKbps = kbps;
    render();
  }

  window.StreamControl = { refresh: init, setOvershoot };

  document.addEventListener("DOMContentLoaded", init);
})();
