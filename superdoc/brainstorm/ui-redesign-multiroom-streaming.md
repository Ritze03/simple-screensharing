# Brainstorm: UI Redesign & Multi-Room Streaming Architecture

**Date:** 2026-07-26
**Format:** 3 agents × 2 rounds + 1 final verify round (teamlead brainstorm mode)
**Models:** UX/Design lens — Sonnet 5 High · Performance/Rendering lens — Sonnet 5 High ·
Architecture/Data-model lens — Opus 5 High · Final verifier — Opus 5 High
**Repo:** `simple-screensharing-client`

## Origin

Triggered by two combined asks: (1) a full visual redesign of the client GUI — Discord/Slack-style
dark theme, always-on left sidebar + right content area, replacing the earlier styling-guide
conformance pass which still "looked old and bugged" — plus a detailed sidebar/stream-control spec
the user provided up front (project name / nav / rooms list / streams list, multi-destination
streaming, bandwidth-ratio splitting, multi-stream tiled viewing); and (2) a reported render
performance bug ("the UI renders at 2.5fps with immediate mode") plus a request to change
presentation mode while actively watching a stream.

Three lenses ran every round, each ranging across the whole topic, not siloed to their name:
**UX/Design**, **Performance/Rendering**, **Architecture/Data-model** (this lens explicitly owned
the bandwidth-allocation math and the multi-destination streaming model, hence the Opus tier).

---

## Round 1 — open exploration

### UX/Design — key ideas
- Flagged that "ROOMS" (new sidebar concept) and the existing saved "lounges" (`RoomStore`) read as
  two unrelated things and need reconciling.
- Sidebar needs its own scroll region for ROOMS (consumes the greedy space) and a capped,
  independently-scrolling STREAMS section — not an unbounded bottom-pin, since multi-destination
  fan-out can make STREAMS grow past sidebar height.
- Room rows: collapsed `<ID> - N Active Users`, expand to right-aligned `WxH@FPS` per user,
  icon-only `[Watch]` buttons (denser than text buttons).
- Nav (Stream Control / View / Settings) is a view-switcher only — watching/streaming must not be
  affected by which nav tab is active.
- Multi-tile grid: simple `ceil(sqrt(n))` layout, retire the old "one selected stream" concept in
  favor of every watched stream rendering at once; per-tile hover overlay (pop-out/stop/fps/volume);
  pop-out leaves a "Popped out — [Return]" placeholder instead of reflowing the grid.
- Current `theme.rs` is too flat/single-elevation for a Discord/Slack look — needs elevation tokens,
  bigger rounding (12–16px vs. 8px), a more saturated accent, and video tiles should drop card
  chrome entirely (edge-to-edge media, not bordered cards).

### Performance/Rendering — key ideas
- Original root-cause theory for "2.5fps": focus-detection getting stuck reporting "unfocused"
  (pinning the app to its 500ms/2fps fallback interval), possibly after the OS screen-share portal
  dialog steals focus. (Superseded in round 2 by more precise user repro — see below.)
- Sidebar's own live-updating data must repaint via the same coarse throttle as everything else,
  never an unconditional per-frame repaint.
- Multi-destination fan-out has a real hardware ceiling: VAAPI concurrent encode sessions are
  limited (often 2–4); many destinations × custom per-destination resolution could force most
  streams onto software encode.
- Multi-stream tiled viewing's real bottleneck is likely N concurrent **software** H.264 decodes
  (no hardware decode path exists today), not tiling/texture-upload cost.
- Pop-out windows already use real egui multi-viewport (`show_viewport_deferred`) — reuse as-is —
  but the popout closure's repaint has no focus-gating at all (repaints unconditionally forever).

### Architecture/Data-model — key ideas
- The protocol welds one QUIC connection to exactly one room (`ScreenshareClient`) — multi-room
  necessarily means N connections, not one connection carrying N rooms.
- Proposed full data model: `RoomHandle` / `CaptureSourceId` / `DestinationId`, `RoomSession` /
  `LocalCapture` / `Destination`, replacing the singleton `Connected`.
