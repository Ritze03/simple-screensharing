# Webcam capture

Sharing a camera as a stream of its own: the source, fixed encoding and the decisions behind them.

Part of the Stream Control reference, hub: `superdoc/features/stream-control.md`. The text moved here verbatim from that page: "above", "below" and "this page" in it may point at a section that now lives in a sibling page of this folder (the hub lists them).

## Webcam capture (2026-11-04, task I8, plan decisions D7, D8, D10)

A native webcam share is **its own capture**, not a mode of the screen share: a second `LocalCapture` next to the
screen (or alone, a screen share is not needed), with its own destinations and its own bar button. Code: the inline
`capture::webcam` module (listing, modes, the source fragment, pure and tested), `CaptureSource::Webcam`,
`capture::start_webcam_sharing`; `AppState::start_webcam` / `stop_webcam` / `set_webcam_settings` /
`refresh_webcams` and `AppSettings::webcam` (`rooms::WebcamSettings`) on the session side; the UI is
`ui/webcam_settings.rs`, `share::webcam_menu`, `stage/pip.rs` (`superdoc/ui/STAGE.md` "Webcam and webcam in picture").

- **On the wire a webcam is an ordinary `StreamKind::Video` stream whose label starts with `Webcam`** (D7;
  `webcam::LABEL`, `webcam::is_webcam_label`, `LocalCapture::is_webcam`, `LocalCapture::wire_label`). *Why a label and
  not a new kind:* a new `StreamKind` would break every peer that does not know it (the wire is unchanged); a label
  does not: an older client shows the webcam as one more stream called "Webcam" (its picture, Watch button, volume
  row and all). The one rule "label starts with `Webcam`" is what the stage, the audio rules and the PIP read, and the
  browser client uses the same name. `spawn_destination` takes the label from the capture's source
  (`CaptureSource::wire_label`), so a destination added later (fan-out, a reconnect) announces it too.
- **Linux only, `v4l2src`, System domain** (D10). `CaptureSource::Webcam(Spec)` and `start_webcam_sharing` are
  `#[cfg(target_os = "linux")]`, as the Windows-only arms next to them are `#[cfg(windows)]`; the rest of
  `capture::webcam` (modes, keys, `source_fragment`) compiles everywhere, `list_cameras()` is empty and
  `unavailable()` says `Platform` off Linux, which disables the bar's button with a tooltip. The pipeline is
  `v4l2src device=".." ! image/jpeg,WxH,framerate=N/D ! jpegdec` (MJPEG modes) or `v4l2src ! video/x-raw,format=F,WxH,framerate=N/D`
  (raw), then the usual System-domain `videoconvert ! BGRx ! tee`. *Why no GPU domain:* a camera frame is small,
  `CaptureDomain::for_source` only takes the GPU path for the portal, and nothing is gained by a VA upload of a
  720p picture that a decode already put in system memory. A missing `v4l2src` plugin (`Unavailable::NoPlugin`)
  disables the button; it never crashes.
