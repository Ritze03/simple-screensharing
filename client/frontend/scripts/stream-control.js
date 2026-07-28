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
//   list_saved_rooms, list_active_streams, list_staged_captures, get_settings,
//   room_detail, list_audio_sources, select_audio_source, set_bandwidth_mode,
//   set_bandwidth_total, set_bandwidth_floor, edit_destination,
//   stop_destination, stop_share, retry_destination, transfer_destination,
//   start_destination, stage_video_share, go_live, set_bitrate, set_framerate,
//   set_tune, set_quality_level, set_rate_control, start_audio_share,
//   rename_room.
// Notably `stage_video_share`/`start_audio_share` take no encoder
// arguments — framerate/bitrate/tune/quality-level/rate-control/audio-source
// are process-wide settings applied via `set_framerate`/`set_bitrate`/
// `set_tune`/`set_quality_level`/`set_rate_control`/`select_audio_source`
// *before* the share starts, not per-call arguments. `go_live` (the "Go
// live" flow's second half, see `superdoc/features/stream-control.md`'s
// 2026-07-28 note) only takes `captureId`/`roomHandle` for the same reason.
//
// Layout, top to bottom (the redesign this fixes vs. the old egui tab, see
// stream-control.css's header comment):
//   1. Bandwidth — compact summary strip, mode toggle, overshoot banner.
//   2. Active shares — the prominent, equal-width destination grid; staged
//      ("Share Screen"/"Share another screen", not yet "Go live"'d) cards
//      render first, ahead of live ones.
//   3. Room-scoped: Members/rename + Share audio, for one selected room.