- Multi-destination fan-out via dynamically-managed GStreamer tee branches (not a static
  `parse_launch` string) — a `capture::branch` module with add/remove.
- Transfer (moving a stream to a different room) must be make-before-break: add the new
  destination, confirm it's live, only then remove the old one — bare re-pointing is impossible
  because encryption keys/nonces are room-bound.
- Bandwidth: `width*height*fps` as a pure-function weight for proportional allocation; live bitrate
  changes are cheap encoder property writes; live resolution/fps changes need caps renegotiation
  (a visible hitch) — flagged as an open tradeoff, resolved in round 2.
- Flagged `active_shares: HashMap<StreamId, _>` as a must-fix-first re-keying bug (collides across
  multiple rooms).

### Round 1 — distilled questions & answers

| # | Question | Answer |
|---|---|---|
| 1 | True simultaneous multi-room, or single-room + bookmarks? | **True multi-room** — N simultaneous connections. |
| 2 | Is "Rooms" the same concept as saved "lounges"? | **Yes, unified** — add a "+ Add Room" button at the bottom. |
| 3 | fps-bug repro detail (portal dialog involvement) | Happens switching to **Mailbox**: 2.5fps unfocused (correct), right behavior focused. **Immediate: always ~1.0fps** regardless of focus. Also: frames aren't skipped when stale — the viewer "fast-forwards to catch up" instead of skipping in real time. |
| 4 | Was the slow rate during active watching or idle/nav? | **Watching, when unfocused.** |
| 5 | OK for a destination-settings change to restart the capture? | **Yes**, as long as the viewer never sees a broken pipeline — repeat the last frame or show "Waiting for Capture", don't break other destinations. |
| 6 | Motivation for live (no-restart) presentation-mode switching? | **Testing/comparison only** — restart is acceptable if a true live switch is too complex. |
| 7 | Audio routing model | **Attached to the stream**, must be toggleable/switchable live **without the viewer noticing**. |
| 8 | Bandwidth floor vs. user's max — overshoot or starve? | **Overshoot and warn** — should basically never happen, but never starve below floor. |
| 9 | Cross-session persistence of active streams/watches? | **No** — everything starts fresh every launch; only preferences persist. User manually restarts sharing/watching each session. |

---

## Round 2 — converging on concrete designs

### UX/Design — concrete proposals
- **Add Room flow**: inline mini-card under the "+ Add Room" button (display name + join link,
  same fields as today's join form) — connects immediately on Join, no separate staging list
  (`RoomStore` already is the persistent "saved" state). Room rows render in three states: joined
  (green dot, expandable), saved-but-disconnected (hollow dot, "not connected", `[Reconnect]`), and
  reconnecting (amber pulsing dot).
- **Capture-restart placeholder**, tiered: <800ms — pulsing accent bar over the frozen last frame;
  800ms–6s — spinner + "Waiting for capture…"; >6s — danger-tinted "Still waiting — capture may
  have failed" + Retry button.
- **Live audio toggle UI**: sender-side speaker icon-button (🔊/🔇/no-audio-outline), instant
  optimistic toggle (no spinner). Viewer-side: tile's speaker icon fades in with one pulse when
  audio newly appears, fades out (no pulse) when it disappears; audio itself fades volume over
  200ms rather than hard-cutting.
- **Empty states** (now the common case every launch, not an edge case): STREAMS section shows
  "You're not sharing anything" + a primary `[+ Start a Stream]` button; the tiled viewing pane
  shows a centered "Nobody's on screen right now — pick someone from a room in the sidebar and
  click Watch" message with no button (the action lives in the sidebar).
- **Bandwidth-overshoot warning**: inline amber banner above the bandwidth section in Settings,
  states actual numbers, persistent but collapsible to a small chip, re-expands if the overshoot
  amount changes materially. Needs a new `theme::WARNING` color token.

### Performance/Rendering — concrete proposals
- **Immediate-mode ~1fps, re-rooted cause**: ruled out (via reading the pinned `wgpu-core`/`wgpu`
  source) both a silent present-mode fallback (would panic instead, since `egui-wgpu` installs no
  custom error handler) and any interaction with the app's repaint-scheduling code or
  `PacingState`/decode (both are demonstrably present-mode-agnostic). Most likely an
  environmental/compositor quirk with `Immediate`'s tearing-present request not being properly
  supported for a windowed surface on this Linux setup. **Recommendation: don't chase further —
  demote `Immediate` to experimental/unreliable; `Mailbox` already delivers full correct fps.**
- **Mailbox's 2.5fps-when-unfocused is confirmed correct/intentional** (matches the existing
  focus-gated repaint design) — not a bug.
