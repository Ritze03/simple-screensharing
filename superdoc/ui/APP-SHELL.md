# App Shell (native egui client)

Rewritten 2026-10-14. How the desktop client's one window is laid out, which module owns
which region, and the behavioural rules and design reasons behind it. Code:
`client/src/ui/` (screens), `client/src/widgets/` (layout + controls),
`client/src/theme.rs` (tokens). How controls are built and styled: `STYLING-GUIDE.md`.
What the UI sits on top of: `superdoc/architecture/overview.md`.

The client's UI is **one native egui app**. It replaced an Electron + HTML/CSS/JS frontend
(and, before that, a first egui UI with a Stream Control / View / Settings nav rail); both
are history, see the end of this page and `FRONTEND-SHELL.md`.

Split 2026-11-07: this page keeps the shell (sidebar, header, narrow windows, the screen contract, popovers and modals, the
behavioural rules, saved-room identity). The rest moved verbatim to single-topic pages: `STAGE.md` (the stage, bar, menus,
webcam in picture, pop-outs, fullscreen, saved Layout settings), `SHARE-FLOW.md` (share state, stop, recovery, stream-settings
rows, gamescope question), `SETTINGS.md` (Settings cards, Boost, Level loudness, Playback sliders), `MODULE-MAP.md` (file table).

## Design

**Room-centric, Discord-like.** You pick a room in the left sidebar; the centre shows that
room; everything you can do about sharing is scoped to the selected room. The layout was
approved by the user as an HTML mockup (its "Proposed" tab; link in `.claude/local/styling-reference.md` in the main checkout).

```
┌ sidebar 272 ────────┐┌ room header band 56: title, status pill, meta                   ┐
│ brand band 56       │├────────────────────────────────────────────────────────────────┤
│ Watched streams     ││ the stage (black): streams as Split / Crop / Floating           │
│ ROOMS (rooms well,  ││   people strip: avatars, ring, LIVE badge, hover card           │
│  sharer sub-rows)   ││   control bar: [sidebar][invite] [share|audio|webcam][layout|   │
│ Create or join      ││      room|level|people][leave] [pop out][fullscreen]            │
│ user band 56        ││  (other states: scrolling body, pad 16)                         │
└─────────────────────┘└────────────────────────────────────────────────────────────────┘
```

The People panel and the bottom action band are gone (task I3, 2026-11-04): the stage's strip and bar replace
them, and the room header lost its controls (Auto-connect, Copy invite link, Leave and `⋯` live in the bar and its
Room settings menu). *Why:* the user approved the replica's stage, where people and controls sit on the stage like a
call, not in side panels.

- Left sidebar **272 px** (hides with the bar's button, `Nav::sidebar_hidden`); room header band **56**. A room
  with no stage (not connected, connecting) keeps a `⋯` in the header for the Room settings menu, since there is
  no bar to hold it.
- **Connecting vs Reconnecting** (`ui::ConnState`: `Joined`, `Connecting`, `Reconnecting`,
  `Disconnected`). `Connecting` is a room's *first* connect in flight (sidebar/header pill
  "Connecting"); `Reconnecting` is only a suspended room (TERMINOLOGY) that the lib is
  retrying (`AppState::reconnect_info`). Both show the panels and a "stop connecting" Leave.
  *Why:* the first Connect of a saved room used to read "Reconnecting" (stage-6 QC).
  **In the call / not in the call** (row 192) is not a `ConnState`: a `Joined` room reads `AppState::in_call`
  (header pill "In the call" vs "Connected"). Every connect starts not in the call; the red bar button hangs up
  in the call (`HangUp`, room stays) and disconnects outside it (`Leave`). `STAGE.md`.
  A failed Connect (`start_connect` error) raises an error toast and the room's Disconnected
  view shows an inline warning banner above the empty state, text
  `ui.room.connect_failed` ("Couldn't connect: <error>"), cleared on the next Connect.
- **Share labels** are translated at draw time by `ui::share_label(raw)`: the default capture
  label and the wire default "Screen" (in any locale) render in the current language, other
  names (audio sources, peer-chosen labels) as is. The lib stores the label translated at
  creation, so without this a language switch would leave old shares in the old language.
- The stage (strip and bar included) exists only while the shown room is joined; "Watched streams" is always a stage;
  a disconnected, connecting or reconnecting room and the welcome view show the scrolling body (`room::shows_stage`).
- **The Stream Control / View / Settings nav rail is gone.** Settings is a modal opened
  from the user band's gear; sharing lives in the stage's control bar; watching is the
  the room's stage.
