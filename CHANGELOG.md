# Changelog

All notable user-facing changes, newest first. Categories:

**Added** (new features),
**Fixed** (bug and behaviour fixes),
**Removed** (things taken out),
**Info** (notes worth knowing).

## 2026-11-12

### Added
- **Pop-out stage windows keep their own dialogs**: modals, menus and toasts raised from a
  popped-out stage now open in that window instead of the main window (toasts follow the
  pop-out only if it took input in the last 2 seconds). Native app and website.
- **Crop pan on your own tiles**: Shift+drag also works on your own webcam and screen tile
  while its live preview is showing.

### Fixed
- **Grid shape**: Split and Crop now pick the grid from the stream count and the stage's
  orientation only (4 streams are always 2x2), so the layout no longer jumps between row
  counts.

## 2026-11-11

### Added
- **Hide when unfocused**: a new Layout menu checkbox hides overlays when the window loses
  focus; your own webcam preview never hides. Native app and website.
- **Automatically watch new streams**: a Settings > Playback option, on by default. Joining a
  room also starts watching the webcams that are already on. Native app and website.
- **Crop pan**: in the Crop layout, Shift+drag moves the picture inside its cell. Native
  app and website.
- **Website direct link**: right-click Copy invite link for a copy menu; a `#join=` link
  opens the website and saves the room.
- **Fedora / Nobara dependencies**: the README lists what to install on Fedora and Nobara.

## 2026-11-10

### Added
- **OBS virtual camera**: the Share dialog can share the OBS Virtual Camera next to
  gamescope; hidden device providers are now listed so it is found.

## 2026-11-07

### Added
- **Website on phones**: rooms drawer, bigger touch targets, long-press menus and a hint
  where the browser cannot share a screen.
- **Screen Wake Lock**: the website keeps a phone awake while you watch, share or are in
  a room.

### Fixed
- **Website fullscreen**: tile fullscreen no longer errors; a full-viewport fallback is used
  where element fullscreen is missing or rejected.
- **Website lost frames**: a frame lost whole is requested again and put back in order, so
  a gap costs a round trip instead of a keyframe wait.
- **Share survives a capture restart**: a destination that fails during a capture restart
  is kept, so the share no longer dies silently.

### Info
- **Connection log**: the website's Connection log shows repaired and unrepaired gaps and
  the full decoder failure line.

## 2026-11-05

### Added
- **Instant Share**: the Share button starts at once with your remembered settings; the
  Share menu still opens the settings dialog and live edits are remembered. Native app and
  website.
- **Share gamescope directly**: when a gamescope PipeWire node exists, the app asks whether
  to share the screen or gamescope.
- **Own avatar emoji picker**: 56 emoji in three groups, both clients.
- **Room settings by right click**: right-clicking a room in the sidebar opens its Room
  settings menu, both clients.
- **Connection reasons**: Settings > Connection shows why a watch or the session dropped.

### Fixed
- **Phone dropouts**: relay control reads are cancel-safe, which fixes dropouts on phones.
- **Preset highlighting**: a preset lights only on an exact FPS, encoding and keyframe
  match, and Preset sits above FPS.
- **Source resize**: a resized source swaps the encoder after a short delay, and a portal
  that stops delivering restarts quickly, without restart backoff for a plain resize.
- **Website broken frames**: a delta frame on a broken reference no longer reaches the
  decoder.

## 2026-11-04

### Added
- **Stage view**: the room centre and Watched streams are now a stage with Split, Crop and
  Floating layouts, resizable cells, a control bar, a people strip with hover cards, stage
  pop-out and auto-hiding fullscreen controls. Native app and website.
- **Presets**: Text, Quality and Gaming presets with a saved keyframe interval, in separate
  Compression and Video cards; you can change the source while live.
- **Webcam sharing**: share a webcam, with a webcam settings menu and a webcam in picture
  that snaps to eight spots. Native app and website.
- **Desktop audio and audio-only shares**: audio sources are grouped (Games, Browsers,
  Media, Voice chat, Devices), you can share audio on its own, and an exclusion list
  (Discord and TeamSpeak by default) keeps apps out. This app's own audio is never listed.
- **Emoji avatars**: pick any emoji as your avatar in Settings > Profile; others see it,
  including through the relay. Native app and website.
- **Notification sounds**: sounds when someone joins or leaves your stream or others' streams
  start or stop, with test buttons and a volume slider.
- **Settings tabs**: Profile, Interface, Playback and Diagnostics, native and website.
- **Layout menu**: Show stats and the other layout switches live in one menu and are saved
  across restarts; new defaults are Split with Webcam in picture on.