(function () {
  "use strict";

  const {
    el,
    icon,
    card,
    warningBanner,
    bandwidthModeToggle,
    destinationCard,
    secondaryButton,
    renderEmptyState,
    emptyRow,
    dropdown,
    attachWheelStep,
  } = window.Components;

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
  // Staged captures — the "Go live" flow's pre-"Go live" cards (`dto::
  // StagedCaptureRow`, see `superdoc/features/stream-control.md`'s
  // 2026-07-28 note). Empty by default: unlike `MOCK_DESTINATIONS`, there's
  // nothing to demo here until "Share Screen"/"Share another screen" is
  // actually clicked.
  let mockStagedCaptures = [];
  let bandwidth = Object.assign({}, MOCK_BANDWIDTH);
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

  /** Staged captures (see `dto::StagedCaptureRow`) — the "Go live" flow's
   * pre-"Go live" cards. */
  async function fetchStagedCaptures() {
    if (window.__TAURI__) {
      return await window.__TAURI__.core.invoke("list_staged_captures");
    }
    return mockStagedCaptures.slice();
  }

  async function fetchBandwidthSettings() {
    if (window.__TAURI__) {
      const settings = await window.__TAURI__.core.invoke("get_settings");
      return { bandwidth_mode: settings.bandwidth_mode, absolute_total_kbps: settings.absolute_total_kbps, destination_floor_kbps: settings.destination_floor_kbps };
    }
    return Object.assign({}, MOCK_BANDWIDTH);
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
    // this panel (preset, framerate, the bitrate slider, and the three
    // tune/quality/rate-control toggles) applies on its own change/click — so
    // `patch` here is only ever the field(s) that just changed. Merge it onto
    // the tracked edit buffer and send the *whole* buffer every time, same way
    // the old scale hardcoding already did for the one field this tab still
    // doesn't expose a control for.
    const edit = dest
      ? getDestEdit(dest)
      : Object.assign(
          { bitrateKbps: 3000, framerate: 30, tune: "best_quality", qualityLevel: "balanced", rateControl: "constant", error: null },
          patch
        );
    Object.assign(edit, patch);
    if (dest) destEditState.set(destinationId, edit);

    // `Why the bitrate sent depends on the bandwidth mode:` in Per-Stream mode
    // the buffer's value *is* the user's choice and must be sent. In Absolute
    // mode there is no Bitrate slider (the allocator owns it), so the buffer's
    // copy is whatever the bitrate happened to be when this card first
    // rendered — stale by now, and sending it would briefly stomp the
    // allocator's live value every time the user touched an unrelated control
    // like the preset. Sending the destination's currently-reported bitrate
    // instead keeps such an edit a genuine no-op for this field.
    const bitrateKbps = perStreamBitrateMode() || !dest ? edit.bitrateKbps : dest.bitrate_kbps;

    if (window.__TAURI__) {
      try {
        await window.__TAURI__.core.invoke("edit_destination", {
          destinationId,
          scalePercent: 100,
          framerate: edit.framerate,
          bitrateKbps,
          tune: edit.tune,
          qualityLevel: edit.qualityLevel,
          rateControl: edit.rateControl,
        });
        edit.error = null;
        // Mirror what was just applied onto the local DTO copy so the card's
        // Bitrate/Framerate *readouts* agree with its controls. Nothing else
        // will: a tier-1 bitrate write never changes the destination's state
        // label, so `destination-state-changed` never fires and
        // `mockDestinations` is never re-fetched (see `destEditState`'s note).
        if (dest) {
          dest.bitrate_kbps = bitrateKbps;
          dest.framerate = edit.framerate;
        }
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
    if (dest) Object.assign(dest, { bitrate_kbps: bitrateKbps, framerate: edit.framerate });
    render();
  }

  /** Whether the Bitrate slider is the user's to set — i.e. whether the
   * bandwidth allocator is out of the picture. See `destinationCard`'s
   * Bitrate row for why Absolute mode has no manual bitrate. */
  function perStreamBitrateMode() {
    return bandwidth.bandwidth_mode === "per_stream";
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
   * failed branch. Mock branch mirrors `mockGoLive`'s starting -> live
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

  /** "Share Screen"/"Share another screen" clicked: start the staged half of
   * the "Go live" flow — a real capture with zero destinations (see
   * `session::AppState::stage_video_share`'s doc comment). Returns the new
   * capture's id; the caller (the ghost row's `onClick`, below) just
   * triggers a refresh, since the driver's `staged-capture-changed` event
   * (or, in the mock branch, this function's own direct mutation) is what
   * actually gets the new card rendered. */
  async function stageVideoShare() {
    if (window.__TAURI__) {
      // Push the persisted "last used" values into the process-wide encoder
      // settings *before* staging — `stage_video_share` (like the
      // `start_video_share` it replaces, see this file's header note) reads
      // those, not any argument of its own, and the capture's framerate in
      // particular is fixed at pipeline-build time inside it. Without this,
      // a staged card would open running whatever `AppState::framerate`/
      // `bitrate_kbps`/etc last happened to be (its hardcoded first-run
      // fallback on a fresh launch) instead of what "Go live" last actually
      // used.
      const defaults = await fetchStartStreamDefaults();
      if (defaults) {
        await window.__TAURI__.core.invoke("set_framerate", { fps: defaults.fps });
        await window.__TAURI__.core.invoke("set_bitrate", { kbps: defaults.bitrateKbps });
        await window.__TAURI__.core.invoke("set_tune", { tune: defaults.tune });
        await window.__TAURI__.core.invoke("set_quality_level", { qualityLevel: defaults.qualityLevel });
        await window.__TAURI__.core.invoke("set_rate_control", { rateControl: defaults.rateControl });
      }
      await window.__TAURI__.core.invoke("stage_video_share");
      mockStagedCaptures = await fetchStagedCaptures();
      render();
      return;
    }
    mockStageVideoShare();
  }

  /** The staged card's persisted last-used encoder values (`rooms::
   * AppSettings::start_stream_defaults`) — seeds a *new* staged card's edit
   * buffer (`getStagedEdit`) so it opens showing the same settings "Go live"
   * last actually used, instead of a hardcoded fallback. */
  async function fetchStartStreamDefaults() {
    if (window.__TAURI__) {
      const d = await window.__TAURI__.core.invoke("get_start_stream_defaults");
      return { fps: d.framerate, bitrateKbps: d.bitrate_kbps, tune: d.tune, qualityLevel: d.quality_level, rateControl: d.rate_control };
    }
    return null;
  }

  // captureId -> { bitrateKbps, framerate, tune, qualityLevel, rateControl,
  // roomHandle, error }. Local edit buffer for a staged card's controls, same
  // "caller keeps the state, card renders from the buffer" shape as
  // `destEditState` (see that map's own doc comment) — a staged capture has
  // no `DestinationRow` to read tune/quality/rate-control back from either,
  // and `roomHandle` (which room "Go live" will use) has nowhere else to
  // live at all until "Go live" is actually pressed.
  const stagedEditState = new Map();

  function getStagedEdit(staged) {
    let s = stagedEditState.get(staged.capture_id);
    if (!s) {
      s = {
        bitrateKbps: staged.bitrate_kbps,
        framerate: staged.framerate,
        tune: staged.tune,
        qualityLevel: staged.quality_level,
        rateControl: staged.rate_control,
        roomHandle: mockJoinedRooms.length > 0 ? mockJoinedRooms[0].id : null,
        error: null,
      };
      stagedEditState.set(staged.capture_id, s);
    }
    return s;
  }

  /** Preset/Framerate/Bitrate/Advanced-tuning changed on a staged card.
   * Mirrors `applyDestinationEdit`'s merge-buffer-then-send shape, but writes
   * through the same process-wide `set_framerate`/`set_bitrate`/`set_tune`/
   * `set_quality_level`/`set_rate_control` commands `stage_video_share`'s
   * predecessor `start_video_share` always used (see this file's header
   * note) — *not* `set_start_stream_defaults`: per the owner's explicit
   * request, persisting these as the new defaults happens only once, at
   * "Go live" (`goLive`, below), not on every keystroke while still staged. */
  async function applyStagedTuning(captureId, staged, patch) {
    const edit = getStagedEdit(staged);
    Object.assign(edit, patch);
    if (window.__TAURI__) {
      try {
        if ("framerate" in patch) await window.__TAURI__.core.invoke("set_framerate", { fps: edit.framerate });
        if ("bitrateKbps" in patch) await window.__TAURI__.core.invoke("set_bitrate", { kbps: edit.bitrateKbps });
        if ("tune" in patch) await window.__TAURI__.core.invoke("set_tune", { tune: edit.tune });
        if ("qualityLevel" in patch) await window.__TAURI__.core.invoke("set_quality_level", { qualityLevel: edit.qualityLevel });
        if ("rateControl" in patch) await window.__TAURI__.core.invoke("set_rate_control", { rateControl: edit.rateControl });
        edit.error = null;
      } catch (e) {
        edit.error = String(e);
      }
      render();
      return;
    }
    const row = mockStagedCaptures.find((c) => c.capture_id === captureId);
    if (row) Object.assign(row, { bitrate_kbps: edit.bitrateKbps, framerate: edit.framerate, tune: edit.tune, quality_level: edit.qualityLevel, rate_control: edit.rateControl });
    render();
  }

  /** Room picker changed on a staged card — purely local (which room "Go
   * live" will target), nothing to send until "Go live" itself. */
  function setStagedRoom(captureId, roomHandle) {
    const s = stagedEditState.get(captureId);
    if (s) s.roomHandle = roomHandle;
    render();
  }

  /** "Go live" clicked: attach the staged capture's first destination into
   * whichever room the card's Room picker has selected, then persist the
   * card's current encoder settings as the new "last used" defaults — see
   * `applyStagedTuning`'s doc note on why this is the one moment that save
   * happens, mirroring `AppState::go_live`'s own doc comment on the backend
   * side. */
  async function goLive(captureId, staged) {
    const edit = getStagedEdit(staged);
    if (edit.roomHandle == null) return;
    if (window.__TAURI__) {
      // Neither `go_live` nor `start_video_share` (its predecessor — see
      // this file's header note) returns a `Result`: a capture that fails
      // to attach shows up as a `failed` destination card once `list_
      // active_streams` next reflects it, same as any other destination
      // failure, rather than as a rejected promise here.
      await window.__TAURI__.core.invoke("go_live", { captureId, roomHandle: edit.roomHandle });
      try {
        await window.__TAURI__.core.invoke("set_start_stream_defaults", {
          framerate: edit.framerate,
          bitrateKbps: edit.bitrateKbps,
          tune: edit.tune,
          qualityLevel: edit.qualityLevel,
          rateControl: edit.rateControl,
        });
      } catch (e) {
        console.error("failed to save start-stream defaults", e);
      }
      stagedEditState.delete(captureId);
      mockStagedCaptures = await fetchStagedCaptures();
      mockDestinations = await fetchActiveDestinations();
      // Seed the edit buffer `getDestEdit` would otherwise lazily default to
      // "best_quality"/"balanced"/"constant" for the destination that just
      // appeared, so the live card's Preset/Advanced-tuning controls reflect
      // what was actually picked on the staged card, instead of the
      // hardcoded fallback — `DestinationRow` (the DTO `fetchActiveDestinations`
      // reads) has no tune/quality_level/rate_control fields, so this is the
      // only place the frontend can recover them from. `go_live` attaches
      // exactly one new destination per call (see `session::AppState::
      // go_live`), so matching on `capture_id` is unambiguous.
      for (const dest of mockDestinations) {
        if (dest.capture_id === captureId && dest.kind === "video") {
          destEditState.set(dest.destination_id, {
            bitrateKbps: edit.bitrateKbps,
            framerate: edit.framerate,
            tune: edit.tune,
            qualityLevel: edit.qualityLevel,
            rateControl: edit.rateControl,
            error: null,
          });
        }
      }
      render();
      return;
    }
    mockGoLive(captureId, staged, edit);
  }

  /** "Cancel" clicked on a staged card: tear the capture down before it ever
   * sent anything anywhere. Identical primitive to a live card's "Stop"
   * (`stop_share` — see `AppState::stage_video_share`'s "Why this can reuse
   * `stop_share`" doc note), just with nothing to detach on the room side. */
  async function cancelStagedCapture(captureId) {
    stagedEditState.delete(captureId);
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("stop_share", { captureId });
      mockStagedCaptures = await fetchStagedCaptures();
      render();
      return;
    }
    mockStagedCaptures = mockStagedCaptures.filter((c) => c.capture_id !== captureId);
    render();
  }

  /**
   * Start sharing `source`'s audio into `roomHandle`, attached to the screen
   * share `forCaptureId` — audio is no longer a stream of its own in this
   * UI, it's a row inside the share it belongs to, and the backend records
   * the pairing so the frontend knows which card to put it in (see
   * `session::LocalCapture::audio_for_capture`).
   */
  async function startAudioShare(roomHandle, source, forCaptureId) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("select_audio_source", { id: source.id, name: source.name });
      await window.__TAURI__.core.invoke("start_audio_share", { roomHandle, forCaptureId: forCaptureId ?? null });
      return;
    }
    const room = mockJoinedRooms.find((r) => r.id === roomHandle);
    mockDestinations.push({
      destination_id: destIdSeq++,
      capture_id: destIdSeq,
      room_handle: roomHandle,
      room_label: room ? room.name : String(roomHandle),
      kind: "audio",
      label: source.name,
      state: "live",
      width: 0,
      height: 0,
      framerate: 0,
      bitrate_kbps: 128,
      pending: null,
      paired_capture_id: forCaptureId ?? null,
      muted: false,
    });
    render();
  }

  /** Mute/unmute a running audio share in place (its card row's toggle). */
  async function setAudioShareMuted(captureId, muted) {
    if (window.__TAURI__) {
      await window.__TAURI__.core.invoke("set_audio_share_muted", { captureId, muted });
      // The DTO is the source of truth for the button's label; refetch so
      // the next render picks the new state up rather than trusting a local
      // guess that could disagree with the pipeline.
      mockDestinations = await fetchActiveDestinations();
    } else {
      const d = mockDestinations.find((x) => x.capture_id === captureId);
      if (d) d.muted = muted;
    }
    render();
  }

  /** Re-list the system's currently-audio-producing applications, for the
   * "+ Add audio" picker on a share's card. Returns the fresh list: this
   * reassigns `mockAudioSources`, so a caller holding the previous array
   * would otherwise keep showing the old (on first open: the mock
   * placeholder) sources — see `addAudioRow` in components.js. */
  async function refreshAudioSources() {
    mockAudioSources = await fetchAudioSources();
    return mockAudioSources;
  }

  let destIdSeq = 100;
  let stagedCaptureIdSeq = 1000;

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
  const stagedCardEls = new Map(); // capture_id -> { el, sig }
  // audio destination_id -> its `.dest-audio-meter` element, which lives
  // inside the *paired video* destination's card. `updateStats()` needs this
  // indirection because `stream-stats-updated` keys `audio_level` by the
  // audio destination, not the card it's drawn in.
  const audioMeterEls = new Map();
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
  // destination_id -> { bitrateKbps, framerate, tune, qualityLevel, rateControl, error }.
  // Local edit buffer for `edit_destination`'s fields, and — since the
  // destination card renders every control in the edit panel from it — the
  // single source of truth for what those controls show.
  //
  // `Why the buffer and not the `DestinationRow` DTO is what the panel
  // renders:` the DTO lags, and for some edits it never catches up at all. A
  // bitrate-only change is a tier-1 live encoder property write, so the
  // destination stays `live`, so `driver.rs` never emits
  // `destination-state-changed`, so `shell.js` never calls `refresh()` and
  // `mockDestinations` is never re-fetched. Rendering the slider from the DTO
  // therefore showed the pre-edit value indefinitely — the reported "changing
  // the bitrate just resets the slider back, but I can see the bitrate rising
  // on the server" bug. `DestinationRow` also has no tune/quality_level/
  // rate_control fields to read back at all, so those three were always going
  // to be caller-held; keeping bitrate and framerate in the same buffer makes
  // the whole panel consistent rather than half-optimistic.
  //
  // Defaults mirror `capture::TuneMode`/`QualityLevel`/`RateControlMode`'s
  // `Default` impls (best_quality/balanced/constant — a deliberate "good mix"
  // out of the box, not either speed/quality extreme; see those types' doc
  // comments).
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
      s = {
        bitrateKbps: dest.bitrate_kbps,
        framerate: dest.framerate,
        tune: "best_quality",
        qualityLevel: "balanced",
        rateControl: "constant",
        error: null,
      };
      destEditState.set(dest.destination_id, s);
    }
    return s;
  }

  // The framerates the destination card's Framerate picker offers — the same
  // nine values a staged card's Framerate picker offers (240/280 exceed real
  // monitor Hz and the project's documented 240Hz spec ceiling, offered
  // anyway per explicit user request to experiment past that ceiling), plus
  // this destination's own current rate if it somehow isn't one of them — a
  // stream started before
  // this list changed, or one whose rate was clamped server-side, must still
  // be shown truthfully rather than silently snapped to a neighbour.
  const FRAMERATE_OPTIONS = [15, 30, 60, 90, 120, 144, 165, 240, 280];
  function framerateOptionsFor(editState) {
    if (!editState) return FRAMERATE_OPTIONS;
    const current = Number(editState.framerate);
    if (!current || FRAMERATE_OPTIONS.includes(current)) return FRAMERATE_OPTIONS;
    return FRAMERATE_OPTIONS.concat([current]).sort((a, b) => a - b);
  }
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

    syncChildren(gridEl, [updateActiveSharesCard(alloc), updateBandwidthCard(), bannerText ? updateOvershootBanner(bannerText) : null]);
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
    attachWheelStep(slider);
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

    // "Share another screen" — the entry point into the "Go live" flow (see
    // `superdoc/features/stream-control.md`'s 2026-07-28 note): starts a
    // real, staged capture right away (portal prompt and all) and lets it
    // render as its own card in the grid below, rather than opening a
    // separate form. Nothing is sent to any room — no `AnnounceStream`, no
    // destination — until that staged card's own "Go live" button is
    // pressed.
    const screenRow = el("div", {
      className: "sc-ghost-row",
      attrs: { role: "button", tabindex: "0" },
      onClick: () => stageVideoShare(),
    });
    screenRow.appendChild(icon("cast", "sc-ghost-row-icon sc-ghost-row-icon-accent"));
    screenRow.appendChild(el("span", { text: "Share another screen" }));
    wrap.appendChild(screenRow);

    // `Why there is no "Share audio" ghost row:` it started a *standalone*
    // audio share, which no longer has anywhere to render — audio belongs to
    // a screen share now and is added from that share's own card ("+ Add
    // audio"). Leaving this entry point would have been the one way to
    // create an audio share with no card to live in.

    activeSharesGhostEl = wrap;
    return wrap;
  }

  function updateActiveSharesCard(alloc) {
    // Audio never renders a card of its own, so "is this list empty" is a
    // question about *screen shares* — an audio-only destination list would
    // otherwise show an empty grid instead of the empty state. Staged
    // captures (real, running, "Go live"-not-yet-pressed shares) count as
    // non-empty too — a staged card is exactly the kind of thing this grid
    // exists to show.
    if (mockDestinations.filter((d) => d.kind !== "audio").length === 0 && mockStagedCaptures.length === 0) {
      audioMeterEls.clear();
      destCardEls.clear();
      stagedCardEls.clear();
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

    // Staged cards render first, ahead of live ones — "Go live" hasn't
    // happened yet, so there's nothing to compare bandwidth/room chips
    // against; they're just the capture + its editable settings.
    const seenStagedIds = new Set();
    const stagedCardEl = (staged) => {
      seenStagedIds.add(staged.capture_id);
      const edit = getStagedEdit(staged);
      const sig = JSON.stringify({ staged, edit, roomOptions: mockJoinedRooms });
      let entry = stagedCardEls.get(staged.capture_id);
      if (!entry || entry.sig !== sig) {
        const priorAdvancedEl = entry ? entry.el.querySelector(".dest-advanced") : null;
        const advancedOpen = priorAdvancedEl ? priorAdvancedEl.open : false;
        entry = {
          el: destinationCard(
            staged,
            {
              onEditApply: (captureId, patch) => applyStagedTuning(captureId, staged, patch),
              onStagedRoomChange: (captureId, roomHandle) => setStagedRoom(captureId, roomHandle),
              onGoLive: (captureId) => goLive(captureId, staged),
              onCancel: (captureId) => cancelStagedCapture(captureId),
            },
            {
              staged: true,
              stagedRoomHandle: edit.roomHandle,
              roomOptions: mockJoinedRooms,
              encoderTuningEditable: true,
              perStreamBitrateEditable: perStreamBitrateMode(),
              framerateOptions: framerateOptionsFor(edit),
              editState: edit,
              advancedOpen,
            }
          ),
          sig,
        };
        stagedCardEls.set(staged.capture_id, entry);
      }
      return entry.el;
    };
    const stagedCardElsOrdered = mockStagedCaptures.map(stagedCardEl);
    for (const id of Array.from(stagedCardEls.keys())) {
      if (!seenStagedIds.has(id)) stagedCardEls.delete(id);
    }

    const seenIds = new Set();
    audioMeterEls.clear();
    // Audio destinations are not cards. Each one is rendered as a row inside
    // the screen share's card it was attached to — matched on the pairing the
    // backend recorded (`paired_capture_id`, see
    // `session::LocalCapture::audio_for_capture`) *and* the room, since one
    // capture can be fanned out to several rooms and each room's audio is its
    // own share. An audio row with no matching video card left (only
    // reachable if a share was torn down out from under it, or an audio share
    // was started by the old egui frontend) simply doesn't render here; the
    // sidebar's OUTGOING list still lists and can stop it, so it is never
    // invisible or stranded.
    const videoDestinations = mockDestinations.filter((d) => d.kind !== "audio");
    const audioFor = (videoDest) =>
      mockDestinations.find(
        (d) => d.kind === "audio" && d.paired_capture_id === videoDest.capture_id && d.room_handle === videoDest.room_handle
      ) || null;

    const orderedCardEls = videoDestinations.map((dest) => {
      seenIds.add(dest.destination_id);
      const audio = audioFor(dest);
      const roomsAlreadyFedByCapture = videoDestinations.filter((d) => d.capture_id === dest.capture_id).map((d) => d.room_handle);
      const withShare =
        bandwidth.bandwidth_mode === "absolute" && dest.kind !== "audio" && dest.state !== "starting"
          ? Object.assign({}, dest, { computedShareKbps: alloc.share })
          : dest;
      // Two separate questions, deliberately two separate flags — see
      // `destinationCard`'s `opts` doc comment. The encoder tuning panel
      // (Preset / Framerate / Advanced) is meaningful in *both* bandwidth
      // modes: none of those fields is the allocator's business. Only the
      // Bitrate slider inside it is Per-Stream-only, because bitrate is
      // exactly what the Absolute-mode allocator owns.
      const encoderTuningEditable = dest.kind !== "audio";
      const perStreamBitrateEditable = perStreamBitrateMode() && encoderTuningEditable;
      const editState = encoderTuningEditable ? getDestEdit(dest) : null;
      const stalledSince = getStalledSince(dest);
      const sig = JSON.stringify({
        withShare,
        encoderTuningEditable,
        perStreamBitrateEditable,
        editState,
        roomOptions: mockJoinedRooms,
        roomsAlreadyFedByCapture,
        stalledSince,
        // Only the fields the audio row actually renders — deliberately NOT
        // the whole audio row object, and emphatically not its level: the VU
        // meter is animated by direct DOM writes from `updateStats()`, so
        // folding a 10Hz level into this signature would rebuild the card
        // (and every listener on it) ten times a second.
        audio: audio ? { id: audio.destination_id, capture: audio.capture_id, label: audio.label, muted: audio.muted } : null,
      });

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
              onAddAudio: (d, source) => startAudioShare(d.room_handle, source, d.capture_id),
              onAudioMute: (captureId, muted) => setAudioShareMuted(captureId, muted),
              onAudioStop: (audioDestinationId) => stopDestination(audioDestinationId),
              onRefreshAudioSources: () => refreshAudioSources(),
            },
            {
              roomOptions: mockJoinedRooms,
              encoderTuningEditable,
              perStreamBitrateEditable,
              framerateOptions: framerateOptionsFor(editState),
              roomsAlreadyFedByCapture,
              editState,
              stalledSince,
              advancedOpen,
              audio,
              audioSources: mockAudioSources,
            }
          ),
          sig,
        };
        destCardEls.set(dest.destination_id, entry);
      }
      // Index this card's VU meter by the *audio* destination id, because
      // that's the key `stream-stats-updated` reports `audio_level` under
      // even though the meter lives inside the video destination's card.
      if (audio) {
        const meter = entry.el.querySelector(`.dest-audio-meter[data-audio-destination-id="${audio.destination_id}"]`);
        if (meter) audioMeterEls.set(audio.destination_id, meter);
      }
      return entry.el;
    });

    for (const id of Array.from(destCardEls.keys())) {
      if (!seenIds.has(id)) destCardEls.delete(id);
    }

    syncChildren(activeSharesGridEl, stagedCardElsOrdered.concat(orderedCardEls));
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
  // Staged capture flow (mock) — "Share Screen"/"Share another screen"'s
  // portal-prompt-then-`ready` progression, and "Go live" turning a staged
  // card into a real destination. See `superdoc/features/stream-
  // control.md`'s 2026-07-28 note.
  // ---------------------------------------------------------------------

  function mockStageVideoShare() {
    const captureId = stagedCaptureIdSeq++;
    mockStagedCaptures.push({
      capture_id: captureId,
      state: "starting",
      width: 0,
      height: 0,
      framerate: 30,
      bitrate_kbps: 8000,
      tune: "best_quality",
      quality_level: "balanced",
      rate_control: "constant",
    });
    render();
    // Mimics the real portal-prompt-then-pipeline-up delay at toy timescale.
    setTimeout(() => {
      const row = mockStagedCaptures.find((c) => c.capture_id === captureId);
      if (!row) return;
      row.state = "ready";
      row.width = 1920;
      row.height = 1080;
      render();
    }, 700);
  }

  function mockGoLive(captureId, staged, edit) {
    const room = mockJoinedRooms.find((r) => r.id === edit.roomHandle);
    mockStagedCaptures = mockStagedCaptures.filter((c) => c.capture_id !== captureId);
    stagedEditState.delete(captureId);
    const newDest = {
      destination_id: destIdSeq++,
      capture_id: captureId,
      room_handle: edit.roomHandle,
      room_label: room ? room.name : String(edit.roomHandle),
      kind: "video",
      state: "starting",
      width: staged.width || 1920,
      height: staged.height || 1080,
      framerate: edit.framerate,
      bitrate_kbps: edit.bitrateKbps,
      pending: null,
    };
    // Seed the edit buffer `getDestEdit` would otherwise lazily default to
    // "best_quality"/"balanced"/"constant" — so the mock destination card's
    // Preset/Advanced-tuning controls reflect what was actually picked on
    // the staged card, same as the real Tauri branch does (see the matching
    // `destEditState.set(...)` in `goLive`, above).
    destEditState.set(newDest.destination_id, {
      bitrateKbps: edit.bitrateKbps,
      framerate: edit.framerate,
      tune: edit.tune,
      qualityLevel: edit.qualityLevel,
      rateControl: edit.rateControl,
      error: null,
    });
    mockDestinations.push(newDest);
    render();
    setTimeout(() => {
      newDest.state = "live";
      render();
    }, 800);
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
    mockStagedCaptures = await fetchStagedCaptures();
    bandwidth = await fetchBandwidthSettings();
    // `Why audio sources aren't fetched here any more:` nothing renders them
    // until someone opens a share card's "+ Add audio" picker, and that
    // handler re-lists them on open anyway (which applications are producing
    // audio changes by the second, so a list fetched at startup would be
    // stale by the time it's shown). Listing them here only bought the first
    // paint a wait on a real system-audio query — up to `withTimeout`'s full
    // 3s on a machine with no audio server — for data nothing was drawing.

    // Any staged card's Room picker defaults to the first joined room, same
    // "pick something sane, let the user change it" convention the old
    // "START A STREAM" form's Room dropdown used — see `getStagedEdit`.
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
      // Audio destinations report a level instead of frame counters, and
      // their readout lives in another destination's card — handled first,
      // and separately, because `destCardEls` has no entry for them at all.
      if (d.audio_level !== null && d.audio_level !== undefined) {
        const meter = audioMeterEls.get(d.destination_id);
        if (meter) paintAudioMeter(meter, d.audio_level);
        return;
      }

      const entry = destCardEls.get(d.destination_id);
      if (!entry) return;
      const readout = entry.el.querySelector(".dest-fps-readout");
      if (readout) {
        readout.hidden = false;
        readout.textContent = `Captured: ${Math.round(d.captured_fps)}fps · Sent: ${Math.round(d.sent_fps)}fps`;
      }

      // The destination's real live bitrate — see `DestinationStatsDto::
      // bitrate_kbps`'s doc comment for why this can't wait for the normal
      // sig-diffed `render()` path: in Absolute-Maximum mode the allocator
      // changes this value with a tier-1 encoder property write that never
      // fires `destination-state-changed`, so `mockDestinations` (and thus
      // every other rendering of `dest.bitrate_kbps`, e.g. the edit panel's
      // "Bitrate" readout when it isn't the live editable slider) would
      // otherwise show a stale number indefinitely — the reported "the
      // bitrate doesn't update when the Total Maximum changes" bug.
      // `d.bitrate_kbps` is `null` for audio (handled above) — nothing else
      // sends `null` here, so this is exactly "was this destination's row
      // video".
      if (d.bitrate_kbps !== null && d.bitrate_kbps !== undefined) {
        const dest = mockDestinations.find((x) => x.destination_id === d.destination_id);
        if (dest) dest.bitrate_kbps = d.bitrate_kbps;
        const bitrateValue = entry.el.querySelector(".dest-metric-bitrate-value");
        if (bitrateValue) bitrateValue.textContent = `${d.bitrate_kbps} kbps`;
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

  /**
   * Draw one VU-meter frame: light the leftmost `level * bars` bars and set
   * every bar's height from the same level, so the meter reads as a small
   * rising/falling block rather than a binary on/off.
   *
   * `Why the height curve isn't linear:` `audio_level` is already a dB-scaled
   * fraction (see `audio::level_fraction_from_rms_db`), but ordinary speech
   * and music still spend most of their time in the lower half of it. Each
   * bar gets a share of the range so the bar nearest the left lights first
   * and the rightmost only at near-full-scale — the usual VU behaviour, and
   * the thing that makes the meter legible at a glance rather than a
   * uniformly twitching block.
   */
  function paintAudioMeter(meter, level) {
    const clamped = Math.max(0, Math.min(1, Number(level) || 0));
    const bars = meter.querySelectorAll(".dest-audio-meter-bar");
    bars.forEach((bar, i) => {
      const threshold = i / bars.length;
      const lit = clamped > threshold;
      bar.classList.toggle("dest-audio-meter-bar-lit", lit);
      // Each bar fills across its own 1/N slice of the level range.
      const fill = Math.max(0, Math.min(1, (clamped - threshold) * bars.length));
      bar.style.height = Math.round(20 + fill * 80) + "%";
    });
  }

  window.StreamControl = { refresh: init, setOvershoot, updateStats };

  document.addEventListener("DOMContentLoaded", init);
})();