- **Settings is a modal with four tabs** (Playback is back since 2026-11-04, row 136), Profile | Interface (Language, Display, Sounds) | Playback (Jitter buffer, Catch-up speed) | Diagnostics (a
  `widgets::segmented` row, `ModalsState::settings_tab`, opens on Profile and keeps the choice while the app runs); the
  body has one height on every tab (`SETTINGS_BODY_H`). The browser has Profile | Interface (Language, Sounds) | Playback | Connection.
  Level loudness is not in Settings: it is a control-bar button (`SETTINGS.md`, "Level loudness: a control-bar button").
  *Why:* the user's request, 2026-11-04.
- **Stream settings open in a modal**, not a side panel. *Why:* the user said the side
  panel was too squished for the number of controls.
- **Leave sits in the control bar** (red, as tall as the button groups; before I3 in the room header). *Why:* the
  replica's call; it is a room-level action, so it is not in the Share group.
- **Narrow windows (800 px minimum, a ~528 px stage when the sidebar shows).** *Why 800 (PR #4, 2026-11-03; 960
  before):* so the window half-tiles on a 1920 px screen. The control bar degrades in a fixed order (`stage::bar::arrange`,
  pure, tested): the **Copy invite link label** goes first, then the bar takes **two rows** (the middle part on top, the
  side buttons under it, 104 px instead of 72); the middle part (two groups and Leave, ~500 px) still fits 528 px with 8 px
  side padding. Icons never move into a menu. The hover card and the menus clamp to the window. The header drops its meta
  line when it does not fit (never half-cut), a long room name truncates. Stage tiles: the name tag drops the stats
  resolution, then the bitrate (`tile::stats_fields`), the number slots never half-draw. Body and modal: the
  stream-settings modal stacks its two columns into one when a column would be narrower than its widest segmented
  control / Screen readout needs (measured from the current locale's labels, so it is one column at 800 and 960 and
  two at 1280+ in en and de); the reconnect banner moves its buttons under the text when the text column beside them
  would be under 320 px; empty states wrap, centred. *Why measured, not a fixed breakpoint:* German labels run ~20 %
  longer, so a fixed width either stacks English for nothing or still truncates German. Details in
  `stream_settings.rs` (`column_need`), `room::reconnect_banner` and `stage/bar.rs`.
- **The stage** (the centre of a joined room and of Watched streams: tiles, Split / Crop / Floating, resize bars, people strip, control bar, menus, webcam in picture, pop-outs, fullscreen chrome, saved Layout settings): `STAGE.md`.

### Layout implementation rule

Every region is built from explicit nested vertical/horizontal containers with fixed-size
slots (`widgets::HBox`, row helpers, `card`/`well`/`band`/`grid_rows`/`modal_shell`); never
`ui.horizontal` + `add_space` for rows. The shell itself is egui `SidePanel` /
`TopBottomPanel` / `CentralPanel` at the fixed sizes above. Full rule and rationale:
`STYLING-GUIDE.md`. *Why panels and not spacers for the shell:* "this region is fixed, that
one takes the rest" is exactly what panels express; hand-computed `add_space` fillers would
re-derive the split every frame and silently break when content height changes.

### The screen contract (data in, actions out)

A screen module owns one `<X>State` (its private UI state: text buffers, edit copies,
hover) and `pub fn show...(ui, cx: &mut Cx, st)` functions. It reads everything via `Cx`
(`&AppState` read-only, `nav`, `shell`, `videos`, plus selectors) and **never mutates
`AppState`, `Nav` or `Shell`**; every change is `cx.act(UiAction::..)`, applied by
`App::apply` after all screens have drawn (visible next frame). Local edit state is pushed
as one action when it should take effect (e.g. on `drag_stopped`, not every frame of a
drag), except where the user wants to hear or see the change while dragging (the viewer volume and the Boost
slider push on every change). *Why:* the borrow checker can't hand four screens `&mut AppState` while they also
read it, and a single apply site keeps every state transition in one reviewable place.

The shell hands each screen a `Ui` that is exactly its region; a screen must not paint
outside it.

### Popovers, modals, toasts