- **Level loudness button**: sits in the control bar next to the people button.
- **Own preview in HD**: your own tile preview now runs up to 1920x1080 at 30 fps.
- **Playout buffer**: the native client adapts a per-stream delay to the stream's recent
  lateness, feeding the audio delay, with a gentle catch-up.

### Fixed
- **Keyframe stutter**: keyframes are sent with deadline pacing and an adaptive pacing rate,
  which removes the lag spike when a keyframe is sent over a slow uplink.
- **AV1 encode failures**: a failed AV1 encode branch is rebuilt instead of reading as a
  lost portal source, and live settings changes on AV1 work.
- **Portal capture restart**: after repeated encode-branch failures the capture restarts
  instead of failing the share.
- **Same-name people**: two members with one name are two people, in both clients; the
  people strip shows each person once across rooms.
- **Encoder warning**: the "encoder can't keep up" warning is gone from the Share menu.

### Removed
- **Jitter-buffer settings**: the native jitter-buffer and catch-up sliders are gone, the
  playout buffer replaces them; old settings files still load.
- **Native Playback tab**: removed in favour of the control bar.

## 2026-11-03

### Added
- **Selective retransmission**: lost media fragments are re-requested (NACK) via a relay
  cache, in the native app and the website.
- **Sharer sees who watches**: the sharer gets the list of viewers of their stream.
- **Audio share by name**: the dropdown lists each name once and mixes every stream with
  that name into the share.
- **Viewer loudness levelling**: an on/off button next to every volume slider evens out
  loud and quiet sources, native and website; sender-side boost is capped at 200 %.
- **Fullscreen overlay**: the fullscreen stream gets the pop-out's overlay and hides it,
  with the cursor, after 3 seconds without input.
- **Logs in Settings**: Settings > Diagnostics > Logs has an always-on log with Copy to
  clipboard and Save to file.
- **Optional diagnostics upload**: an opt-in Settings checkbox sends client diagnostics to
  the relay.
- **Volume sliders**: scroll the mouse wheel on a slider (5 % steps, 10 % above 200 %); they
  apply live while dragging.
- **Smaller window**: the main window can be 800 px wide so it half-tiles on 1920 px
  screens, and the stat bar drops fields instead of clipping.
- **Browser certificate fallback**: the website falls back to the regular certificate
  check when a link's pinned certificate hash fails.

### Fixed
- **Big keyframes**: rate-based IDR pacing in native and browser sharers keeps big keyframes
  from choking the connection.
- **Shared audio**: it no longer garbles or goes silent after pause/resume, a source going
  idle and running again, or a stream that reappears after a gap; mute survives a source
  switch and reconnect; silent audio is true digital silence.
- **Audio meter**: the bar meter prefers the live share over a stopping one.
- **Portal start-up crash**: capture waits briefly before playing and repairs a portal
  capture that stalled after the start-up flap, avoiding crashes on many starts.
- **Pop-out survives re-creation**: it keeps working after a reconnect, relay restart or
  re-share.
- **Share card grid**: no empty columns and cards no longer shrink as the window grows.

## 2026-11-02

### Added
- **Volume up to 500 %**: the viewer volume slider goes to 500 % with a snap at 100 %,
  native and website.
- **Stronger transport**: the relay and client use BBR congestion control and tuned
  keep-alives, and keyframe datagrams are paced.

### Fixed
- **Stop and recovery**: Stop frees the room at the click and tears down fast; a lost
  capture source is restarted; keyframe redundancy after swaps.
- **Viewer recovery**: loss detection now escalates from keyframe request to decoder reset
  to reconnect, so streams recover after packet loss.

## 2026-11-01

### Added
- **A/V sync**: the viewer delays audio to match video latency, native and website.

### Fixed
- **Mesa AMD encoding**: Balanced and Quality use the right target-usage; Performance uses
  the fastest setting on other drivers.
- **Stream settings**: controls that do nothing for the encoder in use are hidden, in the
  native app and the share dialog of the website.

## 2026-10-29

### Info
- **Arch Linux dependencies**: the README lists the packages to install on Arch.

## 2026-10-24

### Added
- **Install via ritz-get**: the native client can be installed and updated with ritz-get;
  the README has a Linux quick start.

## 2026-10-15

### Added
- **Native egui desktop client**: the app is a native Rust/egui client in one repo with the
  relay, replacing the Electron app. It has a room-centric layout with a sidebar, action
  bar, stream settings, pop-out viewer windows and English and German (du-form) text.