- **"Fast-forward catch-up" bug, root cause found**: `Connected.event_rx` is an *unbounded* channel
  drained only once per `App::update()` call. While unfocused, `update()` only runs every 500ms, so
  incoming `MediaFrame` events pile up unconsumed in that channel — not in decode, not in
  `PacingState`'s budget math. The moment `update()` next runs, the entire backlog drains in one
  burst, feeding decode a rapid-fire sequence of now-stale frames; `PacingState::decide()` sees tiny
  inter-call gaps and paces each one back to real time — a literal real-time replay of history.
- **Fix**: decouple `MediaFrame` ingestion from UI repaint cadence entirely (a dedicated task/thread
  per connection feeds the decoder in real time, independent of window focus/repaint throttling).
  Additionally give `PacingState` a wall-clock-anchored absolute staleness check (a fixed epoch, not
  updated per call) as a defensive backstop — skip unconditionally if a frame is >200–300ms stale
  relative to that epoch, regardless of the existing relative budget math.
- **Live presentation-mode switching: don't build it.** Confirmed testing-only motivation, and the
  pinned `eframe`/`egui-wgpu` 0.33.3 doesn't expose the render surface for reconfiguration once the
  window exists. Keep restart-to-apply; consider a separate software-side live-adjustable rate cap
  as an independent testing/dev tool (throttles effective display rate without touching the wgpu
  surface at all).
- **Restart vs. caps-renegotiation for resolution/fps changes**: now that a full restart with a
  placeholder is acceptable, prefer restart/branch-rebuild over in-place caps renegotiation on a
  live encoder — renegotiation is the more fragile, stall-prone option.
- Multi-room removes cold-start pressure but *relocates* the decode-cost concern to sustained
  steady state (N concurrent `StreamDecoder`s can all be live indefinitely) — raises the urgency of
  fixing `decode.rs`'s per-frame `ColorImage` heap allocation and eventually adding a VAAPI-decode
  fallback path.

### Architecture/Data-model — concrete proposals
- **Ownership model, tightened**: `LocalCapture` (not `RoomSession`) owns `destinations` — a
  capture can feed destinations in several rooms, and a room disconnecting must not tear down a
  capture another room still uses. `RoomSession` holds only `Vec<DestinationId>` back-references.
  One shared `quinn::Endpoint` at the `App` level, N connections opened on it (not one endpoint per
  connection).
- **Branch-swap mechanism** — the single primitive behind per-destination settings changes,
  transfer, *and* capture restarts: build the new encode branch on the same live `tee`, keep the
  *old* branch's appsink authoritative until the new branch emits its first IDR, then atomically
  flip in Rust and tear down the old branch. Other destinations on the same tee are mechanically
  isolated via their own per-branch `queue` and are never touched. Three tiers of change: bitrate
  (live property write, zero impact) → resolution/fps/encoder-backend for one destination (branch
  swap, zero viewer-visible impact) → the capture source itself changing (actual restart, the only
  path that visibly stalls, and it stalls every destination sharing that capture simultaneously).
- **Stall/placeholder protocol**: `DestinationState` enum (`Starting` / `Live` / `Rebuilding` /
  `Stalled{reason}` / `Failed`). While `Stalled`: never send `StopStream` (no viewer-side
  teardown/resubscribe); re-send the last cached IDR at ~2fps, freshly re-encrypted with an
  advancing wire PTS; set an **unauthenticated, advisory-only** `MEDIA_FLAG_STALLED` bit in the
  media header (cosmetic — the flags byte is outside the AAD, must never drive anything
  security-relevant). Wire PTS becomes a property of the `Destination` (a `pts_epoch` +
  recalculated `pts_offset_us` on every pipeline restart) so it's monotonic across a restart —
  this is what makes a restart look like "a period of low framerate," not a discontinuity.
