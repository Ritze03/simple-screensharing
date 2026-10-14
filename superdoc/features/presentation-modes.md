# Presentation Modes

What the "Presentation mode" setting does, where it lives, and why it works the way it
does. Lives in `client/` (`rooms.rs`, `main.rs`, `ui/modals.rs`).

## What it does

The client draws through `eframe`'s `wgpu` renderer. The presentation mode is the real
`wgpu::PresentMode` the window surface is configured with. Three modes are offered
(`rooms::PresentationMode`):

| Mode | `wgpu::PresentMode` | Behavior |
| --- | --- | --- |
| **Mailbox (default)** | `Mailbox` | No cap, no tearing: presents swap into a single-slot buffer instead of waiting for vblank. |
| Vsync | `AutoVsync` | Capped to the display refresh, no tearing. Was the default until 2026-10-14. |
| Immediate | `Immediate` | No cap, submits as soon as a frame is ready. May tear. |

`Fifo`/`FifoRelaxed`/`AutoNoVsync` are not offered separately; they would be redundant with
these three.

## Why Mailbox is the default (2026-10-14)

eframe presents its viewports **serially on one thread**, and with `Fifo` (what `AutoVsync`
resolves to) every present blocks until *that window's own* vblank. With a pop-out window open
and a stream faster than the monitor's refresh, the windows starve each other: measured on a
120 Hz monitor with a 240 fps stream, the **main window fell to 69 fps**. Mailbox never blocks
on vblank and held **240/240 fps in both windows**, and unlike Immediate it does not tear.
(Pop-outs: `superdoc/features/viewer-window.md`; renderer: `superdoc/features/video-pipeline.md`
"Native display".)

Only settings files **without** a stored mode get the new default: every save writes the whole
`AppSettings`, so an existing user's stored `AutoVsync` is kept.

## Where it is stored and applied

- `rooms::AppSettings.presentation_mode`, in the settings TOML
  `$XDG_CONFIG_HOME/simple-screensharing/settings.toml` (`AppSettings::config_path`), next to the
  language. Its own small enum rather than `wgpu::PresentMode`, so the persisted schema survives a
  wgpu upgrade that renames variants.
- `main.rs` reads it once at startup and hands `PresentationMode::to_wgpu()` to
  `NativeOptions.wgpu_options` (`wgpu_configuration`).
- **Restart to apply.** The pinned eframe/egui-wgpu (0.33.3, wgpu 27) keeps the surfaces and
  `WgpuConfiguration` private, so the surface can be configured only once, before the window
  exists. Selecting a mode saves immediately (`UiAction::SetPresentationMode`) and the dropdown shows
  a "restart to apply" hint while the selection differs from `launch_presentation_mode`. A real
  dependency limit, not a design preference: if a future eframe exposes surface reconfiguration,
  live switching would be strictly better.

## The dropdown

In the **Settings modal** (`ui/modals.rs`, `PRESENTATION_MODES`, locale keys
`ui.settings.mode_*`), beside the language select. It is a global rendering setting, not per
room or per stream. (Before the egui rebuild it was a card on the Settings nav tab; see
`superdoc/ui/APP-SHELL.md` for the current shell.)

## If the mode is unsupported: relaunch with Vsync

An earlier version of this page claimed wgpu falls back to Fifo for an unsupported
`Immediate`/`Mailbox`. **wgpu 27 does not**: it fails fatally with `UnsupportedPresentMode` at
surface configure. So for any mode except `AutoVsync`, `main.rs`'s adapter selector checks
`surface.get_capabilities(adapter).present_modes` while choosing the adapter (mirroring wgpu's own
power-preference ranking), sets `PRESENT_MODE_UNSUPPORTED` and fails the setup if the mode is
missing. `main` then **relaunches itself** (`exec`, because winit cannot build a second event loop
in one process) with `SSS_PRESENT_MODE_FALLBACK=1`, which forces `AutoVsync` for that run. The
stored setting is left alone.

## Repaint cadence is not the same thing

Removing the vsync ceiling means nothing throttles repaints any more, so the shell caps idle
polling itself: `request_repaint_after` of 50 ms focused / 500 ms unfocused
(`IDLE_REPAINT_INTERVAL` / `UNFOCUSED_REPAINT_INTERVAL`, `ui/mod.rs`). Video repaints are separate
and frame-driven (a waker thread per view, `video-pipeline.md` "Native display"), so a backgrounded
window with no video does not spin the GPU in any mode. `decode::PacingState` is unrelated: it
decides *which frame* is shown, not how fast the surface can present, and it is not on the display
path today.

## Related docs

- `superdoc/ui/STYLING-GUIDE.md` and `superdoc/ui/APP-SHELL.md`: the modal and its widgets.
- `superdoc/architecture/overview.md`: module map.
