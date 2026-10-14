# Presentation Modes

What the "Presentation mode" setting does, where it lives, and why it works the way it
does. Lives in `simple-screensharing-client`.

## What it does

The desktop client's window is drawn through `eframe`'s `wgpu` renderer, which by
default configures its `wgpu::Surface` with `PresentMode::AutoVsync` — every frame is
capped to the display's refresh rate (vsync-locked), with no way for the user to change
that. The presentation-mode setting exposes the real `wgpu::PresentMode` used to
configure that surface, so a user can trade vsync's no-tearing guarantee for lower
latency / an uncapped render rate if they want it.

Three modes are offered, matching `rooms::PresentationMode`:

| Mode | `wgpu::PresentMode` | Behavior |
| --- | --- | --- |
| Vsync (default) | `AutoVsync` | Capped to display refresh, no tearing. Today's implicit behavior before this feature existed. |
| Immediate (no vsync) | `Immediate` | No cap, submits as soon as a frame is ready. May tear. |
| Mailbox (no vsync, no tearing) | `Mailbox` | No cap, no tearing, via a single-slot swap instead of a wait-for-vblank queue. Falls back to `Fifo` (vsync) at `Surface::configure()` time on a backend that doesn't support it — no fallback logic needed on the app side, `wgpu` itself handles it. |

These three are the modes documented as supported everywhere; `Fifo`/`FifoRelaxed`/
`AutoNoVsync` aren't offered separately since they'd be redundant with the fallback
behavior of `Immediate`/`Mailbox`/`AutoVsync` above.

## Where it's stored

`rooms::AppSettings.presentation_mode: PresentationMode` — the same small TOML file as
the language preference, `$XDG_CONFIG_HOME/simple-screensharing/settings.toml` (see
`AppSettings::config_path`). Defaults to `PresentationMode::AutoVsync` so a user who
never touches the setting sees no behavior change from before this feature existed.

`main.rs` reads `AppSettings::load().presentation_mode` once at startup and passes
`PresentationMode::to_wgpu()`'s result into `NativeOptions.wgpu_options.present_mode`
before calling `eframe::run_native`.

## Where the dropdown lives

`ui::App::ui_settings_card`, a `theme::card`-wrapped settings section rendered by the
**Settings** nav tab (`ui_settings_tab`), right alongside the existing language toggle —
it's a global rendering setting, not per-stream or per-room, so it belongs somewhere the
user reaches regardless of which room they're in, not buried inside room-scoped
screen-sharing controls. *(Until the 2026-07-26 app-shell pass it lived at the top of the
old one-time join screen, for the same reason; that screen no longer exists — see
`superdoc/ui/APP-SHELL.md`.)*

Selecting a mode saves it immediately (`save_presentation_mode`), and — since it can't
take effect until the next launch (see "Why" below) — the row shows a small
"restart to apply" hint whenever the current selection differs from
`App::launch_presentation_mode` (the mode that was actually active when this window was
created).

## Why: true `wgpu::PresentMode` switching, not software repaint pacing

This project already has an *unrelated* pacing layer: `decode::PacingState` throttles
decoded video frames to the sender's capture-time PTS via `thread::sleep` on the decode
worker thread, upstream of rendering entirely. That mechanism controls *which frame*
gets shown when; it has nothing to do with *how fast the surface can present*, and this
feature doesn't touch it.

Before this feature, the client had no way to affect the actual GPU present cadence: the
`eframe` window's `wgpu::Surface` was always configured at the default `AutoVsync`, so
frame delivery to the screen was hard-capped at the monitor's refresh rate no matter what
the decode side did. The explicit design decision here (not re-litigated by future
changes without a real reason) is that this ceiling should be removable via a *real*
`wgpu::PresentMode` change on the render surface — decoupling redraw cadence from vsync
in software (e.g. just calling `request_repaint()` more often without changing
`PresentMode`) would not have removed that ceiling, since the surface's own `present()`
call still blocks/paces itself according to whatever `PresentMode` the surface was
configured with.

**Runtime swap vs. restart-required — what was actually found:** the pinned
`eframe`/`egui-wgpu` version (0.33.3, matching pinned `wgpu` 27.0.1) does not expose the
render surface for reconfiguration once the window exists. `egui_wgpu::Painter` (the
internal type that owns the `wgpu::Surface` per viewport) keeps its `surfaces` map and
`WgpuConfiguration` private, and the public API surface (`Painter::render_state()` /
`eframe::Frame::wgpu_render_state()`) only exposes the `wgpu::Adapter`/`Device`/`Queue`,
never the `Surface` itself or a method to reconfigure it. So there is no way to call
`Surface::configure()` again with a new `PresentMode` from application code — the
surface is only ever configured once, at `NativeOptions.wgpu_options.present_mode`,
before `eframe::run_native` creates the window. That's why this feature is
persist-immediately / apply-on-next-launch rather than live: it's a real constraint of
the pinned dependency version, not a design preference. If a future `eframe`/`egui-wgpu`
upgrade exposes surface reconfiguration, this should be revisited — swapping live would
be strictly better UX than requiring a restart.

Because the actual supported present modes for a given adapter/surface
(`wgpu::Surface::get_capabilities(&adapter).present_modes`) aren't reachable either
(same reason — no `Surface` handle available where the dropdown is drawn, before a
window/surface even exists yet at settings-load time), the three offered modes are
hardcoded to the well-known cross-platform-safe set (`AutoVsync`/`Immediate`/`Mailbox`)
rather than queried live. `Immediate`/`Mailbox` fall back to `Fifo` automatically at
`Surface::configure()` time if a platform doesn't support them, so this is safe even
without querying capabilities up front.

**Why the repaint-cadence focus gate exists alongside this:** removing the vsync
*ceiling* is not the same as removing the responsibility to stop rendering when there's
nothing to see. `ui::App::update` already drove a continuous `ctx.request_repaint()`
while a stream was being watched inline, uncapped by anything except the surface's own
`PresentMode`. Once `PresentMode` can be `Immediate`/`Mailbox`, that loop would otherwise
burn full GPU/CPU even while the window sits unfocused in the background — vsync used to
provide that ceiling essentially by accident, and this feature explicitly removes it. So
`update()` now checks `ctx.input(|i| i.viewport().focused)` and only requests the
full-rate repaint while focused; unfocused, it falls back to a coarser
`request_repaint_after(UNFOCUSED_REPAINT_INTERVAL)` cadence (500ms) regardless of which
`PresentationMode` is selected. This applies uniformly — an uncapped mode only relaxes
the render-rate *ceiling*, the focus check is what keeps a backgrounded window from
spinning the GPU for no visible benefit.

## Related docs

- `superdoc/ui/STYLING-GUIDE.md` — the dropdown and its "restart to apply" hint are
  built with the `theme.rs` widget helpers this doc's conformance pass introduced.
- `superdoc/architecture/overview.md` — module map entry for `theme.rs`/`ui.rs`.
