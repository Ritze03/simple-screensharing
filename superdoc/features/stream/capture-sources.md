# Capture sources

What a capture can point at (portal pick, gamescope, a window, an idle monitor), what happens when the source resizes or errors, and how those failures are reported.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## Sharing gamescope directly (2026-11-05, row 142)

**Brief.** The user, with a screenshot of the PipeWire graph (node `gamescope-<name>`, subtitle `gamescope`, output port `capture_1`): "if
this capture is detected, first ask if the user wants to stream his screen or gamescope directly". Native client, Linux only.

**Detection** (`capture::gamescope`, `nodes_in` is the pure part): `pw-dump` (the audio module's parser, `audio::run_pw_dump`), a node with
`media.class == "Video/Source"` whose `node.name` or `media.name` contains "gamescope", ignoring case. Stock gamescope is `gamescope`, the
user's build `gamescope-<name>`; the same-named audio nodes and the webcam (`v4l2_input...`) do not match. *Why a look-up per Share click and
no cached list:* a cached "no gamescope" would hide a game started a minute ago, and `pw-dump` is ~30 ms off the UI thread (2 s timeout,
then "none"), so without gamescope the click reaches the portal as before.

**Flow.** Share main button (`ShareNow`) and menu entry (`StageShare`) both go through `App::stage_share`, which starts the look-up; `poll_staged`
finishes it: none = today's flow, found = `Modal::SharePick` ("Screen" / "Gamescope"). Screen = `SharePicked { node: None }` = the portal picker;
Gamescope = `stage_gamescope_share_with_saved_defaults(node)`: no portal, the main button goes live at once, the menu path opens the Stream
settings modal. The decision is `ui::share_after_probe` (tested).

**Capture.** `CaptureSource::Gamescope(Node)`: `pipewiresrc target-object="<node.name>" do-timestamp=true keepalive-time=.. min-buffers=..`
(the portal's fragment without `fd`/`path`: it connects to the default PipeWire daemon). Same pipeline after the source: the GPU domain
(DMA-BUF, `vapostproc`) with the CPU fallback on a failed start, `SizeWatch`, preview, encoder choice. The share is labelled `Gamescope` on the
wire (any non-`Webcam` label is a screen to every viewer). The xdpw start-up workarounds (`decoupled_source` queue, the 30 ms pause, `repair_portal_start`) also run
because they key on `pipewiresrc`; they are harmless here (the first-buffer wait is at most 200 ms).
*Why keepalive and silence = loss as for the portal:* gamescope repaints when the game does, so a still game sends nothing; the keepalive resend is the
heartbeat, and with it a silent tee means the node is gone.

**The node dies.** `pipewiresrc` reports `stream error: target not found` on the bus (measured, rig): the capture is marked lost and the session's
recovery runs (`CAPTURE_RESTART_DELAYS`: 0, 2, 5, 10, 30 s). The "restore token" of a gamescope capture is `node:<node.name>`
(`gamescope::TOKEN_PREFIX`), and `start_capture_restoring` answers it with `start_gamescope_capture`, which looks the node up again by name each
attempt (its id changes), never the portal. A game back within the schedule is picked up (measured: attempt 4 attached to the new node);
otherwise the share ends with `capture lost: the gamescope capture "gamescope" is gone`.
`ponytail:` two gamescopes at once: the first listed wins.

**Measured (rig, 2026-11-05, release).** Real `gamescope --backend headless -W 1280 -H 720 -- vkcube` inside the rig's sway and private PipeWire:
node `gamescope`, capture started in the Gpu domain, viewer decodes 1280x720 30 fps H.264 via `vah264dec`, 6 Mbps. Main button and menu path
both worked, "Screen" went to the portal, killing gamescope mid-share gave the give-up error after 5 attempts and no picker. A fake `pipewiresink`
`Video/Source` (`videotestsrc`, BGRx) behaved the same, including the quick-return recovery. Not measured: a node that changes size in place (the
headless gamescope cannot be resized and the fake node did not renegotiate); that path is the shared `SizeWatch` of row 141.

**Why** (the user's wording above): ask first rather than guess, because a gamescope node exists whenever a game session runs, even while the user
means to share the desktop.

---

## The source changes size: a window resized, a mode change, a game changing resolution (2026-11-05, row 141)

**Brief.** While a share is live the source can change size. Before this, nothing noticed: `source_caps()` cached the first size for
good and the only way out was the dead-source route below, which waited for the silence watchdog (3 s without an encoded frame, 2 s
more) and then restarted: about 6 s frozen.

**Measured in the rig (stock xdpw 0.8.4, `swaymsg output HEADLESS-1 resolution ...`, `client/scripts/hs-resize.sh`).** The portal
cannot follow a size change in place. Three outcomes were seen, run to run, at every codec: (a) `xdpw.log` `ext: frame capture failed:
buffer constraint mismatch`, then `out of buffers` / `unable to export buffer` for ever; `pipewiresrc` renegotiates and pushes the new
size as a caps event, but delivers 0 fresh buffers (a bare `pipewiresrc ! queue ! fakesink` probe does the same, so it is not our
pipeline); (b) the same without any caps event; (c) the node is destroyed (`stream error: target not found`). A burst of 16 buffers
sometimes arrives seconds later and nothing after it. So an in-place fix (rewriting caps, swapping branches) cannot help against this
portal, and the restart (new portal session, restore token, ~0.5 s) stays the answer for it. Window capture was not testable: the rig's
xdpw shares outputs only (`chooser_type=none`).

**What changed.**
- `SizeWatch` (a probe on the tee's sink pad, `capture.rs`): the live frame size from the caps events, when it last changed, buffers
  since, and the newest buffer's time. `source_caps()` stays the size the destinations were built for (the basis `rescale_for_new_source`
  and `edited_params` work from); the live size is separate.
- `CaptureHandle::poll_resize` (called per frame from `AppState::poll_capture_recovery_at`, step 0):
  1. the size must stay put for `RESIZE_SETTLE` (600 ms), so a drag with many sizes per second acts once;
  2. settled and **fresh** buffers arrived (`RESIZE_ALIVE_FRAMES` = 3 distinct `GstBuffer`s, because `pipewiresrc`'s keepalive resends the
     held buffer and must not read as alive): every destination gets `rescale_for_new_source(params, old, new)` through
     `change_destination`, the ordinary tier-2 branch swap (build alongside, first IDR, flip). The new size becomes `source_caps()`.
     Viewers keep the stream id; the decoder reconfigures on the new parameter sets and the stage re-lays out from the decoded size
     (the relay's announced `width`/`height` are not updated, nothing reads them once a frame decoded);
  3. settled and no fresh buffer: the capture is marked lost at once (`SOURCE_RESIZED_TEXT`) and the session restarts it, instead of
     waiting for the 5 s watchdog;
  4. no buffer at the tee for `SOURCE_GAP_LOST` (2 s) on a keepalive source: lost too, for outcome (b), which has no caps event.
  The pumps' watchdog holds off `SourceDead` while the size is still changing (`CaptureErrors::size.resizing`).
- `finish_recovery`: a restart that comes out at another size does not walk the restart backoff (`recovery_history`). A drag with a portal
  that cannot follow it restarts once per settled size, each time at position 0 (before: +2 s, +5 s, ... after the second resize).
- `rescale_for_new_source` moved to `capture::destination` (re-exported by `session`); the per-mille share is rounded and 99.5 % counts as
  the whole source (truncation drifted 1920x1080 to 1916x1076 over three restarts).

**Why:** a branch has fixed encode caps; scaling into them keeps the stream alive but distorts the aspect, and an encoder cannot take new
parameters mid-stream, so the branch swap is the one honest way to a new size. It is debounced because every swap is an encoder build.
The restart is kept, not replaced, because the measured portal needs it.

**Measured (rig, release, 60 fps, stock xdpw, viewer = client 2):**

| Scenario | Before | After |
| --- | --- | --- |
| one mode change 1920x1080 -> 1280x720, H.264 / H.265 | source declared lost 5.7 s later, ~6.3 s frozen | lost 1.1 s after the change, recovered 0.5 s later: ~1.6 s, viewer at the new size and aspect |
| burst of 14 sizes in 1.2 s, ending 1600x900 | (not run; schedule +2 s, +5 s each) | one or two restarts at position 0, picture back ~2.7-3 s after the first change, correct size |
| size change with no caps event (outcome b) | ~5.7 s | 2 s gap rule + 0.5 s restart: ~2.6 s |
| live source that survives (test source 1280x720 -> five sizes in 500 ms -> 640x480) | not handled | one swap, viewer decodes 640x480, no gap over 3 s (`end_to_end_test::a_source_that_changes_size_keeps_the_viewer_decoding_at_the_new_aspect`) |

**Known ceilings (`ponytail:` in code).** A destination whose swap is already in flight when the size settles answers `Err` and keeps
the old size until the next change. The 2 s gap rule restarts a capture that stalls 2 s for any reason (the watchdog's 5 s before).
One H.265 run hit `ring vcn_unified_1 timeout` (only that ring was reset, `Ring reset succeeded`, the client process died with `amdgpu: The
CS has cancelled because the context is lost`): the encoder was busy while the old capture was torn down. Not reproduced in two more runs.

**For a direct `pipewiresrc` of a `Video/Source` node (gamescope, row 142):** such a node renegotiates in place, so it takes path 2 (swap),
not the restart; `SizeWatch` hangs on the tee and works unchanged for any source. Frames there must be fresh buffers, not keepalive
resends, or `poll_resize` reads it as dead.

### Resize recovery verified on software, one silent share death fixed, VA still faults on a gentle window resize (2026-11-07, row 148)

**Why this was done.** The user: "Frequent resolution changes (for example, when sharing a window), kills the screenshare (stalls). Make the
recovery reliable. Right now it sometimes trips out and just stalls forever", then "Push and test it (crashes shouldnt happen anymore)".
The code under test is the row-148 WIP: `poll_resize` reshape retries (`RESHAPE_RETRY`/`RESHAPE_TRIES`), `FIRST_FRAME_LOST`,
`Watchdog` (rebuild, restart the capture, fail; 20 s per step, `WATCHDOG_AFTER`), `RESTART_ATTEMPT_MAX`, `MIN_ENCODE_DIM` = 144
(`clamp_to_source` raises each axis to 144, so a sliver window is stretched on that axis only; the VA encoders refuse to link below 128/144).

**What was exercised.** Real window share: `HS_PATTERN=window` puts a floating `vkcube` window on the captured output, xdpw 0.8.4 picks it
with `chooser_type=dmenu` (`grep -m1 '^Window: vkcube'`), the sharer runs with `SSS_PORTAL_SOURCES=window` (test knob in
`portal_source_types`; the product asks for monitors only), `hs-resize.sh winstress|ostress` resize it with `swaymsg resize`. Also the output
mode change (`hs-resize.sh ostress` without `RESIZE_TARGET=win`). Software = `LIBVA_DRIVERS_PATH=/nonexistent SSS_CAPTURE_PATH=system` in
`CLIENT_ENV_1` and `_2` (the log then says `VAAPI encoder element 'vah264enc' is not available`, `capture started in the System domain`, the
viewer `H.264 decoding via avdec_h264 (software)`).

**Finding 1: stock xdpw never follows a resize in place, for a window too.** Every settled window size ends in `the capture source delivers
nothing since it changed size`, a portal restart (about 0.5 s, up to 3 s when the start-up flap repair runs) and a fresh branch. So against this
portal the `poll_resize` branch-swap path (the in-place one) is almost never taken; the restart path carries the load. The swap path is
covered by `end_to_end_test::a_window_resized_at_random_keeps_streaming` (test source, software, 40 s: 69 sizes, 13 swap rounds, longest gap
2.0 s, final aspect right). Run: `E2E_FORCE_BACKEND=software SSS_STRESS_SECS=40 SSSHARE_SERVER_BIN=... cargo test -p simple-screensharing-client
--lib --release -- --ignored a_window_resized_at_random --nocapture`.

**Finding 2 (bug, fixed): a destination that failed to come up during a restart ended the share silently.** A restart re-adds the
destinations on the new capture (`finish_recovery`), the bring-up takes up to 5 s (`no IDR within 5s`), and a window dragged again meanwhile
kills the new source under it. `poll_pending_destinations` treated that `Err` as the user's problem: `forget_destination`, a toast, and a
capture that "recovered" with zero destinations (UI `Not sharing`, portal session left running, nothing ever retried). Two orderings were
caught in 120 s window stresses (seeds 11 and 21, each one in about three runs): the error arrives while the recovery runs (fixed: the
destination stays in the capture, `finish_recovery` re-adds it, log `failed to come up while its capture is being restarted`), and it arrives
0.5 s before the 2 s silence rule notices the dead source (fixed: a destination added by a restart carries `PendingDestination::from_recovery`,
its failure calls `CaptureHandle::declare_lost`, the next restart re-adds it, log `restarting the capture again`; the restart schedule
bounds the loop and ends in a visible failure). Without a restart and without `from_recovery` the old behaviour stands (the user's own
Go live failing is still an error). Test: `session::ui_ops_tests::a_destination_that_fails_during_a_capture_restart_is_kept_for_the_restart`
(the first ordering; the second needs a real capture and is covered by the stress runs).

**Software stress results (x264, rig, release, 30 fps, viewer = client 2; viewer evidence = hash of the viewer tile's pixels every 0.2 s and
the tile aspect from `SSS_UI_DUMP`):**

| Run | Sizes | Longest frozen picture | End state |
| --- | --- | --- | --- |
| output mode changes, one at a time (1280x720, 640x360, 1920x1080) | 3 | 1.5 s | live |
| output, 180 changes in 90 s down to 40x30, bursts of 6 at 80-160 ms | 180 | 3.7 s | live at 1280x720 (`Decoded 30 fps, 0 dropped`) |
| window, `winstress` 90 s seed 5 (40x30 up to 1800x1000, bursts of 3-8 every 60-250 ms) | 159 | 4.8 s | live, aspect of 1000x600 |
| window, 120 s seed 11, before the fix | 185 | **61.9 s and never ended**: share gone (finding 2) | `Not sharing` |
| window, 120 s seeds 21 (before fix: 76.5 s, never ended; after: 7.3 s), 11 and 33 (after) | 219 / 204 / 195 | 7.6 s / 7.3 s / 10.1 s | live every time |

The longest freezes sit inside bursts (each settled size is a restart); after the last change every run showed a moving picture again within
about 2 s, no `StartFailed`, no watchdog step (`watchdog 1/3` never logged: the faster paths always won), no `share error`.

**Finding 3: VA, window share, gentle sizes: the GPU faulted on the second change (HARD STOP, run ended).** Stage B (VA encoder `vah264enc`
60 fps, `vapostproc` GPU capture, viewer `vah264dec`, window sizes >= 640x360, one change every 3-6 s): the 4th change (858x977, 3.5 s after the
previous one) was followed within 1.5 s by `amdgpu: [mmhub] page fault (src_id:0 ring:56 vmid:15 pasid:1402) ... in page starting at address
0x0000000000000000 from client 18 ... Faulty UTCL2 client ID: VCN0` in the sharer's encoder thread (`simple-scr:cs0`), then `ring vcn_unified_0
timeout`, `Starting vcn_unified_0 ring reset`, `Ring vcn_unified_0 reset succeeded` (no full GPU reset this time). The client log shows what
ran: the previous restart's VA branch torn down (`capture pipeline did not reach EOS within 1s; forcing it to Null anyway`) while the next
restart built a new VA encoder; two such restarts 2.1 s apart. Stages B and C were stopped there and nothing more ran on the GPU.
So even sizes of 640x360 and up at one change per 3 s are not safe on this machine's VA encoder when each change is a capture restart
(the only kind this portal gives). Untested: whether `MIN_ENCODE_DIM` matters (the fault came at a normal size), whether the odd height 977
is the trigger (the clamp does not round to even), and whether slower changes or a longer EOS wait avoid it. Candidate fixes (not done, they
need GPU runs that this machine cannot take safely): wait for the old pipeline's real EOS or Null before building the next VA encoder, and
space restarts further apart.

---

## Cancelling the screen picker is silent (2026-10-14)

Dismissing the portal's picker (Esc, Cancel, or the picker being killed) used to surface
as `screencast portal 'Start' request failed: ... Invalid session`. Measured against the
real `xdg-desktop-portal-wlr-patched` + `wlr-share-picker`: the `Start` call itself fails,
~1 s after the dismissal, with `org.freedesktop.DBus.Error.AccessDenied: Invalid session`
(not a `Response` code). `capture::is_portal_user_cancel` classifies that, plus the
standard `ResponseError::Cancelled` and `PortalError::Cancelled`, as a dismissal;
everything else (`Failed`, `NotFound`, any other D-Bus error, missing portal) is still a
real failure. A dismissal becomes `capture::PickerCancelled` (find it with
`is_picker_cancelled`), which `session` turns into `SHARE_START_CANCELLED`: the capture is
removed and `share_error` stays empty.

---

## Idle monitors: a capture can go without a first frame (2026-10-14)

**Symptom:** sharing an idle monitor (nothing repainting) through the real portal ended
with `VAAPI encode branch ... produced no IDR (... within 5s); retrying on software`, then
`encode branch did not drain to EOS within 5s; removing it anyway`, and the share vanished.
A monitor that repaints worked.

**Root cause (measured, not inferred):** `xdg-desktop-portal-wlr` captures with
`wlr-screencopy`'s `copy_with_damage`, and the compositor only answers when the output
commits a new frame. On a monitor where nothing repaints it doesn't answer even the
*first* request, so `pipewiresrc` never receives a buffer. `GST_DEBUG=pipewiresrc:6` on
idle output: stream reaches `streaming`, two buffers allocated, then `popped buffer (nil)`
forever; 0.0 fps at the tee for 20 s. One pointer movement onto the monitor produced a
frame, and from then on `keepalive-time` held ~29 fps with the pointer gone, for as long
as it was watched (60 s). So `keepalive-time` is correct but can only *repeat* a buffer it
already has — it cannot supply the first one. (Side effect seen in the same trace: with
`keepalive-time` set and no buffer ever received, `pipewiresrc` spins on a nil pop instead
of sleeping — ~500k log lines/s at `pipewiresrc:6`. Not fixable from here.)

**What the old code did with that:** built the first destination's encode branch against a
silent tee, burned the hardware encoder's 5 s IDR deadline, burned another 5 s on the
software retry, and reported the *encoder* as the failure. The drain-to-EOS warning was a
second bug: `remove_branch` injected its EOS from a `BLOCK_DOWNSTREAM` probe that only fires
when a buffer is pushed, so on a silent source every removal waited out its full 5 s.

**The fix:**

- `branch::wait_for_source_frame` — before any branch is built, wait for the `tee` to see a
  buffer (a one-shot `BUFFER` probe on its sink pad). `branch::add_branch_live_waiting`
  takes a `SourceWait`: `Indefinite` for a destination's first branch and for
  `rebuild_after_source_restart` (idle is a normal state there, and a stalled destination's
  viewers are fed the cached IDR meanwhile); `Bounded(SOURCE_FRAME_TIMEOUT)` for tier-2
  swaps (`add_branch_live`), where the old branch is still running and a dead source should
  fail back to it. The wait ends with an error if the pipeline goes to `Null`, so a stopped
  capture never leaves a task parked. The encoder's 5 s IDR deadline now only ever measures
  the encoder.
- `remove_branch` also installs an `IDLE` probe, so EOS is injected even when no buffer is
  in flight.
- `CaptureHandle::awaiting_first_frame` / `AppState::capture_awaiting_first_frame` expose
  "this capture has never produced a frame" so a UI can say *why* a destination is still
  `Starting` ("waiting for the shared screen to change"). Rendered by `ui/share.rs` (`awaiting_first_frame`, `waiting_hint`) and `ui/stream_settings.rs`.
- `session::poll_pending_destinations` no longer raises `share_error` for a destination
  whose capture/destination the user already stopped, and stops (instead of adopting) a
  destination that finishes coming up after it was cancelled. Both windows used to be ~5 s
  and are now unbounded on an idle monitor.

**What the user sees:** sharing an idle monitor does not fail. The destination stays
`Starting` until anything on that monitor changes (pointer onto it, a window, a clock),
then comes up in ~0.2 s. Verified for real: waited 7.2 s, nudge at 7.0 s, viewer's first
frame 185 ms after subscribing, 24.5–28.4 media fps over the next 60 s untouched
(`capture::tests::real_portal_idle_monitor_share_reaches_a_viewer_and_stays_alive`).

`Why not get the first frame some other way:` it needs a compositor repaint, which a
client can't ask for through the portal. A one-shot `wlr-screencopy` `copy` (no damage)
would return immediately but bypasses the portal's consent flow; a tiny layer-shell surface
on the captured output would force a commit but is invasive. Both are options if "starts
only after the first repaint" turns out not to be good enough in practice.

### Silence watchdog (the stall protocol's other entry)

Until now only a *deliberate* source restart ever entered `Stalled`. A source that simply
went quiet left the viewer with nothing. `DestinationRuntime::stall_if_silent` (driven by
the pump's 500 ms ticker; threshold `destination::silence_threshold`, ≥3 s) now moves a
`Live`, un-paused destination to `Stalled(SourceLost)` after that much silence, so the
cached-IDR keepalive starts; the first frame back also forces an IDR (a delta can't end a
stall). With `keepalive-time` a healthy static screen never trips it. The silence clock is
restarted on resume-from-pause and after a swap so those gaps don't count.

---

## Capture source errors lead the share error (2026-10-15, #76)

**Symptom (a user's AppImage build on a Hyprland desktop):** `Share error: failed to bring up the encode branch
for stream 0: software fallback encode branch produced no IDR either`, while the terminal
had the real cause: `capture pipeline error from .../GstPipeWireSrc:pipewiresrcN: stream
error: unhandled format`. The root cause of *that* error is packaging, see
`client/packaging/README.md` "the bundled `pipewiresrc` is a patched build".

**Why the message was wrong:** a source that dies right after its first frame(s) leaves the
encode branch without input, and `branch::add_branch_live_waiting` can only observe "no IDR
within 5 s" (twice: hardware, then the software retry). The capture pipeline's bus error was
consumed by `capture_loop` and only logged, so the user-facing error blamed the encoder.

**Fix (smallest one):** `capture_loop` now also records the *first* bus error in the
capture's `CaptureErrors` (`CaptureHandle::capture_errors`, shared with the loop through an
`Arc<Mutex<Option<String>>>`; later errors are consequences, so they are dropped).
`spawn_destination` passes a failed bring-up through `blame_the_source`: with a recorded
error the result reads `screen capture failed: pipewiresrc17: stream error: unhandled
format: failed to bring up the encode branch for stream 0: software fallback encode branch
produced no IDR either: encode branch produced no IDR within 5s` (the encoder's own chain
stays at the end for diagnosis); without one it is unchanged. Covered by
`capture::tests::a_source_error_on_the_bus_is_blamed_when_the_branch_fails`.

`Why post-hoc and not cancel the wait on a source error:` `add_branch_live_waiting` may be
mid-way through building a branch; dropping its future there could leave an orphan branch
on the tee (its failure arms do the cleanup explicitly). The 5 s IDR deadline bounds the
delay.

**Known gap:** a source that errors *before its very first frame* never reaches the IDR
stage: `wait_for_source_frame(Indefinite)` keeps waiting (the destination stays `Starting`)
and the recorded error is not surfaced until something fails. A source that dies that early
is normally caught by `start_pipeline_bounded` (its bus poll), so it has not been seen.
