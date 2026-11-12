# Changelog

All notable user-facing changes, newest first. Categories:

**Added** (new features),
**Fixed** (bug and behaviour fixes),
**Removed** (things taken out),
**Info** (notes worth knowing).

## 2026-11-12

### Added
- **Pop-out windows**: dialogs, menus and notices now appear in the pop-out window you
  are using.
- **Panning your own picture**: Shift+drag in Crop now also works on your own webcam and
  screen.

### Fixed
- **Stream layout**: the grid of streams no longer jumps around; four streams are always
  shown as 2x2.

## 2026-11-11

### Added
- **Hide when unfocused**: a new option hides the overlays when you switch to another
  window.
- **Watch new streams automatically**: new streams, and webcams already on when you join,
  open by themselves. You can turn this off in Settings.
- **Crop panning**: Shift+drag moves the picture inside its cell in the Crop layout.
- **Direct links on the website**: copy a link that opens the room straight away.
- **Fedora and Nobara**: the README now lists what to install.

## 2026-11-10

### Added
- **OBS virtual camera**: you can now share the OBS Virtual Camera.

## 2026-11-07

### Added
- **Phones**: the website is easier to use on a phone, and the screen stays awake while
  you watch or share.

### Fixed
- **Fullscreen on the website**: fullscreen no longer fails on some browsers.
- **Choppy video on the website**: lost video frames are now recovered faster.
- **Share stopping silently**: a share no longer ends without notice after a restart of
  the screen capture.

## 2026-11-05

### Added
- **Instant sharing**: the Share button starts right away with your last settings.
- **Sharing a game**: the app asks whether to share your screen or a gamescope game.
- **More avatars**: pick from 56 emoji.
- **Room menu**: right-click a room in the sidebar for its settings.
- **Connection info**: Settings now shows why a stream or your connection dropped.

### Fixed
- **Phone disconnects**: fixed random disconnects on phones.
- **Preset display**: a preset is only highlighted when your settings really match it.
- **Resizing a shared window**: sharing keeps working when the shared window changes size.
- **Broken picture on the website**: fixed corrupted video after lost data.

## 2026-11-04

### Added
- **New stage view**: watched streams are shown together in Split, Crop or Floating
  layouts, with a control bar and a strip showing who is there.
- **Presets**: Text, Quality and Gaming presets, and you can change the source while live.
- **Webcam sharing**: share your webcam, shown as a small picture in the corner.
- **Desktop audio**: share a game's or app's sound on its own, grouped by type, and keep
  apps like Discord out of it.
- **Emoji avatars**: choose an emoji as your avatar.
- **Notification sounds**: hear when people join, leave, start or stop streams.
- **Settings tabs**: Profile, Interface, Playback and Diagnostics.
- **Layout menu**: your layout choices are remembered between launches.
- **Loudness button**: evens out loud and quiet streams from the control bar.
- **Sharper own preview**: your own preview is now Full HD.

### Fixed
- **Lag spikes**: removed brief stutters when a stream refreshes.
- **AV1 sharing**: fixed shares that broke on AV1 or when changing settings live.
- **Capture failures**: sharing recovers by itself instead of failing.
- **People with the same name**: two people with one name now show up separately.
- **Misleading warning**: removed a wrong "can't keep up" warning from the Share menu.

### Removed
- **Jitter-buffer settings**: removed, the app now handles this itself.

## 2026-11-03

### Added
- **Better recovery from lost data**: streams recover from network glitches more smoothly.
- **See who watches**: the person sharing can see who is watching.
- **Audio by name**: sharing an app's audio now includes everything with that name.
- **Loudness levelling**: a button evens out loud and quiet streams.
- **Fullscreen**: the controls and cursor hide after a few seconds.
- **Logs**: Settings has a log you can copy or save for bug reports.
- **Optional diagnostics**: you can choose to send diagnostics to help fix problems.
- **Volume sliders**: scroll the mouse wheel on them; they react while you drag.
- **Smaller window**: the window can be narrower so it fits half a screen.

### Fixed
- **Big picture changes**: fixed stutter on sudden big picture changes.
- **Shared audio**: fixed crackling or silence after pausing, and lost mute settings.
- **Start-up crashes**: sharing starts reliably on more systems.
- **Pop-out windows**: they keep working after a reconnect.
- **Share cards**: they no longer leave gaps or shrink.

## 2026-11-02

### Added
- **Louder volume**: the volume slider now goes up to 500 %.

### Fixed
- **Stopping a share**: Stop works instantly and a lost source restarts by itself.
- **Streams after network trouble**: viewers recover by themselves.
- **Smoother connection**: the connection copes better with a weak network.

## 2026-11-01

### Added
- **Audio and video in sync**: sound is delayed to match the picture.

### Fixed
- **AMD graphics**: better quality and speed settings on AMD cards.
- **Stream settings**: options that do nothing are hidden.

## 2026-10-29

### Info
- **Arch Linux**: the README now lists what to install.

## 2026-10-24

### Added
- **Easy install on Linux**: the app can be installed and updated with ritz-get.

## 2026-10-15

### Added
- **New native app**: the app is rebuilt as a fast native program with rooms, a sidebar,
  stream settings and pop-out windows, in English and German.
- **Website version**: watch and share straight from your browser.
- **Hardware video decoding**: smoother playback with less load on your computer.
- **Windows**: a Windows version of the app.
- **Flatpak and AppImage**: easy packages for Linux.
- **Join links with room names**: links can include the room name.
- **Name prompt**: the app asks for your name if you have not set one.
- **Own preview**: see a live preview of your own share.
- **Double-click for fullscreen**: double-click a stream to go fullscreen.
- **Automatic reconnect**: the app reconnects after a dropped connection.
- **Hyprland**: now supported.

### Fixed
- **Idle screens**: sharing a screen that is not changing now works.
- **Rotated monitors**: they now appear upright.
- **Stream ends**: viewing stops when the sharer stops and resumes when they return.
- **Sharing start**: sharing starts more reliably.
- **Duplicate rooms**: the same room is no longer saved twice.
- **Small windows**: the app fits narrow windows without cutting things off.

### Removed
- **Old app versions**: the previous version of the app is gone.

### Info
- **License**: the project is now MIT licensed.

## 2026-07-30

### Added
- **New desktop app**: sidebar, stream control, a watch grid and room details, with
  several rooms at once.
- **Go live**: set up a share first, then go live.
- **Audio follows video**: you automatically hear who you watch, with per-stream volume
  and mute and a sound-source picker with level meters.
- **Pop-out viewers**: watch in a separate window or fullscreen.
- **Video options**: choose H.264, H.265 or AV1, a Gaming preset and up to 240 fps.
- **Live stats**: see the frame rate of each stream.
- **Downloads**: a Linux AppImage and a Windows version.

### Fixed
- **Freezes and lag**: fixed several freezes and growing delays while watching.
- **Audio sharing**: it shared the microphone instead of the app.
- **Connecting**: a connection attempt now gives up after 15 seconds and can be cancelled.
- **Stuck error message**: a "Stream error" label no longer stays after recovery.

### Removed
- **280 fps option**: 240 fps is the maximum.

## 2026-07-24

### Added
- **First release**: a desktop app for sharing your screen and sound with friends, plus a
  relay server.
- **Room previews**: see small previews of what is being shared.
- **Pop-out windows**: watch in their own window, with F11 for fullscreen.
- **Languages**: English and German.
- **Windows**: a Windows version.
- **Frame rate**: choose your frame rate.

### Fixed
- **Early stability**: fixed hangs, freezes and quality drops.