- **Website client**: a browser client on GitHub Pages can watch and share, connecting to the
  relay over WebTransport with a pinned certificate hash in the join link; WebGL rendering
  copes with refresh rates above 60 fps.
- **Hardware decoding**: VA decoding for H.264, H.265 and AV1 on Linux, D3D11 on Windows,
  with GPU rendering of the video.
- **Windows build**: a GUI-subsystem exe with logging to a file, D3D11 decoders, and
  bundled GStreamer.
- **Flatpak and AppImage**: Flatpak release scripts and a Linux AppImage around the native
  client.
- **Join link with room name**: the join link carries an optional room name that pre-fills
  the add-room field; the relay address must include a port, with an inline hint.
- **Choose-your-name dialog**: asked while no display name is saved, native and website.
- **Own live preview**: your own tile shows a live preview of your share.
- **Double-click to fullscreen**: double-click a stream; avatar colours come from your name.
- **Auto-reconnect**: the native client reconnects after a dropped connection (Retry now or
  Cancel, with a banner and last-known roster) and leaves cleanly on exit.
- **Relay shutdown**: the relay exits promptly on Ctrl+C, SIGTERM or TUI quit, and tells
  clients when a join is rejected.
- **Hyprland support**: documented for the Hyprland desktop next to other compositors.
- **GPU capture**: capture stays on the GPU (DMA-BUF) with rate capping instead of padding.

### Fixed
- **Idle monitors**: sharing an idle monitor waits for its first frame instead of failing;
  cancelling the picker is silent.
- **Rotated monitors**: they arrive upright.
- **Watch ends with the sharer**: a watch ends when the sharer stops and resumes on
  re-share; AV1 padding is cropped to the announced size in both clients.
- **Capture start**: start-up is bounded and retried on a new portal session if the portal
  died, with no CPU fallback on a vanished portal node.
- **Duplicate saved rooms**: one saved room per join string; duplicates are migrated on load.
- **Window layout at 960 px**: the action bar and room header degrade to icons instead of
  clipping, and the stream settings dialog stacks to one column.
- **Rate control**: it is no longer configurable and is always dynamic (VBR); the
  encoder-limit banner is gone.

### Removed
- **Electron, Tauri and HTML frontends**: the old Electron shell, Tauri sidecar and the
  Windows Electron workflow were deleted.

### Info
- **License**: MIT.

## 2026-07-30

### Added
- **Electron app**: the desktop UI moved to an Electron shell with a sidebar, Stream Control,
  a Watch grid and a room-detail panel. It supports multiple rooms, create/join tabs,
  copy join link and a global display name.
- **Go live flow**: a staged share card with Preset and Advanced encoder tuning, then Go live.
- **Audio follows video**: viewers automatically hear whoever they watch, with per-tile
  volume and mute, an audio source picker with live level meters, and live source switching.
- **Pop-out viewers**: fullscreen or borderless viewer windows with the tile overlay, stats
  and volume.
- **Codecs and presets**: H.264, H.265 and AV1 selection, a Gaming preset, rate-control
  choices, tooltips, and capture framerates up to 240.
- **Live statistics**: live captured, sent and received FPS per stream, plus a network graph.
- **Packaging**: a self-contained Linux AppImage and a Windows build delivered as a GitHub
  Release.

### Fixed
- **Stalls and freezes**: fixes for viewer drift under sender overload, freezes on big FPS
  jumps, a freeze from an uncapped self-preview, and hangs on rapid Stop and Share cycles.
- **Audio sharing**: it shared the microphone instead of the app; fixed, and an audio-only
  stream can no longer exist without a live video stream.
- **Connecting**: a connect attempt times out after 15 seconds and can be cancelled.
- **Stuck "Stream error" label**: no longer survives a recovered stream.

### Removed
- **280 fps option**: 240 fps stays the ceiling.
- **Electron menu bar**: the default application menu is gone.

## 2026-07-24

### Added
- **First release**: a native Rust/egui desktop client with screen capture, encoding,
  decoding and audio, and a Rust QUIC relay with a ratatui admin TUI.
- **Room previews**: thumbnails of rooms' streams, plus pop-out viewer windows with F11
  fullscreen.
- **Languages**: English and German.
- **Windows build**: a Windows CI build with a D3D11 capture path.
- **Configurable framerate**: the encoder framerate is user-configurable with an FPS display.

### Fixed
- **Early stability**: fixes for leaked capture handles, hangs when stopping capture or
  audio, video freezes under motion and quality collapse at high framerates.