At most one popover (`Nav.popover`, anchored to the button's rect) and one modal
(`Nav.modal`). Opening a modal closes the popover; Escape closes the popover before a
modal; Escape, x and a backdrop click close every modal. A modal is drawn in the window that opened it (main or a popped-out stage, `ui::modal_host`, `STAGE.md`). A toast is painted in a popped-out stage window only if that window took input within the last 2 s when it was raised (`ui::toast_origin`, as the browser), else main (`ui::toast_host`; a closed window hands it to main), last in that window
(`mod.rs` / `stage::show_windows`; 2.6 s; error toasts stay 6 s, `ERROR_TOAST_SECS`).

Settings cards (Logs, Relay diagnostics, Sounds, Avatar), Boost, Level loudness and the native Playback sliders: `SETTINGS.md`. Module list: `MODULE-MAP.md`.

**Window icon** (2026-10-15): `ui::app_icon()` (`client/assets/icon.png`, the `docs/assets/icon.svg`
favicon rasterised once to 256 px with `rsvg-convert`) is set with `with_icon` on the main and the
pop-out viewports. *Why:* on Windows (and X11) eframe's default "e" showed in the title bar and
taskbar. Wayland takes the icon from `app_id` + the .desktop file, so it is harmless there. The
exe file itself (Explorer, shortcuts) has no embedded icon resource: that needs a build-time
resource compiler (`winresource`/`embed-resource` + an `.ico`), not added.

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
- **Hanging up** (`Modal::HangUpConfirm`) follows the same rule as leaving: a confirm iff something is shared or watched,
  and the room stays connected afterwards.
- **Leaving (disconnecting) raises a confirm iff the room has an outgoing share or a live watch**
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
  address without its port (`relay.example.org`, relay on port 41000) showed "could not resolve
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
- **Share state, stop, recovery** (`ShareState`, "Stop frees the room at the click", "Recovering"), the stream-settings modal's Encoding / Keyframe rows and the gamescope question: `SHARE-FLOW.md`.
**Hide anything that does nothing, even when it is only a browser limitation** (2026-11-01, task #93).
User rule, verbatim: "If a setting doesn't do anything, hide it (Yes, even when its just a browser
limitation). Just hide it, so other platforms can still use it." So the rule is: *hide in the UI of the
platform where it is a no-op; never delete the setting from the shared data, persistence, protocol or
the other platforms' UI*, and a stored value this platform cannot honour must round-trip unchanged.
*Why hide, not delete:* the same settings file / join flow is used by the native app and the browser;
the setting still means something elsewhere. *Why hide, not grey out or show read-only:* a greyed or
read-only control still looks like a setting and invites the question what it is for (#90 said "not
show"; #93 extended it to the readouts and the disabled buttons).

Still applied today (checked against the code 2026-11-07): the **Codec** row is drawn only when the machine encodes more than one
codec (`presets::codec_choice_offered`), and `capture::supported_codecs(Vaapi)` offers H.265 / AV1 only when GStreamer has
`vah265enc` / `vaav1enc`, so a codec the GPU cannot encode has no button; **Settings > Presentation** in a fallback run
(the surface lacked the stored present mode, so `main` relaunched on Vsync, `SSS_PRESENT_MODE_FALLBACK`) hides the stored mode
because it does nothing for that run and shows Vsync as selected, and re-picking Vsync is no change
(`ui::modals::presentation_choices`); the browser share modal has no Quality-level row (WebCodecs has no such knob) and hides
the Codec row when only one codec is encodable (#93; not re-checked in 2026-11-07).
The `capture::destination::DestinationRuntime::actual_backend` field stays apart from `VideoParams::backend` (see History).

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
- **Stream settings: Tune, twin presets, per-destination backend, Advanced height** (#90, #93, #95 and a 2026-10-15 height fix; 2026-10-15 to
  2026-11-01; the code is gone since the 2026-11-04 preset rework, plan D8). Why lines kept:
  *Why Tune stayed for a while:* the user judged x264's ~0.7-1 s lookahead latency on "Best quality" acceptable ("It's screensharing,
  not remote gaming"), and asked "Keep the [Tune] setting. But make sure to not show settings, if they wouldn't do anything!"
  (it only acts on x264; every VA encoder dropped it). *Why twin choices were hidden:* on Mesa AMD the driver reads VA target-usage as a bitfield
  where 2 and 3 are identical, so "Balanced" was a second "Performance", and Low-Latency equalled Balanced there; a button that
  yields the same encode is a control that does nothing. It was derived from the effective encode of the running backend, not from a
  hard-coded table, because the answer differs per driver. The browser hid its Low-Latency preset button the same way (it equalled Balanced);
  a stored `low_latency` or `balanced` is now shown as Quality (`docs/js/ui/store.js`).
  *Why `actual_backend` is a separate field from `params.backend`:* `params.backend` is the *preferred* backend, tried first again on the next
  branch swap; a transient VA failure must not pin the destination to x264 for good.
  *Why the modal once reserved the Advanced rows' height:* the user disliked it growing, shrinking and, being centred, jumping when Advanced was
  toggled (a measured pass beat a remembered or fixed height); with Advanced gone the problem is gone too.
- The full text of the earlier versions of this page (nav rail, click semantics, STREAMS,
  Electron-era notes) is in git history: `git log -- superdoc/ui/APP-SHELL.md`; the last
  pre-rewrite content is at commit `3f2d5d3`.
