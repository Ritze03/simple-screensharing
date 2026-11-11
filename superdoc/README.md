# simple-screensharing: documentation index

The agents' knowledge base for a friends screensharing platform: a native Rust/egui client, a Rust QUIC/WebTransport relay, a shared wire crate and a browser client. Start with [`architecture/overview.md`](architecture/overview.md) (module map, data flow, "where to look for X"). The rules for working on these docs are in [`../CLAUDE.md`](../CLAUDE.md) and [`claude-instructions/documentation.md`](claude-instructions/documentation.md). Pages are identified by date, not version (see [`claude-instructions/documentation-version-policy.md`](claude-instructions/documentation-version-policy.md)).

A page that was split keeps its old name as a hub: the hub lists its pages, and text moved verbatim may say "above" or "below" for a section that now sits on a sibling page.

## architecture/ - how the code fits together

- [Overview](architecture/overview.md): module map, process model, data flow, "where to look for X". **Start here.**
- [Session model](architecture/session-model.md): the client's rooms / captures / destinations model and the per-destination lifecycle (state machine, wire PTS, stall protocol).
- [Wire protocol spec](architecture/protocol-spec.md): the binding wire contract (channels, headers, encryption, NACK, previews, WebTransport leg).
- [Protocol decisions](architecture/protocol-decisions.md): why the wire looks the way it does.
- [WebTransport cert](architecture/webtransport-cert.md): the browser leg and the rotating pinned certificate, decisions.
- [Relay media forwarding](architecture/relay-media-forwarding.md): why the data plane avoids the room lock.
- [Client reconnect and watches](architecture/client-reconnect-and-watches.md): auto-reconnect, suspended rooms, stopped streams (#36, #40).
- [Implementation notes](architecture/implementation-notes.md): where the build diverged from the plan, status, the removed Electron era; index of the topic pages above.
- [Bug history](architecture/bug-history.md): bugs found by end-to-end and manual testing whose cause explains code that still exists.

## features/ - what the project does

- [Stream control](features/stream-control.md): hub of the sharing backend (capture, encode, fan-out), plus Auto-Connect. Pages in `features/stream/`:
  - [Capture sources](features/stream/capture-sources.md): gamescope, source resize, the picker, idle monitors, source errors.
  - [Capture stop and restart](features/stream/capture-restart.md): instant stop, source restart, keyframe redundancy, big-keyframe pacing.
  - [The capture pipeline](features/stream/capture-pipeline.md): one pipeline for N destinations, own-tile preview branch, 120-240 fps performance.
  - [Go live and destinations](features/stream/go-live-and-destinations.md): staging, fan-out, transfer, the three edit tiers, resume, retry.
  - [Encoder tuning](features/stream/encoder-tuning.md): presets, tuning controls, backend mapping, start-stream defaults.
  - [Encoder failures and freezes](features/stream/encoder-failures.md): live settings changes, coded-buffer and AV1 branch deaths, freeze root causes.
  - [Rate control](features/stream/rate-control.md): history of the modes, why encoding is always Dynamic.
  - [VA-API target-usage](features/stream/vaapi-target-usage.md): the bitfield on Mesa AMD and how it reaches the hardware.
  - [Video codec selection](features/stream/video-codecs.md): H.264 / H.265 / AV1 threading, availability probe, codec strings.
  - [Keyframes and bandwidth](features/stream/keyframes-and-bandwidth.md): keyframes bigger than the uplink, pacing hold, bandwidth modes.
  - [Webcam capture](features/stream/webcam-capture.md): the webcam as a stream of its own.
  - [Sharing audio](features/stream/share-audio-attachment.md): audio as an attachment on a share, and the 2026-07-28 audio bugs.
- [Audio sharing](features/audio-sharing.md): hub of audio capture, sync and levelling. Pages in `features/audio/`:
  - [Desktop audio](features/audio/desktop-audio.md): Desktop audio, audio-only shares, exclusion list, source groups, notification sounds.
  - [Levelling and volume](features/audio/levelling-and-volume.md): loudness levelling, sender boost, volume to 500 %, the share meter.
  - [Sync and clocks](features/audio/sync-and-clocks.md): viewer-side audio delay, two clocks, dither, pause/resume.
  - [PipeWire lifecycle](features/audio/pipewire-lifecycle.md): the private share sink, vanished and returning streams, switching source.
- [Video pipeline](features/video-pipeline.md): hub of the native viewer's video path. Pages in `features/video/`:
  - [Decode and display](features/video/decode-and-display.md): native decode, wgpu display, timing report.
  - [Playout buffer](features/video/playout-buffer.md): the viewer-side jitter buffer.
  - [Viewer recovery](features/video/viewer-recovery.md): L1 / L2 / L3 escalation.
  - [Selective retransmission](features/video/selective-retransmission.md): NACK from the viewer's side.
- [Website](features/website.md): hub of the browser client. Pages in `features/website/`:
  - [The stage](features/website/stage.md): tiles, control bar, layouts, chrome, tile fullscreen, stage tests.
  - [Website on a phone](features/website/phone.md): dropouts, decoder failures, supported phones, wake lock.
  - [Webcam, audio-only and picture-in-picture](features/website/webcam-and-audio.md): the browser's webcam and audio-only shares.
  - [Automatically watch new streams](features/website/auto-watch.md): the Playback option (on by default), the rule, why the join snapshot stays unwatched.
  - [Viewer features](features/website/viewer-features.md): sounds, emoji avatars, fullscreen, own-tile preview, who watches, above 60 fps.
  - [Join links](features/website/join-links.md): display name and room name in join links.
  - [Browser direct link](features/website/direct-link.md): `site#join=<link>` opens and saves a room; the Copy invite link context menu.
  - [NACK and big keyframes](features/website/nack-and-big-keyframes.md): selective retransmission and large keyframes in the browser.
  - [Harness and measurements](features/website/harness-and-measurements.md): measured numbers, running the harness.
- [Viewer window](features/viewer-window.md): watches and pop-out windows.
- [Own-tile live preview](features/self-preview.md): the sharer sees their own share, native and browser.
- [Relay transport](features/relay-transport.md): QUIC congestion control, datagram eviction, idle timeout.
- [Presentation modes](features/presentation-modes.md): the render vsync mode setting.
- [Windows client](features/windows.md): the Windows build and what differs from Linux.

## ui/ - the native egui client's screens

- [App shell](ui/APP-SHELL.md): window layout, sidebar, screen contract, behavioural rules.
- [Stage](ui/STAGE.md): the room view: tiles, layouts, people strip, control bar, menus, pop-outs, fullscreen, saved layout settings.
- [Share flow](ui/SHARE-FLOW.md): starting, stopping and recovering a share, the stream-settings modal, the screen-or-gamescope question.
- [Settings](ui/SETTINGS.md): the Settings modal, Boost, Level loudness, Playback sliders and auto-watch.
- [Module map](ui/MODULE-MAP.md): which file of `client/src/ui/` owns what.
- [Styling guide](ui/STYLING-GUIDE.md): tokens, widget helpers, the slot layout model, avatar colour, i18n.
- [HTML replica](ui/REPLICA.md): the design sandbox `ui/replica/index.html` and the notes taken from it.
- [Frontend shell](ui/FRONTEND-SHELL.md): history of the removed Electron/HTML frontend.

## testing/ - facts behind the testing rules

The rules are in `claude-instructions/testing-and-verification.md`; these pages hold the reference material.

- [Rig reference](testing/rig-reference.md): the headless-sway rig in detail and the reasoning kept from the old page.
- [GPU safety incidents](testing/gpu-safety-incidents.md): measured incidents behind the GPU rule.
- [Audio tests in the VMs](testing/vm-audio.md): running audio tests in the test VMs.
- [Test-writing notes](testing/test-writing-notes.md): global locale, "nothing reached the decoder" tests, scratch config.
- [Relay and website checks](testing/relay-and-website-checks.md): running the relay, checking the browser viewer.

## brainstorm/ - the original plans (history)

- [Platform brainstorm](brainstorm/simple-screensharing-platform.md): overall plan, scope and the first design decisions.
- [UI redesign and multi-room streaming](brainstorm/ui-redesign-multiroom-streaming.md): the 2026-07-26 plan behind the stream-control feature set.

## claude-instructions/ - mandatory rules for agents

- [Working with the docs](claude-instructions/documentation.md): read before you touch, update after you change, record the why.
- [Version policy](claude-instructions/documentation-version-policy.md): date-based, no version numbers.
- [Testing and verification](claude-instructions/testing-and-verification.md): verify against the real binary inside the headless rig; machine safety.

## meta/

- [Terminology](meta/TERMINOLOGY.md): project vocabulary.
- [Glossary details](meta/glossary-details.md): the long-form entries behind the terminology.