- **Live audio attach/detach**: audio stays a fully separate GStreamer pipeline and protocol stream
  (not multiplexed into video — would break the per-stream AAD binding). Mechanism: an **always-on
  `audiomixer` with a silent baseline input** (`audiotestsrc wave=silence`), not a `valve` — the
  mixer's Opus output never stops or restarts, emitting a packet every 20ms whether carrying
  silence or real audio, so toggling is genuinely invisible to the viewer. Attaching/detaching a
  real source = adding/removing a mixer input pad (same make-before-break idiom as branch-swap).
  Explicitly **do not** enable `opusenc dtx=true` (would reintroduce the gap this design avoids).
  New protocol field `attached_to: Option<StreamId>` on `AnnounceStream`/`StreamInfo` replaces the
  fragile "first audio stream matching this owner" heuristic.
- **`allocate()` — final allocator design**: pure function, deterministic, no I/O/clock;
  `total_kbps` / `audio_reserved_kbps` (off-the-top reservation for idle audio) / per-destination
  `DestSpec{width,height,fps,floor_kbps,ceiling_kbps}` using **configured, not measured**
  parameters (avoids feedback oscillation). Weight = `width*height*fps` as `u64` (u32 overflows at
  realistic multi-stream totals). Largest-remainder rounding so parts sum exactly. Floor/ceiling
  clamping with redistribution to a fixed point; overshoot is reported (never silently exceeded,
  never starves below floor). Live bitrate changes apply with hysteresis (>5%/100kbit/s threshold)
  and a rate limit (~1 recompute/sec) to avoid thrashing the encoder's rate controller.
- **Build order** (tightened, no-persistence removes ~1.5 steps of scope): (1) multi-room core —
  `App.rooms`/`App.captures`/`Destination`, re-key off `DestinationId`, shared endpoint, ship with
  one destination per capture first; (2) `capture::branch` module (add/remove/swap), stress-tested;
  (3) PTS continuity + `DestinationState`/stall protocol — moved earlier since step 4 starts
  exercising restarts/swaps; (4) per-destination video params + per-branch VAAPI fallback; (5)
  `BandwidthAllocator` pure function + tests (parallelizable with 2–4); (6) audio mixer rework +
  protocol field; (7) transfer (nearly free once 2–3 exist — it's a branch-swap targeting a
  different room); (8) sidebar/Stream Control UI final pass.
- Also flagged: fix `decode.rs`'s per-frame `ColorImage` allocation (reuse a 2-buffer exchange per
  `StreamDecoder`) before/alongside the tiled pane — multi-room alone already creates sustained
  N-concurrent-decode load, no tiling UI needed to trigger it. And: prefer decoding full video only
  for the focused/popped-out tile(s), rendering other grid tiles from the existing ~30s JPEG
  `PreviewCache` (already built) — the real lever for scaling the tiled view, not raw decode
  optimization. Default live-full-decode tile cap: 4.

### Round 2 — distilled questions & answers

