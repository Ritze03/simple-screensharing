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
//   stop_destination, retry_destination, transfer_destination,
//   start_destination, start_video_share, set_bitrate, set_framerate,
//   start_audio_share, rename_room.
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

  const { el, icon, card, warningBanner, bandwidthModeToggle, destinationCard, secondaryButton, primaryButton, renderEmptyState, emptyRow } = window.Components;

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
    // Demo-only failed destination — see this file's mock/real fallback
    // header note; `errorText` here stands in for the raw error string a
    // real `DestinationState::Failed(String)` carries, since `DestinationRow`
    // (dto.rs) doesn't surface it yet (frontend-only pass, no backend touch).
    {
      destination_id: 4,
      capture_id: 4,
      room_handle: 2,
      room_label: "Gaming Night",
      kind: "audio",
      state: "failed",
      width: 0,
      height: 0,
      framerate: 0,
      bitrate_kbps: 128,
      pending: null,
      errorText: "encoder init failed: no matching PipeWire node for source id 42",
    },
  ];

  // Default mode here is deliberately "absolute" (production default is
  // per_stream per stream-control.md) so the overshoot warning banner is
  // visible on first load without an extra click — a demoability deviation,
  // documented in APP-SHELL.md's doc update. Field names mirror
  // `dto::SettingsDto`'s bandwidth-related fields exactly.
  const MOCK_BANDWIDTH = { bandwidth_mode: "absolute", absolute_total_kbps: 8000, destination_floor_kbps: 3000 };

  const MOCK_MEMBERS_BY_ROOM = {
    // Kept within `Number.MAX_SAFE_INTEGER` for readability — plain JS
    // numbers here, unlike the real (Tauri) path. `conn_id`/`owner` used to
    // be a genuine wire-format limitation ("a real `conn_id` is a `u64` and
    // can exceed that... not something fixable here"): every u64 crossing
    // the JSON boundary as a bare number silently loses precision above
    // 2^53. That's now fixed on the real path — `dto::hex_u64` (Tauri side)
    // sends `conn_id`/`owner` as lowercase-hex strings instead, see its doc
    // comment for the real bug this caused (a watched stream's decoder never
    // receiving frames). The mock branch never crosses that boundary at all,
    // so it's left as plain numbers here — simpler for these hand-picked
    // demo values, and there's nothing real to lose precision on.
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

  // Every setter below updates the local `bandwidth` object on *both*
  // branches, not just the mock one. `updateBandwidthCard()`'s reconciliation
  // skips rebuilding the card when `JSON.stringify({ bandwidth, ... })`
  // matches its last-rendered signature — correct for events that don't
  // touch bandwidth, but if the real branch left `bandwidth` untouched after
  // a *successful* invoke, the post-invoke `render()` call below computes
  // the exact same signature as before the click and the toggle/sliders
  // visually never move, even though the backend's mode did change (only
  // fixed once some unrelated event happens to trigger a full `init()`
  // refetch — confirmed by relaunching the app and seeing the real,
  // persisted mode was correct all along). Setting the field directly here
  // (mirroring what the mock branch already did) keeps the two branches'
  // observable behavior identical, same as every other real/mock pair in
  // this file.
  async function setBandwidthMode(mode) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_bandwidth_mode", { mode });
    }
    bandwidth.bandwidth_mode = mode;
    render();
  }

  async function setAbsoluteTotal(kbps) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_bandwidth_total", { totalKbps: kbps });
    }
    bandwidth.absolute_total_kbps = kbps;
    render();
  }

  async function setAbsoluteFloor(kbps) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_bandwidth_floor", { floorKbps: kbps });
    }
    bandwidth.destination_floor_kbps = kbps;
    render();
  }

  async function applyDestinationEdit(destinationId, patch) {
    const dest = mockDestinations.find((d) => d.destination_id === destinationId);
    // `edit_destination` takes a full edit (scale/framerate/bitrate/tune/
    // quality_level/rate_control), not a sparse patch, but every control in
    // this panel (the bitrate slider, and the three tune/quality/rate-control
    // toggles below it) applies on its own change/click — so `patch` here is
    // only ever the one field that just changed. Merge it onto the tracked
    // edit buffer and send the *whole* buffer every time, same way the old
    // scale/framerate hardcoding already did for the fields this tab doesn't
    // expose a control for.
    const edit = dest ? getDestEdit(dest) : Object.assign({ bitrateKbps: 3000, tune: "low_latency", qualityLevel: "performance", rateControl: "constant", error: null }, patch);
    Object.assign(edit, patch);
    if (dest) destEditState.set(destinationId, edit);

    if (window.__TAURI__) {
      try {
        await window.__TAURI__.core.invoke("edit_destination", {
          destinationId,
          scalePercent: 100,
          framerate: dest ? dest.framerate : 30,
          bitrateKbps: edit.bitrateKbps,
          tune: edit.tune,
          qualityLevel: edit.qualityLevel,
          rateControl: edit.rateControl,
        });
        edit.error = null;
      } catch (e) {
        // Invalid enum strings can't happen from this UI (the toggles only
        // ever send the three known values each), but a `BranchSwap` can
        // still legitimately fail server-side (e.g. the new encoder config
        // rejected by the backend) — surface it on the card rather than
        // swallowing it, same pattern as the "Start a Stream" form's
        // `state.startError`.
        edit.error = String(e);
      }
      render();
      return;
    }
    if (dest) Object.assign(dest, { bitrate_kbps: edit.bitrateKbps });
    render();
  }

  async function stopDestination(destinationId) {
    // Drop this destination's client-side-tracked maps (stalled-since,
    // encoder-tuning edit buffer) right away rather than waiting for the
    // next `updateActiveSharesCard` pass to notice it's gone — that pass
    // only prunes `destCardEls`, not these two, so a destination stopped
    // while `stalled` (or with a customized tune/quality/rate-control edit)
    // would otherwise sit in `destStalledSince`/`destEditState` forever,
    // keyed by an id that will never come back (`destIdSeq` only grows).
    // Over a long session with many stop/retry cycles that's a slow,
    // unbounded leak — small, but avoidable.
    destStalledSince.delete(destinationId);
    destEditState.delete(destinationId);
    destSparkHistory.delete(destinationId);
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("stop_destination", { destinationId });
      return;
    }
    mockDestinations = mockDestinations.filter((x) => x.destination_id !== destinationId);
    render();
  }

  /** "Retry" action on a Failed destination card's `statusBadge` (see
   * `components.js`'s `destinationCard`). Backed by the dedicated
   * `retry_destination` command in `commands.rs`, which tears the dead
   * branch down and brings a fresh one back up in the same room off the
   * same capture, preserving that destination's own last-configured
   * bitrate/tune/quality_level/rate_control (see `AppState::
   * retry_destination`'s `Why:` note) — this used to be a plain
   * `stop_destination` + `start_destination` pair, which silently reset a
   * retried destination to default encoder settings and, for a capture's
   * last destination, could tear the whole capture down instead of just the
   * failed branch. Mock branch mirrors `mockStartStream`'s starting -> live
   * progression at toy timescales. */
  async function retryDestination(dest) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("retry_destination", { destinationId: dest.destination_id });
      return;
    }
    mockDestinations = mockDestinations.filter((x) => x.destination_id !== dest.destination_id);
    const newDest = Object.assign({}, dest, { destination_id: destIdSeq++, state: "starting" });
    mockDestinations.push(newDest);
    render();
    setTimeout(() => {
      newDest.state = "live";
      render();
    }, 800);
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
    }
    // Same fix as `setBandwidthMode`/`setLanguage` etc.: update the local
    // value on both branches, not just the mock one — `updateRoomScopedRow`'s
    // reconciliation signature includes `state.roomName`, and without this
    // the Room name field would snap back to the pre-rename value (rebuilt
    // fresh from the still-stale `state.roomName`) right as "Saved." appears,
    // even though the rename itself succeeded on the backend.
    state.roomName = name;
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
  // Render — targeted reconciliation, not `innerHTML = ""` + full rebuild.
  //
  // Root cause (see FRONTEND-SHELL.md's poll-driven-re-render click-loss
  // note): every `presence-updated`/`destination-state-changed` event from
  // `driver.rs`'s 250ms poll — already diffed per-room/per-destination on
  // the Rust side before it's ever emitted — called `render()`, which wiped
  // `#stream-control-root` and rebuilt *every* card and button from scratch
  // via fresh `addEventListener` calls, even when the event had nothing to
  // do with most of what got rebuilt (e.g. one destination's state flipping
  // tore down the Stop button on every *other* destination's card too, and
  // the bandwidth-mode toggle along with it). A click landing between the
  // old node's removal and the new node's attachment is silently dropped —
  // intermittently, since it's a timing race against the poll, not a hang
  // or crash. Below, each top-level card — and, for Active Shares, each
  // destination card individually — keeps its DOM node (and listeners)
  // across a `render()` call unless a cheap signature of the data it
  // actually displays changed; unchanged destination cards are *moved* into
  // place (a no-op if already there), never recreated.
  // ---------------------------------------------------------------------

  let gridEl = null;
  let bandwidthCardEl = null;
  let bandwidthCardSig = null;
  let overshootBannerEl = null;
  let overshootBannerSig = null;
  let activeSharesCardEl = null;
  let activeSharesGridEl = null;
  let activeSharesBodySig = null;
  const destCardEls = new Map(); // destination_id -> { el, sig }
  // destination_id -> ms epoch first observed 'stalled'. No DTO carries a
  // "since" timestamp for `DestinationState::Stalled` (see `statusBadge`'s
  // doc comment in components.js) — tracked client-side here instead, the
  // same way this file already recomputes `pendingRoleText` etc. locally.
  const destStalledSince = new Map();

  function getStalledSince(dest) {
    if (dest.state !== "stalled") {
      destStalledSince.delete(dest.destination_id);
      return null;
    }
    if (!destStalledSince.has(dest.destination_id)) destStalledSince.set(dest.destination_id, Date.now());
    return destStalledSince.get(dest.destination_id);
  }
  // destination_id -> { bitrateKbps, tune, qualityLevel, rateControl, error }.
  // Local edit buffer for `edit_destination`'s fields — write-only, same as
  // the rest of this panel: `DestinationRow` doesn't expose the server's
  // current tune/quality_level/rate_control, so there's nothing to read back
  // and reconcile against. Defaults mirror `capture::TuneMode`/`QualityLevel`/
  // `RateControlMode`'s `Default` impls (low_latency/performance/constant).
  const destEditState = new Map();
  // destination_id -> number[] (last SPARK_WINDOW `sent_fps` samples, oldest
  // first). No historical stats are fetched from anywhere — this is just a
  // small client-side rolling window built up tick-by-tick from
  // `updateStats()`'s already-flowing `stream-stats-updated` payload, purely
  // to feed the destination card's inline sparkline (`.dest-sparkline`,
  // `.spark-bar`). Same leak-avoidance rationale as `destStalledSince`/
  // `destEditState` above — cleared in `stopDestination`.
  const destSparkHistory = new Map();
  const SPARK_BARS = 6;

  function getDestEdit(dest) {
    let s = destEditState.get(dest.destination_id);
    if (!s) {
      s = { bitrateKbps: dest.bitrate_kbps, tune: "low_latency", qualityLevel: "performance", rateControl: "constant", error: null };
      destEditState.set(dest.destination_id, s);
    }
    return s;
  }
  let startCardEl = null;
  let startCardSig = null;
  let roomScopedEl = null;
  let roomScopedSig = null;

  /** Reconciles `parent`'s children to exactly `children` (falsy entries
   * dropped), reusing/moving existing nodes instead of recreating them —
   * `insertBefore`/`appendChild` on a node already in the right place is a
   * no-op, so an unchanged child never leaves the DOM (and never loses its
   * listeners or an in-flight click). */
  function syncChildren(parent, children) {
    const wanted = children.filter(Boolean);
    wanted.forEach((child, i) => {
      if (parent.children[i] !== child) parent.insertBefore(child, parent.children[i] || null);
    });
    while (parent.children.length > wanted.length) parent.removeChild(parent.lastChild);
  }

  function render() {
    const root = document.getElementById("stream-control-root");
    if (!root) return;
    if (!gridEl) {
      gridEl = el("div", { className: "sc-grid" });
      root.appendChild(gridEl);
    }

    const alloc = computeAllocation();
    // Real branch: only the backend's own reported overshoot (via the
    // `bandwidth-overshoot` event) drives the banner. Mock branch: the local
    // toy allocator above.
    const overshootKbps = window.__TAURI__ ? realOvershootKbps || 0 : alloc.overshootKbps;
    const showBanner = bandwidth.bandwidth_mode === "absolute" && overshootKbps > 0;
    const bannerText = showBanner
      ? window.__TAURI__
        ? `Over budget by ${overshootKbps} kbps: the guaranteed floors don't fit the maximum. No stream is starved below its floor.`
        : `Over budget by ${overshootKbps} kbps: the guaranteed floors need ${
            bandwidth.destination_floor_kbps * alloc.n
          } kbps but the maximum is ${bandwidth.absolute_total_kbps} kbps. No stream is starved below its floor.`
      : null;

    syncChildren(gridEl, [
      updateBandwidthCard(),
      bannerText ? updateOvershootBanner(bannerText) : null,
      updateActiveSharesCard(alloc),
      updateStartStreamCard(),
      updateRoomScopedRow(),
    ]);
  }

  // ---------------------------------------------------------------------
  // Bandwidth card
  // ---------------------------------------------------------------------

  function updateBandwidthCard() {
    const liveCount = mockDestinations.filter((d) => d.kind !== "audio").length;
    const roomsFed = new Set(mockDestinations.map((d) => d.room_handle)).size;
    const sig = JSON.stringify({ bandwidth, liveCount, roomsFed });
    if (bandwidthCardEl && bandwidthCardSig === sig) return bandwidthCardEl;
    bandwidthCardSig = sig;
    bandwidthCardEl = renderBandwidthCard(liveCount, roomsFed);
    return bandwidthCardEl;
  }

  function updateOvershootBanner(text) {
    if (overshootBannerEl && overshootBannerSig === text) return overshootBannerEl;
    overshootBannerSig = text;
    overshootBannerEl = warningBanner(text);
    return overshootBannerEl;
  }

  function renderBandwidthCard(liveCount, roomsFed) {
    const top = el("div", { className: "sc-bandwidth-top" });
    top.appendChild(bandwidthModeToggle(bandwidth.bandwidth_mode, (mode) => setBandwidthMode(mode)));

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
  // Active shares card — keyed per-destination reconciliation: a
  // destination's card DOM node (Stop button included) is only rebuilt when
  // its own signature changes, and is *moved* rather than recreated when
  // only its position in the list needs to shift.
  // ---------------------------------------------------------------------

  // Ghost rows ("Share another screen" / "Share audio") — built once and
  // cached (not rebuilt per-render, unlike everything else in this card)
  // so the audio row's own expand/collapse toggle survives across
  // `updateActiveSharesCard` calls without needing to thread its state
  // through a JSON signature. Moved (never recreated) into whichever card
  // element (`empty`/`list` variant) is currently live, same `appendChild`-
  // is-a-no-op-when-already-in-place trick `syncChildren` relies on
  // elsewhere in this file.
  let activeSharesGhostEl = null;

  function buildGhostRows() {
    if (activeSharesGhostEl) return activeSharesGhostEl;
    const wrap = el("div", { className: "sc-ghost-rows" });

    // "Share another screen" — the real entry point is the "Start a
    // Stream" card below; this just scrolls/focuses it rather than
    // duplicating the room/framerate/bitrate form and its start logic.
    const screenRow = el("div", {
      className: "sc-ghost-row",
      attrs: { role: "button", tabindex: "0" },
      onClick: () => {
        if (!startCardEl) return;
        startCardEl.scrollIntoView({ behavior: "smooth", block: "center" });
        const firstControl = startCardEl.querySelector("select, input, button");
        if (firstControl) firstControl.focus();
      },
    });
    screenRow.appendChild(icon("cast", "sc-ghost-row-icon sc-ghost-row-icon-accent"));
    screenRow.appendChild(el("span", { text: "Share another screen" }));
    wrap.appendChild(screenRow);

    // "Share audio" — expands in place into the same `startAudioShare()`
    // call the room-scoped SHARE AUDIO card below already exposes (not a
    // second copy of that flow, just a second, more discoverable entry
    // point onto it).
    const audioExpand = el("div", { className: "sc-ghost-audio-expand", attrs: { hidden: "" } });
    const roomSelect = el("select", { className: "sc-ghost-audio-select" });
    const sourceSelect = el("select", { className: "sc-ghost-audio-select" });
    audioExpand.appendChild(roomSelect);
    audioExpand.appendChild(sourceSelect);
    audioExpand.appendChild(
      primaryButton("Share Audio", () => {
        const roomHandle = Number(roomSelect.value);
        const source = mockAudioSources.find((s) => String(s.id) === sourceSelect.value) || state.audioSource;
        startAudioShare(roomHandle, source);
      })
    );

    const audioRow = el("div", {
      className: "sc-ghost-row",
      attrs: { role: "button", tabindex: "0" },
      onClick: () => {
        const opening = audioExpand.hidden;
        audioExpand.hidden = !opening;
        if (!opening) return;
        // Repopulate on open — rooms/sources may have changed since the
        // last time this was expanded.
        roomSelect.innerHTML = "";
        mockJoinedRooms.forEach((r) => roomSelect.appendChild(el("option", { text: r.name, attrs: { value: r.id } })));
        roomSelect.value = state.scopedRoomHandle;
        sourceSelect.innerHTML = "";
        mockAudioSources.forEach((s) => sourceSelect.appendChild(el("option", { text: s.name, attrs: { value: s.id } })));
        sourceSelect.value = state.audioSource.id;
      },
    });
    audioRow.appendChild(icon("volume_up", "sc-ghost-row-icon sc-ghost-row-icon-muted"));
    audioRow.appendChild(el("span", { text: "Share audio" }));
    wrap.appendChild(audioRow);
    wrap.appendChild(audioExpand);

    activeSharesGhostEl = wrap;
    return wrap;
  }

  function updateActiveSharesCard(alloc) {
    if (mockDestinations.length === 0) {
      destCardEls.clear();
      if (!activeSharesCardEl || activeSharesBodySig !== "empty") {
        activeSharesCardEl = card(
          "ACTIVE SHARES",
          [
            renderEmptyState("Start sharing your screen or audio below to see it here.", null, null, {
              icon: "cast",
              headline: "No active shares yet",
            }),
          ],
          { className: "sc-active-shares-card" }
        );
        activeSharesCardEl.appendChild(buildGhostRows());
        activeSharesBodySig = "empty";
        activeSharesGridEl = null;
      }
      return activeSharesCardEl;
    }

    if (!activeSharesCardEl || activeSharesBodySig === "empty") {
      activeSharesCardEl = card("ACTIVE SHARES", [], { className: "sc-active-shares-card" });
      activeSharesGridEl = el("div", { className: "sc-active-shares-grid" });
      activeSharesCardEl.appendChild(activeSharesGridEl);
      activeSharesCardEl.appendChild(buildGhostRows());
    }
    activeSharesBodySig = "list";

    const seenIds = new Set();
    const orderedCardEls = mockDestinations.map((dest) => {
      seenIds.add(dest.destination_id);
      const roomsAlreadyFedByCapture = mockDestinations.filter((d) => d.capture_id === dest.capture_id).map((d) => d.room_handle);
      const withShare =
        bandwidth.bandwidth_mode === "absolute" && dest.kind !== "audio" && dest.state !== "starting"
          ? Object.assign({}, dest, { computedShareKbps: alloc.share })
          : dest;
      const perStreamBitrateEditable = bandwidth.bandwidth_mode === "per_stream" && dest.kind !== "audio";
      const editState = perStreamBitrateEditable ? getDestEdit(dest) : null;
      const stalledSince = getStalledSince(dest);
      const sig = JSON.stringify({ withShare, perStreamBitrateEditable, editState, roomOptions: mockJoinedRooms, roomsAlreadyFedByCapture, stalledSince });

      let entry = destCardEls.get(dest.destination_id);
      if (!entry || entry.sig !== sig) {
        // The outgoing card (if any) is about to be thrown away and rebuilt
        // from scratch — carry its Advanced-tuning `<details>` open/closed
        // state into the replacement so editing a control (including one
        // inside Advanced itself) doesn't visibly collapse the section the
        // user just opened. See `destinationCard`'s `advancedOpen` doc
        // comment in components.js.
        const priorAdvancedEl = entry ? entry.el.querySelector(".dest-advanced") : null;
        const advancedOpen = priorAdvancedEl ? priorAdvancedEl.open : false;
        entry = {
          el: destinationCard(
            withShare,
            {
              onEditApply: (id, patch) => applyDestinationEdit(id, patch),
              onTransfer: (id, targetRoomHandle) => transferDestination(id, targetRoomHandle),
              onFanout: (captureId, targetRoomHandle) => fanoutDestination(captureId, targetRoomHandle),
              onStop: (id) => stopDestination(id),
              onRetry: (d) => retryDestination(d),
            },
            { roomOptions: mockJoinedRooms, perStreamBitrateEditable, roomsAlreadyFedByCapture, editState, stalledSince, advancedOpen }
          ),
          sig,
        };
        destCardEls.set(dest.destination_id, entry);
      }
      return entry.el;
    });

    for (const id of Array.from(destCardEls.keys())) {
      if (!seenIds.has(id)) destCardEls.delete(id);
    }

    syncChildren(activeSharesGridEl, orderedCardEls);
    return activeSharesCardEl;
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

  function updateStartStreamCard() {
    const sig = JSON.stringify({
      mockJoinedRooms,
      bandwidthMode: bandwidth.bandwidth_mode,
      startForm: state.startForm,
      startInFlight: state.startInFlight,
      startError: state.startError,
    });
    if (startCardEl && startCardSig === sig) return startCardEl;
    startCardSig = sig;
    startCardEl = renderStartStreamCard();
    return startCardEl;
  }

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
    // 90/120/144/165/240 match real monitor refresh rates; 280 exceeds both
    // real monitor Hz and the project's documented 240Hz spec ceiling (see
    // capture::MAX_FRAMERATE's doc comment) — offered anyway per explicit
    // user request to experiment past that ceiling. MJPEG-over-HTTP delivery
    // bottlenecks around 20-30fps regardless of capture framerate, so
    // nothing above that is expected to visibly improve *viewer* playback
    // yet; this only widens what capture itself can be asked to run at.
    [15, 30, 60, 90, 120, 144, 165, 240, 280].forEach((fps) => fpsSelect.appendChild(el("option", { text: fps + " fps", attrs: { value: String(fps) } })));
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

  function updateRoomScopedRow() {
    const sig = JSON.stringify({
      mockJoinedRooms,
      scopedRoomHandle: state.scopedRoomHandle,
      roomName: state.roomName,
      savedHint: state.savedHint,
      members: mockMembersByRoom[state.scopedRoomHandle],
      mockAudioSources,
      audioSourceId: state.audioSource.id,
    });
    if (roomScopedEl && roomScopedSig === sig) return roomScopedEl;
    roomScopedSig = sig;
    roomScopedEl = renderRoomScopedRow();
    return roomScopedEl;
  }

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
      body.push(emptyRow("person_off", "No other members yet."));
    } else {
      members.forEach((m) => {
        const row = el("div", { className: "sc-member-row" });
        row.appendChild(el("span", { text: m.display_name }));
        // `m.conn_id` is a real (Tauri) `ConnId`: already a lowercase-hex
        // string (see `dto::hex_u64`'s doc comment — a bare `u64` loses
        // precision over JSON) — `.toString(16)` is then a no-op identity
        // call (`String.prototype.toString` ignores its radix argument), so
        // this line still works unchanged for the mock branch's plain-number
        // `conn_id` values too, no branching needed.
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

  // `stream-stats-updated` fires unconditionally every ~250ms (see
  // `shell.js`'s `wireEvents`), so this deliberately bypasses the normal
  // sig-diffed `render()` path: it writes straight into each already-
  // rendered destination card's `.dest-fps-readout` node via `textContent`,
  // keyed off `destCardEls` (the same destination_id -> {el, sig} map
  // `updateActiveSharesCard` maintains). A destination in the payload with
  // no rendered card yet (a startup timing edge case) is skipped rather than
  // erroring; a rendered card whose destination stopped showing up in the
  // payload (it just stopped) is simply left with its last value — the
  // card itself gets torn down separately by `destination-state-changed`.
  function updateStats(payload) {
    const destinations = (payload && payload.destinations) || [];
    destinations.forEach((d) => {
      const entry = destCardEls.get(d.destination_id);
      if (!entry) return;
      const readout = entry.el.querySelector(".dest-fps-readout");
      if (readout) {
        readout.hidden = false;
        readout.textContent = `Captured: ${Math.round(d.captured_fps)}fps · Sent: ${Math.round(d.sent_fps)}fps`;
      }

      // Sparkline: same direct-DOM-write approach as the readout above
      // (bypasses the sig-diffed render path so a stats tick never triggers
      // a full card rebuild) — push this tick's `sent_fps` onto the rolling
      // window, then rescale every bar's height against the window's own
      // max (not a fixed nominal fps ceiling, so both a 30fps and 240fps
      // stream's sparkline reads as a legible shape rather than one being
      // permanently pinned near zero).
      const spark = entry.el.querySelector(".dest-sparkline");
      if (!spark) return;
      let hist = destSparkHistory.get(d.destination_id);
      if (!hist) {
        hist = [];
        destSparkHistory.set(d.destination_id, hist);
      }
      hist.push(Number(d.sent_fps) || 0);
      if (hist.length > SPARK_BARS) hist.shift();

      const max = Math.max(1, ...hist);
      const bars = spark.querySelectorAll(".spark-bar");
      const padCount = SPARK_BARS - hist.length;
      bars.forEach((bar, i) => {
        const histIdx = i - padCount;
        if (histIdx < 0) {
          bar.classList.add("spark-bar-empty");
          bar.style.height = "";
          return;
        }
        bar.classList.remove("spark-bar-empty");
        bar.style.height = Math.max(8, Math.round((hist[histIdx] / max) * 100)) + "%";
      });
      spark.hidden = false;
    });
  }

  window.StreamControl = { refresh: init, setOvershoot, updateStats };

  document.addEventListener("DOMContentLoaded", init);
})();
