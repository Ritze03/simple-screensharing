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
- **No self-preview** of your own share. *Why:* matches the former Electron app's deliberate
  removal (see `superdoc/features/self-preview.md`).

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
shows a placeholder. The pop-out has hover-gated chrome (statbar, audio pill), F11
fullscreen, and Escape only *leaves* fullscreen, never closes (the former Electron viewer's
rules, carried over; `superdoc/features/viewer-window.md`).

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
