# App Shell (native egui client)

Rewritten 2026-10-14. How the desktop client's one window is laid out, which module owns
which region, and the behavioural rules and design reasons behind it. Code:
`client/src/ui/` (screens), `client/src/widgets/` (layout + controls),
`client/src/theme.rs` (tokens). How controls are built and styled: `STYLING-GUIDE.md`.
What the UI sits on top of: `superdoc/architecture/overview.md`.

The client's UI is **one native egui app**. It replaced an Electron + HTML/CSS/JS frontend
(and, before that, a first egui UI with a Stream Control / View / Settings nav rail); both
are history, see the end of this page and `FRONTEND-SHELL.md`.

## Design

**Room-centric, Discord-like.** You pick a room in the left sidebar; the centre shows that
room; everything you can do about sharing is scoped to the selected room. The layout was
approved by the user as an HTML mockup (https://claude.ai/artifact/5yA1iHf1YnXqmABkZajCe4,
its "Proposed" tab).

```
┌ sidebar 272 ────────┐┌ room header band 56: title, status pill, meta | Auto-connect,
│ brand band 56       ││   Copy invite link, Leave, ⋯                                    │
│ Watched streams     │├─────────────────────────────────────────────┬─ People 232 ─────┤
│ ROOMS (rooms well,  ││ body (scroll, pad 16, gap 16):              │ members, sharers │
│  sharer sub-rows)   ││  "On screen" tiles (16:9, GPU video)        │                  │
│ Create or join      ││  "Sharing in this room" cards               │                  │
│ user band 56        │├─────────────────────────────────────────────┴──────────────────┤
└─────────────────────┘│ action band 56 (PANEL_SUNKEN), room-scoped                      │
                       └────────────────────────────────────────────────────────────────┘
```

- Left sidebar **272 px**; room header band **56**; People **232 px**; bottom action band
  **56**. The user band (sidebar bottom) and the action band share one top border line.
- **Connecting vs Reconnecting** (`ui::ConnState`: `Joined`, `Connecting`, `Reconnecting`,
  `Disconnected`). `Connecting` is a room's *first* connect in flight (sidebar/header pill
  "Connecting"); `Reconnecting` is only a suspended room (TERMINOLOGY) that the lib is
  retrying (`AppState::reconnect_info`). Both show the panels and a "stop connecting" Leave.
  *Why:* the first Connect of a saved room used to read "Reconnecting" (stage-6 QC).
  A failed Connect (`start_connect` error) raises an error toast and the room's Disconnected
  view shows an inline warning banner above the empty state, text
  `ui.room.connect_failed` ("Couldn't connect: <error>"), cleared on the next Connect.
- **Share labels** are translated at draw time by `ui::share_label(raw)`: the default capture
  label and the wire default "Screen" (in any locale) render in the current language, other
  names (audio sources, peer-chosen labels) as is. The lib stores the label translated at
  creation, so without this a language switch would leave old shares in the old language.
- The People panel and action bar exist only while the shown room is joined or
  reconnecting (`Cx::shows_room_panels`); a disconnected room, the "Watched streams" view
  and the welcome view have neither.
- **The Stream Control / View / Settings nav rail is gone.** Settings is a modal opened
  from the user band's gear; sharing lives in the room's action bar; watching is the
  room's tiles.
- **Stream settings open in a modal**, not a side panel. *Why:* the user said the side
  panel was too squished for the number of controls.
- **Leave sits in the room header.** *Why:* user's call; it is a room-level action, not
  a sharing one, so it is not in the (sharing) action bar.