- **Modes come from the device** (`webcam::list_cameras`, blocking: run it off the UI thread, `AppState::refresh_webcams`
  does). GStreamer's `DeviceMonitor` with class `Video/Source`, so no new crate. **Two providers show up depending on
  the machine and the code reads both:** GStreamer's own `v4l2deviceprovider` (the rig: its private PipeWire has no
  v4l2 monitor) says `device.path` and **merges structures** (`width=2304, height={ 1536, 1296 }`); PipeWire's
  provider (a desktop with PipeWire's v4l2 monitor, the host: the monitor then shows *only* that one) says
  `api.v4l2.path` and lists one structure per size with a rate list. `modes_of` expands every list field
  (`list_of`: one value or a `gst::List`); a first version read only plain values and silently lost 114 of the C920's
  217 modes under the native provider (found in the rig: the modal said 103 modes, `v4l2-ctl` says 217). Skipped:
  `memory:DMABuf` variants (the same modes again), compressed H.264 (the encoder needs raw frames), a node that has
  no picture mode (the C920's `/dev/video1` is its metadata node: it does not list), a device already listed by the
  other provider. Mode order (`sort_modes`): the modes a person picks first (>= 15 fps) biggest picture first, motion-JPEG
  before raw at one size, faster first; the **slow ones (< 15 fps) after all the others** (a C920 raw 2304x1536 runs at
  2 fps and headed a 217-row list). *Why MJPEG first:* the camera's own compression is what makes its high modes fast
  (a C920 delivers raw 1080p at 5 fps, MJPEG at 30). Default mode (`default_mode`): the biggest picture up to 1280x720
  at >= 15 fps, MJPEG first, a rate up to 30 (60 fps of a face is waste): MJPG 1280x720@30 on the C920.
  **Checked against the device 2026-11-04:** the C920 lists 217 modes (MJPG 17 sizes x 7 rates = 119, YUYV 98) under both
  providers, identical to `v4l2-ctl --list-formats-ext -d /dev/video0` minus its H.264 formats (a one-off comparison, not kept as a script).
- **The saved choice** (`AppSettings::webcam`: camera name, mode as `MJPG 1280x720@30`, codec, bitrate, `#[serde(default)]`,
  old files load) is strings, so a camera that is unplugged or a mode it no longer offers falls back to the first camera
  and its default mode (`webcam::resolve`) **without overwriting the saved names**. A new camera resets the mode.
- **Encoder (D8): fixed** `Performance`, `TuneMode::LowLatency`, keyframe 2000 ms (`EncodeChoice::webcam`, shared with
  the screen's `EncodeChoice::screen` through `start_sharing_with_source`); only the codec and the bitrate (default
  2500 kbps, 200-12000) are the user's. The destination's `params.framerate` is the mode's rate, so the encoder never
  asks the camera for more than it gives (`RateCap` is a cap, never a floor, as for screens).
- **No audio rides with a webcam.** `RoomSession::sync_audio_watches` skips a webcam's watch when it picks the sender's
  audio to follow (`is_webcam_stream_in`), the tile has no volume row ("No audio in this stream"), and
  `owner_has_video` ignores webcams, so a sender with only a webcam and an audio share has that audio as an audio-only
  share with its own Listen button. *Why:* the sound belongs to the screen; a viewer who only watches the camera
  would otherwise get the desktop audio switched on with no control for it on the picture they watch.
- **One webcam at a time** (a camera opens once): `start_webcam` into another room stops the running one first and
  starts when no webcam capture is left (`webcam_start_after_stop`, `poll_webcam_start` runs in `poll_capture_states`),
  because the old pipeline still holds the device while it is `Stopping`. The same path restarts a webcam after a
  camera or mode change in the modal (a different capture); a codec or bitrate change is a live `edit_destination`.
  A room's reconnect keeps the capture and re-adds its destination like a screen's (`SuspendedShare::Video`).
- **A camera that goes away ends the share, with a toast; no restart loop** (`poll_capture_recovery_at` -> `end_lost_webcam`):
  `v4l2src` reports a source-side bus error (`CaptureErrors::mark_lost`), and a webcam is stopped like a user stop
  (destination released at once, the room told) with `errors.webcam_lost`. *Why not the screen's restart schedule
  (#99):* a portal comes back by itself within seconds and a restart can be silent; an unplugged camera does not, and
  nothing here could pick another camera for the user. `CaptureErrors::silence_is_loss` stays false for a webcam
  (only the portal's source resends frames on a static screen; a camera that is quiet for a while is not dead).
  Tested with a `videotestsrc` capture labelled `Webcam` and `inject_source_lost`
  (`session::ui_ops_tests::a_webcam_that_is_lost_ends_the_share_and_a_screen_is_restarted`); the real unplug was not done
  (the host's C920 stays plugged in).
- **Everything screen-specific ignores a webcam:** `share_state`, `Cx::live_rooms/my_phase/live_captures/on_air_rooms`
  skip `is_webcam()` captures, so the Share screen button, "Live in N rooms" and "Add X here too" never see it; the
  webcam has `Cx::webcam_share(room)`, and the sidebar's Outgoing popover lists it with a camera icon.
- **Rig hook `SSS_FAKE_CAMERA=1`** (off unless set, like `SSS_UI_FAKE_AUDIO`; `CLIENT_ENV_1="SSS_FAKE_CAMERA=1"` for
  `hs-rig.sh up`): `list_cameras` returns one "Test camera" (MJPG 1280x720@30, MJPG 640x360@30, YUYV 640x480@15, device
  `fake:ball`) and its source fragment is `videotestsrc is-live=true pattern=ball ! video/x-raw,..`, so the PIP checks have a picture that
  moves. The host's real C920 has its lens blocked: a black, noisy picture is the right result for it
  (device, modes and the stream path), and a camera without the hook lists the real device.
- **Packaging:** `video4linux2` (v4l2src; also the device monitor's native provider) is in `gst-plugins.txt` and
  `v4l2src` in `gst-elements.txt` (not rebuilt: the AppImage needs a container build); the Flatpak has `--device=all`
  (camera nodes); `check-deps.sh` warns about a missing `v4l2src` / `jpegdec` (webcam only, not fatal).

---