| # | Question | Answer |
|---|---|---|
| 1 | Leaving a room while actively watching/popped-out from it — confirm or silent? | **Confirm dialog required.** (Overrides the UX agent's own no-confirm proposal.) |
| 2 | New `WARNING` color token, or reuse `DANGER` at lower opacity? | **Add a new token.** |
| 3 | Are rooms separate OS windows, or tabs in one window? | **One window** (the viewing pane). Only explicitly popped-out individual tiles become separate OS windows. |
| 4 | Confirm dialog when one source-change affects multiple rooms at once? | **Silent** — no confirmation. |
| 5 | Idle-audio cost (~3–4 kbit/s per destination) to keep toggling invisible — acceptable? | **Yes.** |
| 6 | Bandwidth "Maximum" total: per-client aggregate or per-room? | **Per-client.** Starting another stream (to any room) shrinks every other destination's share. Two named modes: **"Absolute Maximum"** and **"Per-Stream Maximum."** |
| 7 | Bandwidth floor: per-destination or shared per-capture? | **Full guaranteed floor per destination.** Confirmed: fanning one capture to 2 rooms is always 2 separate encrypted streams, but only **one capture** on the sending side. |

---

## Final verify pass — findings & resolutions

An Opus verifier checked the converged plan against all 16 answers above. Verdict: all answers were
present and none directly contradicted, but 3 internal contradictions and several real gaps were
found. Resolutions:

**Contradictions — fixed:**
1. The plan claimed manual per-stream bitrates persist across launches, contradicting the
   no-persistence answer (round 1, Q9). **Fixed:** those reset to defaults every launch like
   everything else.
2. Minor wording: "must not break other destinations" vs. "a full capture restart affects every
   destination sharing that capture" read as contradictory. **Fixed:** clarified as "other
   destinations stall gracefully via the protocol above — they are never *broken*, but a shared
   capture restart does pause everything on that capture simultaneously (accepted, since they share
   one real source)."
3. "Per-Stream Maximum = today's existing behavior" is factually wrong — the app currently has one
   global bitrate setting, not per-stream control. **Fixed:** labeled as new work.

**Gaps — resolved with defaults (confirmed by user):**
- Rooms-in-one-window was underspecified (global grid vs. room-scoped view+selector). **Resolved:
  one global tile grid across all rooms** (matches "click Watch anywhere, it shows up" from the
  original spec).
- Audio's place in the data model was undefined (own `Destination` vs. attached to one).
  **Resolved: audio is an optional attachment on a video `Destination`**, not a separate entity —
  matches "attached to the Stream."
- Leaving a room you're actively *streaming to* fell between the confirm/silent rules.
  **Resolved: same confirm-dialog treatment as leaving while watching** — you're killing an
  outgoing broadcast, not just navigating.
- Single-window multi-room means the existing focus-gated throttle now pauses *every* room's video
  at once when the app is unfocused, not per-room. **Resolved: whole-window throttle is fine for
  now** (matches what's already built); finer per-tile-visibility throttling is an explicit later
  iteration, not required for the initial build.

**Gaps — folded into build-order/scope (no user decision needed, just tracked):**
- Re-key *every* room-scoped map (`watches`, `audio_watches`, `preview_textures`,
  `self_preview_textures`, `next_stream_id`), not only `active_shares` — all currently collide
  across multiple rooms.
- Remote streams' `WxH@FPS` sidebar display needs an `fps` field added to `AnnounceStream`/
  `StreamInfo` (not derivable client-side before subscribing).
- Protocol changes (`attached_to`, `MEDIA_FLAG_STALLED`) need coordinating with the relay server and
  browser-viewer, with an explicit "additive optional field, no version bump" decision recorded when
  implemented.
- The fps-bug fixes (event-ingestion decoupling + pacing staleness backstop) get an explicit early
  build slot — they're independent of the rest of the plan and address the most concretely-reported
  bug.
- The event-decoupling fix's real work is the decoder-registry ownership change (moving from
  UI-owned `&mut self` state to something shareable with a dedicated ingestion task/thread) — name
  this explicitly when scoping that step.
- ~~`tui.rs` (a second, less-visible frontend reusing `net.rs`/`capture.rs`/`decode.rs`) and
  the end-to-end test harness construct state directly and will need updating alongside every
  refactor step, not just `ui.rs`.~~ `tui.rs` was removed 2026-07-26 (see
  `architecture/implementation-notes.md`'s "TUI front-end removed") — `ui.rs` is now the only
  frontend. The end-to-end test harness (`end_to_end_test.rs`) still constructs state directly
  and still needs updating alongside every refactor step.
- New UI strings need locale entries in both `en.yml` and `de.yml`.
- Add a "remove/forget saved room" affordance to the ROOMS section (the store already has a
  `remove` method, just no UI hookup).
- Two existing docs (`rooms.rs`'s present-mode doc comment, `superdoc/features/presentation-modes.md`)
  already incorrectly claim automatic vsync fallback — correct both as part of the Immediate-mode
  work, per this project's documentation discipline (fix docs in the same change).

---

## Final Plan (converged, gap-free)

1. **Multi-room core**: `App` owns `room_store` (persistent), `rooms: IndexMap<RoomHandle,
   RoomSession>` (ephemeral), `captures: IndexMap<CaptureSourceId, LocalCapture>` (ephemeral, owns
   all GStreamer state), `settings` (persistent preferences only — bandwidth mode/totals,
   presentation mode, language; nothing about active streams/watches). `LocalCapture` owns
   `destinations`; `RoomSession` holds only back-reference IDs. `Destination` is always 1:1 with one
   room — fan-out to N rooms means N destinations off one capture, each independently encoded and
   encrypted. One shared `quinn::Endpoint`, N connections. Re-key every room-scoped collection off
   `DestinationId`/room-scoped keys. No cross-session persistence of any active state — everything
   starts empty every launch.
2. **`capture::branch` module** (add/remove/swap dynamic GStreamer tee branches) — the single
   mechanism behind live per-destination settings changes, stream transfer, and graceful capture
   restarts alike. Stress-tested against repeated add/remove cycles on a live pipeline.
3. **Stall protocol**: `DestinationState` state machine, cached-IDR repeat + advisory
   `MEDIA_FLAG_STALLED` bit + monotonic per-destination wire PTS, so a capture restart never breaks
   the pipeline and always looks like a brief framerate dip to the viewer, never a glitch.
4. **Per-destination independent resolution/fps/bitrate**, capture running at the max fps any of
   its destinations needs, per-branch downscale/downrate, per-branch VAAPI→software fallback.
5. **Bandwidth allocator**: pure `allocate()` function, `width*height*fps` weighting, two named
   modes — **Absolute Maximum** (one per-client aggregate cap across every destination in every
   room) and **Per-Stream Maximum** (manual per-destination control, new work) — full guaranteed
   floor per destination, overshoot-and-warn (never starve below floor), idle-audio bandwidth
   counted off the top.
6. **Live audio**: always-on silent-baseline `audiomixer` per capture, real sources attached/detached
   as mixer input pads, `attached_to` protocol field pairing audio to its video stream, fully
   invisible toggling to the viewer at a small constant idle-bandwidth cost.
7. **Transfer**: make-before-break branch-swap targeting a different room's `RoomSession`.
8. **fps bug fixes** (independent, early slot): decouple `MediaFrame` ingestion from UI repaint
   cadence (dedicated always-on per-connection feed, not gated by focus/repaint throttling) to
   eliminate the backlog that caused "fast-forward catch-up"; add a wall-clock-anchored absolute
   staleness backstop to `PacingState`; demote `Immediate` present-mode to experimental (root cause
   is an environmental/compositor quirk, not fixable in this codebase); correct the two docs that
   wrongly claim automatic vsync fallback.
9. **Live presentation-mode switching**: not building true live wgpu reconfiguration (testing-only
   motivation, current dependency version doesn't expose the surface for it) — keep restart-to-apply;
   optionally add a separate software-side rate-cap dev tool later.
10. **UI redesign**: always-on left sidebar (Project Name / chunky pill nav / scrollable ROOMS with
    "+ Add Room" and three-state room rows / capped-scroll STREAMS with inviting empty state) + right
    content area (one global auto-tiled viewing grid across all rooms, per-tile pop-out to real OS
    windows, inviting empty state, capture-restart placeholder tiers, live audio-toggle affordances,
    bandwidth-overshoot warning banner with a new `WARNING` color token). New Discord/Slack-style
    `theme.rs` pass: elevation tokens, bigger rounding, more saturated accent, chrome-free video
    tiles. Room-leave confirmation dialog (covers both watching and streaming-to cases). Locale
    entries for all new copy in both languages.

**Explicitly deferred (not in this build, noted so it isn't silently dropped):** manual tile
resizing in the multi-view grid (auto-tile only for now, per the user's original note); a
finer-grained per-tile-visibility render throttle (whole-window focus throttle is fine for the
initial build); hardware-accelerated video decode (VAAPI decode fallback flagged as worth adding
once multi-room load makes it necessary, not blocking this plan).