- **Narrow windows (960 px minimum → ~656 px header / action band):** nothing clips;
  controls degrade in a fixed order instead. Action bar: the status text truncates down
  to 72 px first, then buttons go icon-only (label in the tooltip): Live = Stream
  settings → `N rooms ▾` to `N ▾` → audio compact → Stop sharing; Staged/Picking =
  Stream settings → audio → Discard → Go live. Header: Copy invite link icon-only →
  meta line dropped whole (never "2 pe…") → Leave icon-only → bare Auto-connect checkbox.
  *Why:* redundant controls go first (Stream settings is on the own card, Copy invite
  link in ⋯, the meta counts in People); the primary action keeps its words longest.
  Details in the `action_bar.rs` module doc and `room::show_header`.
  Body and modal: the stream-settings modal stacks its two columns into one when a
  column would be narrower than its widest segmented control / Screen readout needs
  (measured from the current locale's labels, so it is one column at 960 and two at
  1280+ in en and de). The Preset and Codec rows size their segments to their labels
  when equal segments would truncate, so German's "Niedrige Latenz" no longer forces
  one column at every width (#47: four equal Preset segments needed a 585 px column,
  more than the 1120 px modal's ~534 px half). The reconnect banner moves its buttons under the text when the
  text column beside them would be under 320 px; empty states wrap, centred.
  *Why measured, not a fixed breakpoint:* German labels run ~20 % longer, so a fixed
  width either stacks English for nothing or still truncates German. *Why the banner
  keeps its words:* Retry now is its whole point. Details in `stream_settings.rs`
  (`column_need`) and `room::reconnect_banner`.
- **Own-tile live preview** (#78, user request 2026-10-15): the own card in "Sharing in this room"
  shows a live <= 640x360 / <= 30 fps picture of what is captured **while the window is focused**
  (`LIVE`/`STARTING` chip left, `PREVIEW` chip right); unfocused, minimized or with the tile not
  painted it is the hatched "Your screen" placeholder and the capture's preview branch is closed.
  *Why:* "show a stream preview while tabbed into the program; otherwise fine to hide it". It replaces
  the earlier deliberate "no self-preview". Code: `room::my_card`, `App::poll_live_previews`,
  `UiAction::LivePreviewSeen`; design in `superdoc/features/self-preview.md`.

### Layout implementation rule

Every region is built from explicit nested vertical/horizontal containers with fixed-size
slots (`widgets::HBox`, row helpers, `card`/`well`/`band`/`grid_rows`/`modal_shell`); never
`ui.horizontal` + `add_space` for rows. The shell itself is egui `SidePanel` /
`TopBottomPanel` / `CentralPanel` at the fixed sizes above. Full rule and rationale:
`STYLING-GUIDE.md`. *Why panels and not spacers for the shell:* "this region is fixed, that
one takes the rest" is exactly what panels express; hand-computed `add_space` fillers would
re-derive the split every frame and silently break when content height changes.

## Module map (`client/src/ui/`)

| File | Owns |
| --- | --- |
| `mod.rs` | `App` (eframe app: polls the session, lays out the panels, runs modals/toasts, applies actions); `View` (`Welcome` / `Watching` / `Room(RoomKey)`); `Modal`, `PopoverKind`/`Popover`; `Nav { view, modal, popover }`; `Shell` (staged shares, toasts, create/join results, level taps); `UiAction` (every cross-screen action); `Cx`; selectors (`room_entries`, `ShareState`, `same_room`, ...); the shared `tr` / `tr_with` i18n fallback helpers |
| `apply.rs` | `App::apply`: the **only** place a `UiAction` changes state. Thin calls into the lib (`session.rs`) plus nav/shell bookkeeping |
| `video.rs` | `VideoViews`: one GPU `VideoView` per watched stream, keyed by `StreamKey = (RoomHandle, ConnId, StreamId)`, synced once per frame; `popped` flag + `close_requested` for pop-outs |
| `sidebar.rs` | brand band, body (Watched streams well, ROOMS heading, rooms well with sharer sub-rows, Create-or-join), user band (avatar, "Live in N rooms" / "Not sharing" -> Outgoing popover, settings gear) |
| `room.rs` | `show_header`, `show_body` (reconnect banner, "On screen" tiles, "Sharing in this room" cards, empty states), `show_people` |
| `action_bar.rs` | bottom band, one layout per `ShareState`: Idle / Picking / Staged / Live / Paused; owns the share-menu, rooms and audio-source popovers |
| `stream_settings.rs` | the stream-settings modal (staged "Set up your stream", live "Stream settings"), width 1120 |
| `modals.rs` | create/join, app settings, leave / forget confirm, rename, choose-your-name |
| `popout.rs` | pop-out viewer windows: one deferred viewport per popped stream |
| `tests.rs` | session-model lookups, bandwidth wiring, shell geometry (`shell_regions_match_the_spec_sizes`), nav/action behaviour |

### The screen contract (data in, actions out)

A screen module owns one `<X>State` (its private UI state: text buffers, edit copies,
hover) and `pub fn show...(ui, cx: &mut Cx, st)` functions. It reads everything via `Cx`
(`&AppState` read-only, `nav`, `shell`, `videos`, plus selectors) and **never mutates
`AppState`, `Nav` or `Shell`**; every change is `cx.act(UiAction::..)`, applied by
`App::apply` after all screens have drawn (visible next frame). Local edit state is pushed
as one action when it should take effect (e.g. on `drag_stopped`, not every frame of a
drag). *Why:* the borrow checker can't hand four screens `&mut AppState` while they also
read it, and a single apply site keeps every state transition in one reviewable place.

The shell hands each screen a `Ui` that is exactly its region; a screen must not paint
outside it.

### Popovers, modals, toasts

At most one popover (`Nav.popover`, anchored to the button's rect) and one modal
(`Nav.modal`). Opening a modal closes the popover; Escape closes the popover before a
modal; Escape, x and a backdrop click close every modal. Toasts are painted last by
`mod.rs` (2.6 s).

**Settings > Diagnostics > Logs: copy or save the app's recent log** (2026-11-03, task #107, user: "The GUI should
have an option in the settings to dump logs to the clipboard or a file, so investigation is easier."). Two
secondary buttons in an own "Diagnostics" card (`ui::modals::settings`): **Copy to clipboard**
(`UiAction::CopyLogs`: `ctx.copy_text(logbuf::dump())` + "Logs copied" toast) and **Save to file...**
(`UiAction::SaveLogs`: `logbuf::save_to_downloads()` writes `simple-screensharing-<UTC stamp>.log` into the user's
Downloads folder, home folder if there is none; a toast plus an info banner in the card with the full path,
`Shell::saved_logs`). The dump is a short header (version + commit hash from `client/build.rs`, OS, GPU from the
wgpu adapter, VA kernel driver once a share has probed it, time, line count) and then the ring's lines, oldest
first. The ring (`client/src/logbuf.rs`) is a second `fmt` layer next to the stderr one (`main.rs` `init_logging`):
last 5,000 events / 2 MB, **INFO+ from `simple_screensharing_client`/`..._protocol`, WARN+ from everything else**,
independent of `RUST_LOG` (which still only filters stderr / the Windows `client.log`). Same UTC timestamps as
stderr. A panic hook now logs `panic: ...` on every platform (it used to be Windows release only) and then runs the
default hook. *Why in memory, always on:* the old advice ("restart from a terminal with `RUST_LOG=info ... 2>
sss.log`") lost the first occurrence of a stall. *Why Downloads and no file dialog:* no dialog crate is a dependency
and a native one would pull a portal client / GTK in for one button. *No secrets:* no log line prints a room key or
join link (grepped 2026-11-03); `logbuf::redact` still turns any `server|room|key...` token into
`server|room|<redacted>` before a line enters the ring. Rig note: `wl-paste` only sees the copy when the headless
sway seat has a keyboard (hold one open with `wtype -P Shift_L -s 30000 -p Shift_L` while clicking; the existing
"Copy invite link" behaves the same). Tests: `logbuf::tests::*`,
`ui::tests::copy_logs_puts_the_dump_on_the_clipboard_and_toasts`.

**"Live in <room>" toast fires when a room goes on air, not on Go live** (2026-10-15, task #73).
`App::poll` compares `Cx::on_air_rooms()` with `Shell.announced_live` (`newly_on_air`): a room
that is only Starting, or whose start failed (the destination is forgotten and the error toast
shown), never gets it. *Why:* it used to fire inside `UiAction::GoLive`, so a share that failed to
start showed "Live in Test" above the "Share error" toast (the user's WinBoat run, a missing
GStreamer element). Test: `live_in_toast_fires_once_per_on_air_spell`.

**Window icon** (2026-10-15): `ui::app_icon()` (`client/assets/icon.png`, the `docs/assets/icon.svg`
favicon rasterised once to 256 px with `rsvg-convert`) is set with `with_icon` on the main and the
pop-out viewports. *Why:* on Windows (and X11) eframe's default "e" showed in the title bar and
taskbar. Wayland takes the icon from `app_id` + the .desktop file, so it is harmless there. The
exe file itself (Explorer, shortcuts) has no embedded icon resource: that needs a build-time
resource compiler (`winresource`/`embed-resource` + an `.ico`), not added.

**The stream-settings modal shows only controls that change the encode** (2026-11-01, task #90, user:
"Keep the [Tune] setting. But make sure to not show settings, if they wouldn't do anything!").
The backend that will encode is `capture::detect_encoder_backend()` (probed once per process: VA-API if
`vah264enc` reaches PAUSED, else software x264; a destination's `VideoParams::backend` is that same value, so
staged and live agree). Against it:
- **Tune** is drawn only on x264 (`EncoderBackend::tune_applies`, was `presets::tune_applies` until #95): `launch_fragment` maps it to `tune=zerolatency`
  there, and on every VA encoder (`vah264enc`/`vah265enc`/`vaav1enc`) it is dropped (`let _ = tune;`).
  On VA-API the Advanced rows are Quality level + Keyframe interval.
- **Codec** is drawn only when `supported_codecs` offers more than one (`presets::codec_choice_offered`).
  Software x264 is H.264 only, so the row used to be H.264 plus two greyed-out buttons; it is gone there
  (VA-API keeps all three, unsupported ones are not a thing on that backend).
- Everything else was audited and is effective on both backends: Framerate (`RateCap`), Bitrate, Quality
  level (`speed-preset` / `target-usage`), Keyframe interval (`key-int-max`), the bandwidth rows, the presets.
- The browser share modal (`docs/js/ui/app.js`) lost its "Quality level: desktop app only" row (WebCodecs has
  no such knob), and hides the Codec row when the probe finds a single encodable codec. (Its Tune and
  Keyframe interval readouts stayed at first; #93 hid them, see the next block.)
*Why Tune stays at all instead of being hard-wired to zerolatency:* the user judged x264's ~0.7-1 s lookahead
latency on "Best quality" acceptable ("It's screensharing, not remote gaming"), so the choice stays with the
user on the one backend where it matters. *Why hide, not grey out:* the user said "not show".
*Preset highlighting:* picking a preset still writes all three fields (tune included), so an x264 fallback
or a settings file used on another machine encodes as the preset says; but `EncoderPreset::detect_for`
ignores the stored tune where it does nothing, otherwise a stale hidden tune (e.g. Low-latency from an older
run, with Balanced's level and GOP) would leave every preset un-highlighted for a reason the user cannot see
or fix. The four presets differ in (quality level, GOP), so the comparison stays unambiguous. On VA-API
Low-Latency and Gaming differ only in the keyframe interval; the preset set was left as is (until #95, below,
which hides the twin presets). The known edge of #90, a live VA branch that fell back to x264 still hiding
Tune, is fixed by #95 (the backend is now per destination). Tests: `presets::tests::detect_for_*`,
`stream_settings::tests::tune_row_is_only_drawn_where_it_does_something`.

**Hide anything that does nothing, even when it is only a browser limitation** (2026-11-01, task #93).
User rule, verbatim: "If a setting doesn't do anything, hide it (Yes, even when its just a browser
limitation). Just hide it, so other platforms can still use it." So the rule is: *hide in the UI of the
platform where it is a no-op; never delete the setting from the shared data, persistence, protocol or
the other platforms' UI*, and a stored value this platform cannot honour must round-trip unchanged.
*Why hide, not delete:* the same settings file / join flow is used by the native app and the browser;
the setting still means something elsewhere. *Why hide, not grey out or show read-only:* a greyed or
read-only control still looks like a setting and invites the question what it is for (#90 said "not
show"; #93 extended it to the readouts and the disabled buttons).
Applied to the browser share modal (`docs/js/ui/app.js`):
- **Low-Latency preset button hidden.** In the browser `ENCODER_PRESETS.low_latency` equals `balanced`
  (`latencyMode: "realtime"`, 2000 ms keyframes), so the button did nothing different. Balanced is the
  default and Gaming (750 ms) is the low-latency choice. A stored `"low_latency"` is *displayed* as
  Balanced (`PRESET_TWIN`) and is not rewritten unless the user picks another preset; the core's
  `ENCODER_PRESETS` and `startShare({ preset: "low_latency" })` are unchanged.
- **Tune and Keyframe interval rows hidden, and the "Advanced tuning" disclosure with them** (nothing
  was left in it). They were read-only readouts of what the preset sets; the preset row already shows
  that choice. The "follows the preset" hint went too.
- **Unencodable codec buttons hidden** instead of greyed out ("This browser can't encode ..."); the Codec
  row is still dropped when one or none is left. If the stored default codec is not encodable the
  staged modal starts on the first encodable one (staged copy only; the stored default is untouched).
- Unused i18n keys removed in en and de: `ui.stream.advanced`, `tune`, `tune_low_latency`,
  `tune_best_quality`, `keyframe_interval`, `advanced_browser_note`, `codec_unavailable`,
  `ui.preset.low_latency`. (The native client's own `ui.preset.*` keys are in `client/locales`, untouched.)
- Kept on purpose (information, not settings, and they act in the browser): the Screen and Audio source
  rows with the level meter, the Bitrate "set by budget" value in absolute-bandwidth mode, the framerate
  select (the encoder is told the rate and excess frames are skipped), the bandwidth rows, jitter/catch-up.

**Native client: hide twin choices and per-destination backend** (2026-11-01, task #95, same user rule as #93:
"If a setting doesn't do anything, hide it (Yes, even when its just a browser limitation). Just hide it, so
other platforms can still use it." then "Make sure to fix all according to spec."). Hide, never grey out and never
delete: every stored value round-trips unchanged, so a settings file moved to another driver or platform still
works. What "does nothing" means is now computed from the *effective encode* of the backend that is running,
`capture::EncoderProfile { backend, mesa_amd }` (`EncoderProfile::of(backend)` probes the VA driver once; the
unit tests build either mapping by hand):
- **Quality level: Balanced hidden on Mesa AMD.** `vaapi_target_usage` gives Performance 2 / Balanced 3 / Quality 4
  there and Mesa reads target-usage as a bitfield where 2 and 3 are bit-identical ("balance" preset, #92), so the
  Balanced button was a second Performance. `EncoderProfile::quality_effect` folds the value the way the driver does
  (Mesa AMD: 1 alone, {2,3}, {4,5}, {6,7}; spec-order drivers and x264: every level distinct), derived from
  `vaapi_target_usage` itself, not from a hard-coded "AMD hides Balanced", so it follows a changed mapping.
  `visible_quality_levels()` keeps Performance and Quality ahead of Balanced when levels are twins. A stored
  Balanced is *displayed* as its twin (`display_quality_level`, Performance) and stays stored as Balanced until the
  user picks another level (clicking the shown, already-selected one is no change). Intel (7/4/1) and x264
  (ultrafast/medium/veryslow): nothing hidden.
- **Presets: Low-Latency hidden when it equals Balanced.** `EncoderPreset::visible_for(profile)` hides a preset
  whose *effective* tuning (`EncoderTuning::effect`: tune only where `tune_applies`, `quality_effect`, GOP) equals
  one already shown; Balanced (the default) wins, the rest win in display order. On Mesa AMD Tune is a no-op and
  Performance = Balanced, so Low-Latency (low-latency, Performance, 2000 ms) = Balanced (best-quality, Balanced,
  2000 ms): hidden, matching the browser (#93 `PRESET_TWIN`). Gaming (750 ms GOP) stays. `detect_for(tuning, profile)`
  compares effective settings, so a stored Low-Latency highlights Balanced on AMD, stays Low-Latency on Intel and x264,
  and the stored fields are untouched until a preset is picked (picking one still writes all three fields).
  Why derived and not a table: the answer differs per driver and per destination (below).
- **Tune and the twins follow the destination's running backend.** `Destination::encoder_backend()` is the backend
  its live branch was built on (`DestinationRuntime::actual_backend`, recorded by `spawn_destination`,
  `swap_to_new_branch` and `rebuild_after_source_restart` from `EncodeBranch::backend()`), the process-wide probe
  while no branch is up, and the probe for the staged modal. So a destination whose VA branch fell back to x264
  (`add_branch_with_fallback` or the no-IDR retry) shows Tune and every level and preset that differs on x264.
  *Why `params.backend` is untouched:* it is the *preferred* backend, tried first again on the next branch swap; a
  transient VA failure must not pin the destination to x264 for good, so the actual one is a separate field.
- **Codec buttons the machine cannot encode are hidden, not greyed out.** `capture::supported_codecs(Vaapi)` was a
  static "H.264, H.265, AV1"; it now offers H.265/AV1 only when GStreamer has `vah265enc`/`vaav1enc` (the VA plugin
  registers an encoder only for a profile the GPU's driver advertises), so a GPU without AV1 encode no longer shows
  an AV1 button that silently produced x264 H.264. A stored codec that is not encodable here is shown as H.264, the
  stream's real codec, and stays stored.
- **Settings > Presentation, fallback run.** When the surface lacks the stored present mode, `main` relaunches on Vsync
  for that run (`SSS_PRESENT_MODE_FALLBACK`). In that run the stored mode (`launch_presentation_mode`) does nothing,
  so it is hidden and Vsync is shown as selected; the stored value is kept and re-picking Vsync is no change
  (`ui::modals::presentation_choices`). Normal runs show all three.
- **Audit result (native, per backend/codec/driver):** Framerate (RateCap, every backend), Bitrate (every backend), Keyframe
  interval (`key-int-max`, every VA codec and x264), bandwidth mode/total/floor (floor and total only drawn in Absolute
  mode, where they act), the Codec row (hidden with one codec), Settings: display name, language, jitter buffer and
  catch-up speed (playback, act on every decoder) were checked and all do something; the Advanced disclosure stays
  because Quality level and Keyframe interval are always in it. Not changed on purpose: the Source card's audio
  controls (they act on every platform). Known remaining: Gaming vs Low-Latency on VA differ only by GOP (a real
  difference, kept); the modal's two-column width still reserves room for the longest label set (`column_need_for`
  measures all options, so hiding some never makes the layout jump).
- Removed the now unused `.collapser` rule from `docs/css/app.css` (the browser stopped drawing it in #93; the egui
  `widgets::collapser` is unrelated and stays).
Tests: `presets::tests::{mesa_amd_hides_the_low_latency_twin, spec_order_va_shows_every_preset, software_shows_every_preset,
detect_for_follows_effective_settings, quality_levels_hide_the_balanced_twin_on_mesa_amd,
spec_order_mapping_has_no_twins_and_amd_mapping_has_one_pair}`, `capture::destination::tests::actual_backend_is_recorded_apart_from_the_preferred_one`,
`ui::tests::session_model::a_destination_reports_the_backend_its_branch_runs_on`,
`ui::modals::tests::presentation_choices_hide_the_mode_the_surface_lacked`, and the rig runs in
`superdoc/features/stream-control.md` (2026-11-01, #95).

**The stream-settings modal keeps one height whether "Advanced tuning" is open or closed**
(2026-10-15). While collapsed, `encoder_card` reserves the rows' height as empty space *below*
the Encoder card (`adv_rows_height`: the rows laid out once in an invisible, non-advancing child
just to read their height), so the modal is as tall as with Advanced open (still capped by the
window; the body scrolls when it is short). Test: `encoder_card_height_does_not_depend_on_advanced`.
*Why:* the user disliked the modal growing and shrinking, and, being centred, jumping, when
toggling Advanced. A measured pass beats remembering the expanded height (no one-frame resize
the first time the modal opens closed) and beats a fixed constant (follows font and language);
the space sits outside the card so it reads as empty body, not an oversized card. It only
changes the height when the right column is the taller one (it is, in the staged modal).
Not covered (can still resize it): the encoder-limit and bandwidth-overshoot banners (not
checked in the rig; the left column has ~90 px slack in the staged modal).

### Video

`VideoViews` (`cx.videos.show(key, ui, size, Fit::Contain)`) draws a GPU `VideoView`
(`client/src/decode/render.rs`): NV12 decoded frames are uploaded as two textures and
converted to RGB in a WGSL shader through an `egui_wgpu` paint callback. **Repaint is
frame-driven**: each view runs a small waker thread that requests a repaint whenever the
decoder publishes a frame, so screens never call `request_repaint()` for video. Presentation
mode defaults to **Mailbox** (uncapped, no vsync ceiling); because of that the shell caps
idle polling at 50 ms focused / 500 ms unfocused (`IDLE_REPAINT_INTERVAL`,
`UNFOCUSED_REPAINT_INTERVAL`). Measured: **240 fps presented on DP-1** (279.75 Hz).
Presentation mode can only be set at surface creation, so a change in Settings applies on
the next launch (`superdoc/features/presentation-modes.md`).

Pop-out: the view sits in an `Arc<Mutex<VideoView>>` so the deferred viewport closure
(`Send + Sync + 'static`) can paint the same view the tile would; while `popped` the tile
shows a placeholder. The pop-out has hover-gated chrome (statbar, audio pill; volume 0-500 % with a snap at 100 %, task #96, `features/audio-sharing.md`), F11
fullscreen, and Escape only *leaves* fullscreen, never closes (the former Electron viewer's
rules, carried over; `superdoc/features/viewer-window.md`).

**Double-click a watched tile = fullscreen** (2026-10-15, task #77). `room::tiles` registers a
`Sense::click` over the tile's video (before the hover overlay, so the overlay's buttons keep their own
clicks; a single click still does nothing) and a double-click pushes `UiAction::EnterStreamFullscreen(key)`.
`apply` records `Nav.fullscreen = Some(FullscreenStream { key, was_fullscreen })` (was the window already
fullscreen?) and sends `ViewportCommand::Fullscreen(true)`. While `Nav.fullscreen` is set, `App::draw` paints
*only* the stream (black `CentralPanel`, `VideoViews::paint(.., Fit::Contain)`, the same paint as a pop-out:
no sidebar, bands, modals or toasts; pop-out viewports are still declared). A double-click on it or Escape
(handled in `App::update`) pushes `ExitStreamFullscreen`, which sends `Fullscreen(was_fullscreen)`, so a window
that was already fullscreen stays fullscreen. If the stream disappears (unwatched, ended, popped out) the next
`draw` leaves the mode by itself. A popped-out tile is a placeholder and is not eligible. Inside a pop-out
viewport a double-click toggles that window's fullscreen like F11 (`popout.rs`, registered before the chrome).

**The fullscreen stream carries the pop-out's overlay** (2026-11-03, task #106): `App::draw` calls
`popout::fullscreen_overlay` after the double-click `interact`, which runs the *same* `popout::chrome`
(hover-gated vignette, statbar `[dot][sharer · room][WxH][dec/pres fps][bitrate]`, audio pill or "No audio
shared" chip) and the same always-visible centre chip (waiting / recovering / sharer stalled), and shows
it while the pointer is anywhere in the window. Differences: the top-right round button is
`fullscreen_exit` ("Exit fullscreen", pushes `ExitStreamFullscreen`) instead of the pop-out's `close`
("bring back to its tile"; pop-out-only), and volume edits go straight out as `UiAction::SetVolume`
instead of through `Shared::pending_volume` (the data is collected per frame with `collect_info`, no 4x/s
refresh). *Why:* user request 2026-11-03: "the same overlay, that a fullscreened pop-out has, should be
there when i fullscreen a screenshare inside of the main window". One `chrome` function serves both so
the two cannot drift apart.
*Why:* user request 2026-10-15 ("double clicking a share should actually full-screen it"). *Why a mode of
the main window and not a new viewport:* the pop-out already exists for "own window"; this is the quick
"fill my screen" gesture and must restore the layout (scroll, selection) untouched. Test:
`stream_fullscreen_needs_a_watched_stream_and_exit_restores_the_window_state`; verified in the headless rig
(sway `fullscreen_mode` 0 -> 1 on the double-click, 0 again on Escape from a windowed start).

## Behavioural rules that carried over

These were decided in the earlier UIs and are still the rules.

- **There is no join gate.** Joining a room is one affordance in an always-present
  sidebar. *Why:* with N simultaneous rooms there is no moment at which the app is "not
  joined yet"; it is joined to some set of rooms, possibly empty, that changes while you
  use it.
- **Rooms list = the saved-room list.** Every saved room is a row; the ones with a live
  `RoomSession` get connected styling (left join, `RoomStore` -> sessions). *Why:* driving
  from the persistent side makes a disconnected saved room a row you can reconnect
  instead of something that vanishes when its connection drops. The two sides are joined
  on the **join string** (a `RoomHandle` is per-connection and a saved room has none), which
  is the UI's `RoomKey`. **It contains the room key: never render or log it**, show the
  room name.
- **Switching what is shown never starts or stops a share, a subscription, a decoder or a
  pop-out.** Starts and stops are explicit actions (`Watch`, `StageShare`, `GoLive`,
  `Unwatch`, ...); `VideoViews` is synced by the shell every frame regardless of `Nav.view`,
  so an off-screen room's streams keep decoding.
- **Leaving raises a confirm iff the room has an outgoing share or a live watch**
  (`Modal::LeaveConfirm`); otherwise it leaves immediately. *Why:* nothing to lose means a
  confirm is only a speed bump on the commonest case, but leaving a room you stream to is
  "killing an outgoing broadcast, not navigating", and a popped-out window vanishing
  unannounced is, if anything, more startling.
- **Display name is one global setting** (Settings modal, Profile card), not asked per
  room. *Why:* the wire protocol sends the name once at connect, so a per-room value was
  only ever the same string typed again.
- **"Choose your name" modal** (`Modal::ChooseName`, `MODAL_W`, `modals::choose_name`): while
  the user never saved a name (`AppState::display_name_set == false`, i.e. empty
  `AppSettings::display_name`; the shown name is then the `whoami_fallback()` OS username),
  `App::prompt_for_name` opens it once per run as soon as no other modal is open
  (`Shell::name_prompt_shown`). Field prefilled with the fallback and selected, Enter/Save
  = `SetDisplayName` + close (saving the prefilled name counts), Later/Escape/x = close until
  the next start; Save is disabled for an empty field. Saved once, it never shows again.
  Rig/launch clients are seeded with `display_name = "client-N"`, so scripted flows never see it.
  *Why:* user request 2026-10-15, encourage setting a name instead of silently using the
  default. Caveat: a room joined before the name is saved (auto-connect) keeps the old name
  for its peers until the next connect, the wire sends the name only once.
- **Server address validation (task #84):** the Create tab's server field and the server part of
  a pasted join link (checked once the first `|` is typed) must be `host:port` / `ip:port` /
  `[ipv6]:port`, port 1-65535 (`net::parse_server_addr`; one shared parser). While it is not,
  a red hint sits under the field ("Add the relay's port, e.g. example.org:4433", or the
  bad-port variant) and Create / Join is disabled (also Enter); an empty field shows no hint,
  only the disabled button. `AppState::create_room` rejects it too. A connect to a saved room
  with a portless server fails with "server address needs a port, e.g. host:4433"; a real DNS
  failure reads `could not resolve host "...": <resolver error>` (those two are English
  `anyhow` messages from `net.rs`, like the other connect errors; the dialog hints are i18n keys
  `ui.modal.server_no_port` / `server_bad_port`). *Why:* user report 2026-10-15, a relay
  address without its port (`vogelnest.any64.de`, relay on 41000) showed "could not resolve
  server address ...: invalid socket address", which read like DNS. Not silently defaulted to
  4433 because real relays use other ports and a default would become an opaque timeout.
  Browser twin: `docs/js/ui/app.js` (`serverHint`, `joinLinkHint`), `superdoc/features/website.md`.
- **Create/join modal:** server field pre-filled from the most recently saved room (the lib
  keeps no "last server" setting). A freshly minted link is shown masked (it carries the
  room key) and has no relay cert hash, so browsers can't use it; "Copy invite link" copies
  the *live connection's* link, which does, and which also carries the saved room name as the
  link's 5th field. Pasting a link with a name into the Join tab fills the Room name field
  (only while that field is empty or still holds the previous prefill; the user can edit it,
  and what is in the field on Join is what is saved; `apply` drops the 5th field from the
  stored join string). *Why:* user request 2026-10-15, so a friend does not have to retype
  the name. A room with no name is labelled from the first 4 bytes of
  its room id in hex (`session::room_label`), never from the join string.
- **Per-stream numbers are never invented**: sidebar fps = decoded fps while we watch a
  stream (the protocol's `StreamInfo` carries none), or the live destination's frame rate
  for our own share; empty otherwise.
- **Share-state drives the action bar** (`ShareState`: Idle / Picking / Staged / Live /
  Paused). Starting a share opens the OS picker first (capture `Starting` = Picking); the
  stream-settings modal then opens by itself.
- **Stop frees the room at the click (2026-11-02, #98).** `AppState::stop_share` removes the
  capture's destinations from the model immediately (the capture stays in `Stopping` only to own
  the teardown), so `share_state` is `Idle`, the bar shows "Share screen" and the sidebar/tile
  stop claiming a live share while a slow driver teardown is still running. *Why:* the old
  behaviour (LIVE and "Stop sharing" for the whole teardown, up to ~15 s on a stuck VA branch)
  was reported by a user as "it just stays open". No separate "Stopping..." indicator: the
  teardown is invisible by design, and the info-level `capture stopped in N ms` log line is how
  a slow one is found. `features/stream-control.md` "Stop is instant".
- **Recovering (2026-11-02, #99).** While `AppState::recovery(capture)` is `Some`, the sharer's
  pill for that capture's destinations is *Recovering* (`PillKind::Progress`, in `BAR_PILLS` so
  the pill slot is wide enough in every language), the bar's title reads "Screen capture lost" /
  "Restarting the screen capture", and the room tile chip reads RECOVERING. The bar tooltip
  (`recovering_tip`) carries the attempt number ("Attempt 2 of 5"), the countdown to the next
  attempt and the last error. Only a capture that actually has a recovery shows it: a plain
  `Stalled` destination (a few seconds of silence that heals itself) keeps the *Stalled* pill.
  When the schedule is exhausted the destinations turn *Failed* with a share-error toast; there
  is no Retry button in the egui UI (`retry_destination` is lib-only), so the user stops sharing
  and shares again. The stream-settings modal's per-room rows still
  use `dest_pill` (Stalled / Failed), not the recovering override.

## Saved-room identity (2026-10-14)

One saved room per room. `RoomStore` (`client/src/rooms.rs`) identifies an entry by
`room_identity(join_string)`: server + room id + key, **without** the optional 4th cert-hash
field (parsed via `net::parse_join_string`, re-encoded canonically; an unparseable legacy
string falls back to its first three `|` fields). `upsert`, `get`, `name_for`, `rename`,
`remove` and `set_auto_connect` all go through it, so each touches exactly one entry.

`Why` not keyed by name: names are free local nicknames, so name-keying let one link joined
as "MovieNight" then "Films" become two entries (both shown connected, one leave dropping
both, forget removing only one). `Why` not the whole string: the relay's cert hash rotates;
a rotated cert must update the entry, not add a second one.

- `upsert(name, js)` on an existing room: non-blank `name` wins (blank keeps the old one),
  stored join string becomes `js` (newest form), `auto_connect` untouched.
- Migration: `RoomStore::load` calls `merge_duplicates` and writes the file back if
  anything merged. Per duplicate group: first entry's position, **last** non-blank name,
  last entry's join string, `auto_connect` OR-ed. `Why` last: the file has no timestamps and
  the old store appended on every new name / rename, so file order is the recency signal.
- UI `RoomKey` = the saved entry's stored (connectable) join string. It is stable across
  name-only upserts and re-saves of the identical string; it changes only when a different
  spelling of the same room (new cert hash) replaces it. `RoomStore::same_room(a, b)`
  compares two join strings by identity, for UI code matching a live session to a key.

## Strings

All user-visible text goes through `ui::tr` / `tr_with` (English fallback inline; locales
in `client/locales/{en,de}.yml`, German in du-form; check with
`python3 -I client/locales/check_keys.py`). See `STYLING-GUIDE.md` > i18n.

## History (superseded designs; the reasons are kept above where they still apply)

- **First egui UI** (2026-07-25/26, `ui.rs` + a nav rail with Stream Control / View /
  Settings tabs, ROOMS and capped STREAMS sections in the sidebar). Superseded by the
  Electron frontend, then deleted. Its sidebar used nested panels instead of spacers (the
  idea survives in the shell above), a cross-room STREAMS list ("a destination is a thing
  I'm broadcasting right now; scoping it to the selected room would hide streams you still
  pay bandwidth for") and `active_room` as a single designated room driving the tabs. The
  new design replaces all three: sharing is room-scoped in the action bar, and "Sharing
  elsewhere" is reachable from the user band's Outgoing popover.
- **Electron + HTML frontend** (2026-07-26 to 2026-10, removed): see
  `FRONTEND-SHELL.md`. Its look (tokens, copy, behaviours) is the reference the egui UI
  copies; its structure is gone.
- The full text of the earlier versions of this page (nav rail, click semantics, STREAMS,
  Electron-era notes) is in git history: `git log -- superdoc/ui/APP-SHELL.md`; the last
  pre-rewrite content is at commit `3f2d5d3`.
