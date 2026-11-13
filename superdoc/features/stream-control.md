# Stream Control (desktop client)

> **Status (2026-11-07).** The client is one native egui binary and these pages are its **backend** reference for sharing:
> `client/src/capture.rs` (with its inline `destination` and `branch` modules), `session.rs`, `bandwidth.rs`, `presets.rs`,
> `rooms.rs`. The UI lives in `client/src/ui/stream_settings.rs`, `ui/share.rs`, `ui/stage/bar.rs`, `ui/room.rs`,
> `ui/modals.rs` (see `superdoc/ui/APP-SHELL.md`). The page was split by topic on 2026-11-07 (row 161): it used to be one
> 230 KB file, the full text is in version control (`git log -- superdoc/features/stream-control.md`). The removed Electron
> frontend's per-widget write-ups were dropped earlier; only the *why* that outlived them was kept, on the pages below.
> The old always-on 320x180 "self-preview" branch is gone; since #78 a valve-gated live preview branch feeds the sharer's
> own tile while the window is focused (`superdoc/features/self-preview.md`, and "Own-tile live preview" in
> `stream/capture-pipeline.md`).

**Added 2026-07-26.** The feature set built in the second stage of the original three-stage UI plan (`superdoc/ui/APP-SHELL.md`): starting and editing outgoing streams, **multi-destination fan-out**, **transfer** between rooms, and the two **bandwidth modes** with `bandwidth::allocate` wired into live encoders. The plan it implemented is `superdoc/brainstorm/ui-redesign-multiroom-streaming.md` (final plan items 4, 5 and 7).

## Pages

- [`stream/capture-sources.md`](stream/capture-sources.md): what a capture points at: gamescope, the source changing size, the screen picker, idle monitors, source errors.
- [`stream/capture-restart.md`](stream/capture-restart.md): stop is instant, a lost capture source is restarted, sender keyframe redundancy, big-keyframe pacing.
- [`stream/capture-pipeline.md`](stream/capture-pipeline.md): the shape of a capture (one pipeline, N destinations), the own-tile live preview, 120-240 fps performance.
- [`stream/go-live-and-destinations.md`](stream/go-live-and-destinations.md): staging, "Go live", fan-out, transfer, the three edit tiers, resuming, retrying, test coverage.
- [`stream/encoder-tuning.md`](stream/encoder-tuning.md): presets (Text, Quality, Gaming), tuning controls and backend mapping, start-stream defaults.
- [`stream/vaapi-target-usage.md`](stream/vaapi-target-usage.md): `target-usage` on Mesa AMD and elsewhere, how it reaches the hardware, the encoder-limit hint.
- [`stream/video-codecs.md`](stream/video-codecs.md): H.264 / H.265 / AV1: `VideoCodec`, the #95 availability probe, output caps, codec strings.
- [`stream/rate-control.md`](stream/rate-control.md): history of the rate-control modes, why encoding is always Dynamic now.
- [`stream/encoder-failures.md`](stream/encoder-failures.md): settings changes on a live share, coded-buffer and AV1 branch deaths, burst and freeze root causes.
- [`stream/keyframes-and-bandwidth.md`](stream/keyframes-and-bandwidth.md): keyframes bigger than the uplink, the pacing hold, the bandwidth modes and allocator.
- [`stream/webcam-capture.md`](stream/webcam-capture.md): the webcam as a stream of its own.
- [`stream/share-audio-attachment.md`](stream/share-audio-attachment.md): audio as an attachment on a screen share.

---

### Auto-Connect (join a saved room at startup)

`rooms::SavedRoom::auto_connect: bool` (`#[serde(default)]`, so older `rooms.toml` files load as
`false`), `AppState::set_room_auto_connect`, and `AppState::auto_connect_queue` drained one room at a
time by `poll_auto_connect_queue`. *Why a queue:* `pending_connect` is a single slot, so firing
several `start_connect`s at startup would orphan every one but the last. Nothing else makes a
saved room rejoin itself at launch. **Auto-connect only connects** (row 192, user: "All connects, both auto an manual"): every connect, auto or
manual, leaves you **not in the call** (see you who is there, watch nothing) until the green Pick up button;
a room that reconnects after a drop keeps its state. `ui/APP-SHELL.md`, `ui/STAGE.md`.
(A first misreading wired this checkbox to "auto-start audio
sharing"; the owner's correction: it means "connect to the room on startup".)

---

## Deliberately not built here

The audio-as-attachment model (audio is still its own capture and its own stream id on the
wire — only its *presentation to a viewer* is now bundled with video, see above);
the View tab's tiled grid and tiered restart placeholders; per-destination floor/ceiling
*per* destination (one global floor is exposed, and every destination shares the project's
40 Mbit/s ceiling); an idle-audio reservation in `AllocInput::audio_reserved_kbps` (left
explicit at 0 until audio becomes an attachment with a known constant rate).
