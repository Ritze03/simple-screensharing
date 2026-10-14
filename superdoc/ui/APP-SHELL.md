# App Shell (desktop client)

**Added 2026-07-26.** How the desktop client's window is laid out, what lives where, and
which parts are deliberately minimal mount points for later stages. Implemented in
`simple-screensharing-client/src/ui.rs`; the visual vocabulary it's built from is
`superdoc/ui/STYLING-GUIDE.md`, and the plan it implements is
`superdoc/brainstorm/ui-redesign-multiroom-streaming.md` (final plan item 10).

**This egui implementation has been superseded by a plain HTML/CSS/JS frontend**, shown in
an Electron `BrowserWindow` (originally a Tauri webview — see
`superdoc/architecture/overview.md`'s "Process model" for the current Electron-shell +
Rust-sidecar architecture and `superdoc/ui/FRONTEND-SHELL.md`'s 2026-07-28 entry for the
migration itself). `ui.rs`/`theme.rs` (this page's implementation) are dead code today —
still in the tree, zero live callers. The live shell lives at
`simple-screensharing-client/frontend/` — see `superdoc/ui/FRONTEND-SHELL.md` for what's
built there, how it maps to the sections below, and the later visual redesign this page
does not cover. The behavioral rules on *this* page (nav-is-a-pure-view-switcher,
ROOMS/Outgoing layout, room-row states, leave-confirm rules, …) are still the rules the
live frontend follows, even though the code that originally defined them no longer runs;
only `FRONTEND-SHELL.md` covers what's HTML/CSS/JS-specific and how the real
implementation has since diverged visually (status vocabulary, destination-card styling,
the room-detail slide-over, the Watch grid) from what's described below.

**In-progress Tauri-port step: `ui.rs`'s state/logic layer has been extracted into
`simple-screensharing-client/src/session.rs`.** `session::AppState` (renamed/restructured
from the old `App`) now owns the room/capture/destination session model and every
business-logic method — connect/room lifecycle (`start_connect`, `insert_room`,
`disconnect_room`, `drain_events`, `request_leave_room`, `poll_connect_result`), watching
(`toggle_watch`, `toggle_listen`), sharing (`start_video_share`, `start_audio_share`,
`stop_share`), and **stage 2's fan-out/transfer/bandwidth machinery**
(`add_destination`/`transfer_destination`/`apply_destination_edit` and the
`BandwidthController` behind Absolute-Maximum mode's live wiring) — and is exported from
`lib.rs` so `src-tauri` can depend on it as a library without pulling in `eframe`/`egui`
rendering. `ui.rs`'s `App` is now a thin wrapper: `state: session::AppState` plus
purely-UI fields (`nav_tab`, `add_room_open`, `expanded_rooms`, `leave_confirm`,
`theme_installed`); every `ui_*` function keeps rendering exactly as before, just calling
`self.state.*` instead of owning logic itself. A few `App` methods
(`start_connect`/`disconnect_room`/`request_leave_room`/`poll_connect_result`) stay
same-named thin wrappers rather than pure pass-throughs, because their `AppState`
counterpart used to also touch a UI-only field directly (the "+ Add Room" card's state,
`expanded_rooms`, `leave_confirm`); `AppState::poll_connect_result` instead returns a
`ConnectOutcome` enum so that boundary never leaks a UI-only field into the extracted
state. This is a pure structural reorganization — nothing on this page's layout, row
states, or behavioral rules changed; the Stream Control feature set below (bandwidth
modes, fan-out, transfer, per-destination edits) is unchanged behaviorally, just now
driven from `session.rs`. `#[tauri::command]` wrappers over `session.rs` are a later,
separate step. See `session.rs`'s module doc for the full rationale.

This is stage 1 of a three-stage UI build:

| Stage | Scope | Status |
| --- | --- | --- |
| 1 | App shell: sidebar, nav, ROOMS, STREAMS, tab skeletons | **this document** |
| 2 | Stream Control's real feature set: multi-destination fan-out, bandwidth modes, transfer | **built** — `superdoc/features/stream-control.md` |
| 3 | View's real feature set: one global auto-tiled grid, restart placeholders, per-tile overlays | **partially built** (real auto-tiled grid + real decoded video + per-tile overlays + a fullscreen/borderless multi-window viewer — see `superdoc/features/video-pipeline.md`/`superdoc/features/viewer-window.md`; the tiered capture-restart placeholders are still not built) |

## Layout

```
┌──────────────────────┬────────────────────────────────────────┐
│ Simple Screensharing │                                        │
│                      │                                        │
│ ┌──────────────────┐ │                                        │
│ │  Stream Control  │ │        content area                    │
│ ├──────────────────┤ │   (whichever nav tab is selected)      │
│ │      View        │ │                                        │
│ ├──────────────────┤ │                                        │
│ │    Settings      │ │                                        │
│ └──────────────────┘ │                                        │
│ ROOMS                │                                        │
│  ● Movie Night   3   │                                        │
│    ▸ …               │                                        │
│  ○ Old Room  [Recon] │                                        │
│        (greedy)      │                                        │
│ [   + Add Room    ]  │                                        │
│ STREAMS   (capped)   │                                        │
│  Movie Night: …      │                                        │
└──────────────────────┴────────────────────────────────────────┘
```

**There is no join screen.** The old model was a one-time gate: a join form, then a
"connected" screen the app stayed on until the connection died. Both are gone. Joining a
room is one affordance inside a sidebar that is always present, on every tab, in every
state.

`Why:` a join gate is a single-room assumption baked into the navigation. With N
simultaneous rooms there is no moment at which the app is "not joined yet" versus
"joined" — it's joined to some set of rooms, possibly empty, and that set changes while
you use the app.

### Implementation note: nested panels, not spacers

The sidebar is built from nested `egui` panels (`TopBottomPanel::top` for the head,
`TopBottomPanel::bottom` for STREAMS and for "+ Add Room", `CentralPanel` for ROOMS)
rather than one vertical layout with `add_space` fillers.

`Why:` the required behaviour is "ROOMS consumes the greedy middle; STREAMS is capped
and never squeezes it out". Panels express that directly — each fixed section takes its
natural (or capped) height and the central region gets the remainder. Hand-computed
spacers would have to re-derive the same split from `available_height()` every frame and
would silently get it wrong whenever either section's content changed height.

## The nav is a pure view-switcher

**Hard rule:** switching tabs never starts or stops a share, a subscription, a decoder,
or a pop-out window. A stream keeps decoding, playing and repainting while its tab isn't
showing.

Two things make this true, both structural rather than a convention to remember:

1. Every start/stop transition is an explicit user action (`toggle_watch`,
   `start_video_share`, `stop_share`, …) reachable from the **sidebar**, which renders
   on every tab. No `ui_*_tab` function performs one as a side effect of being rendered.
2. `eframe::App::update` polls *every* room's watches (`update_watch_textures`) and
   re-declares *every* popped-out viewport (`show_popouts`) **before** the nav tab is
   consulted at all — a room being off-screen is a view decision, never a reason to
   starve its decoders.

Covered by a real regression test: `ui::tests::
watched_panel_repaints_continuously_without_input_at_real_decode_rate` (an `#[ignore]`d
test against a real relay server, a real capture pipeline and a real decoder) flips the
nav tab on *every* loop pass — ~110k switches over an 8-second run — and asserts the
decoder still holds its full source framerate and the watch is still registered at the
end.

## Sections

### ROOMS

The sidebar's "Rooms" **is** the saved-room list. There is no second concept: the list
is rendered as a left join, `room_store ⟕ rooms` — every saved room gets a row, and the
ones that have a live `RoomSession` get connected styling.

`Why` drive the loop from the persistent side: it's what makes a disconnected saved room
a *row* (with `[Reconnect]`) instead of something that vanishes the moment its
connection drops.

The two sides are joined on the **join string**, which is the only key both carry — a
`RoomHandle` is per-session and a saved room has none; a relay-assigned `ConnId` is
per-connection and a saved room has none either. Hence `RoomStore::name_for(join_string)`
and `RoomSession::join_string`.

Three row states:

| State | Dot | Body | Actions |
| --- | --- | --- | --- |
| Joined | solid `SUCCESS` | `<name> — N active` | expand caret, leave (`🗙`) |
| Reconnecting | pulsing `WARNING` (alpha sine) | — | (disabled `[Reconnect]`) |
| Saved, disconnected | hollow `TEXT_FAINT` ring | — | reconnect (empty at rest), forget (`🗙`) |

#### Click semantics (2026-07-29, full overhaul, owner request)

The row's click behavior is now state-dependent and mouse-button-dependent — plain
left-click no longer opens the settings panel for every state (that behavior, documented
just below in the 2026-07-28 note, is **superseded** by this pass):

| State | Left-click | Right-click | Middle-click |
| --- | --- | --- | --- |
| Joined | Jump to the View tab, filtered to this room's streams — unless it's the *only* currently-joined room, in which case no filter (View already shows "all rooms", which is exactly this one room) | Open the room-detail panel | Disconnect (same confirm-gated path as the panel's own Disconnect button) |
| Disconnected | Reconnect (same `join_room` call the old caret-only reconnect button used) | Open the room-detail panel | No-op — nothing to disconnect |
| Reconnecting | No-op — already mid-connect | Open the room-detail panel | No-op |

Implementation (`renderRoomRow`, `frontend/scripts/components.js`):
- Left-click (`click`) branches on `room.state`.
- Right-click (`contextmenu`, `e.preventDefault()`'d so the native OS/browser menu never
  shows) always calls `handlers.onOpenRoom` — this is the old plain-left-click behavior,
  just moved off left-click since that's now spoken for.
- Middle-click (`auxclick`, `e.button === 1`) calls `handlers.onLeave` — the exact same
  `openLeaveConfirm` path (`shell.js`) the panel's own Disconnect/✕ button already uses, so
  a middle-click on a room with an active outgoing stream or a live watch still raises the
  confirm modal; it is **not** bypassed. Judgment call: middle-click is an easy accidental
  trigger (a stray scroll-wheel click), so if anything it deserves *more* guardrail than a
  deliberate button click, not less — bypassing the confirm here would have been the wrong
  direction. A `mousedown` handler on the same button additionally calls
  `e.preventDefault()`, suppressing X11's middle-click autoscroll/paste-primary-selection
  gesture that would otherwise fire before `auxclick` does.
- The row carries a `title` attribute spelling out all three bindings for discoverability
  ("Left-click: connect/view · Right-click: settings · Middle-click: disconnect").
- **The disconnected caret's dedicated reconnect button/icon is gone.** Before this pass,
  a disconnected row's caret slot was its own separate hit-area with a hover-reveal
  `restart_alt` icon and its own `onReconnect` click handler (see the 2026-07-28 note
  below for why it looked the way it did). Once the whole row does the same "reconnect"
  action on left-click, that narrower hit-area became redundant scaffolding — two code
  paths triggering the exact same action with no behavioral difference between them — so
  it was deleted outright rather than kept as a secondary trigger. Disconnected rows now
  render the same empty `.room-caret-spacer` the `reconnecting` state already used; the
  corresponding CSS (`.room-caret-reconnect`, `.room-caret-reconnect-icon`,
  `frontend/styles/shell.css`) was removed with it.
- The joined row's caret (chevron) is unchanged: left-clicking it specifically still opens
  the panel directly (`stopPropagation`'d so it doesn't also fire the row-wide left-click
  jump-to-View behavior) — a deliberate secondary way to reach settings without a
  right-click, now that plain left-click on the rest of the row means something else.
- The View tab's room filter (`view.js`'s internal `roomFilter`) is now settable from
  outside that file via a new `window.ViewTab.setRoomFilter(roomLabel)` export (`null` =
  all rooms) — `shell.js`'s `jumpToViewForRoom` calls it right after `setActiveTab("View")`,
  counting currently-joined rooms (`mockRooms.filter(r => r.state === "joined").length`,
  same pattern `refreshRooms` already uses) to decide whether to pass the room's name or
  `null`.
- The action slot's own leave/forget icon-button (`🗙`) and the sidebar's "Forget room"
  trash icon both keep their existing `stopPropagation()` on `click` — unaffected by any of
  the above, still a fast path independent of the row's new click semantics.
- Verified against the real app (Electron + built `src-tauri` sidecar + a local relay):
  left-click a disconnected room reconnects it; left-click a joined room jumps to View and
  filters to that room's streams when a second room is also joined, and shows "all rooms"
  (no filter) when it's the only joined room; right-click opens the settings panel with no
  native context menu appearing; middle-click on a joined room disconnects it, going
  straight through (no stray confirm modal appeared for a room with no stake — consistent
  with `openLeaveConfirm`'s existing has-a-stake check, not a middle-click-specific bypass).

`Why:` (2026-07-29, second owner screenshot pass) two more cleanups on
`renderRoomRow`/`shell.css`, on top of the 2026-07-28 redesign documented right below this
note:
- **Member/LIVE tree dash removed.** Each member row (`.room-member-tree-name`) used to
  render a literal `"- "` prefix via a `::before` rule as a poor-man's tree bullet — the
  owner's screenshot of an expanded joined room showed it rendering as "- Ritze" and
  flagged it. The `::before` rule is deleted outright (not replaced with a different
  bullet character) — the tree's existing `padding: 2px 0 2px 21px` (lining the rows up
  under `.room-name`, past the dot/caret columns) already reads as a clear indent under
  the room name without needing a literal glyph, so no compensating CSS was added.
- **Reconnect caret's yellow dot removed.** The 2026-07-28 redesign below replaced the
  disconnected caret's `restart_alt` icon with a static solid-yellow `.status-dot`
  (`.room-caret-reconnect-dot`). The owner's next screenshot flagged *that* as redundant
  clutter: a disconnected room already shows the row's own hollow
  `.status-dot-disconnected` ring immediately to its left, so the row was rendering two
  dots side by side for one piece of information. `.room-caret-reconnect-dot` is deleted;
  the caret slot now renders nothing at rest — visually identical to the empty
  `.room-caret-spacer` the `reconnecting` state already uses, so all three caret states
  (chevron / empty / empty) read consistently, and the row's hollow status dot alone
  communicates "not connected". To keep the reconnect action discoverable without a
  permanent dot, a `restart_alt` glyph (`.room-caret-reconnect-icon`) fades in only on
  `:hover`/`:focus-visible` of the `.room-caret-reconnect` button — the button itself
  stays the same size and stays clickable (`onReconnect`) at rest, it's just visually
  quiet until the user is pointed at it.

`Why:` (2026-07-28, room-row redesign — a real screenshot from the project owner
flagged the old disconnected row) two things changed on the JS shell
(`renderRoomRow`, `frontend/scripts/components.js`), stale table cells above fixed to
match:
- **Body text.** Disconnected used to show a literal "not connected" string in the
  meta column; removed as redundant with the row's own hollow status dot and dimmed
  name — it now falls through to the same "—" the other two states already show,
  same convention as `active_stream_count`'s old `"{N} active"` summary being
  replaced by the member/LIVE tree below the row (2026-07-28, `stream-control.md`).
- **Reconnect action.** The disconnected caret's `restart_alt` icon read as "an
  arrow pointing at itself" in the owner's screenshot. Replaced with a plain,
  static, warning-yellow dot — the same `.status-dot` box/size every other room
  state's dot uses (`.room-caret-reconnect-dot`, `frontend/styles/shell.css`) —
  that's still the click target for `onReconnect` (`handlers.onReconnect`,
  `shell.js`'s `renderRooms`). Reconnect-ability stays a fast path on the row
  itself rather than moving into the room-detail panel, since the panel's own
  footer (see below) already covers the room-management actions that *do* belong
  there (disconnect/delete), and duplicating reconnect in both places would be
  redundant. **Superseded 2026-07-29: `.room-caret-reconnect-dot` itself was removed
  as redundant with the row's own hollow status dot — see the 2026-07-29 note above
  this one.**

Every row — joined, reconnecting, or disconnected — can open the room-detail slide-over
panel (`onOpenRoom`, previously wired for `state === "joined"` only). **Superseded
2026-07-29:** this is no longer the row's plain-left-click behavior (see "Click semantics"
above) — reaching the panel now takes a right-click (any state) or, for a joined row
specifically, a click on its caret. The panel itself (`renderRoomPanel`, `shell.js`) is
unchanged by that click-semantics pass and still degrades per state: a
disconnected/reconnecting room shows its status line ("Not connected" /
"Reconnecting…") and the Auto-Connect checkbox, but skips the Members/Live-streams
sections entirely — there's no live `room_handle`/roster to show for a room that
isn't actually joined. The footer's old single "Leave this room" danger button is
now two, side by side: **Disconnect** (renamed, text-only — it never had an icon to
begin with) and a new **Delete room**, reusing the exact same forget-room handler
the sidebar row's own trash icon already calls
(`openLeaveConfirm`→`leaveRoom`→`forget_room` once `room.room_handle == null`) so
forgetting a saved room no longer requires closing the panel first. Each button is
disabled outside the one state it applies to — Delete room needs `room_handle == null`
(disconnected only) — rather than hidden, so the pair's layout doesn't shift per room
state.

`Why:` (2026-07-29, connect-timeout + mid-connect cancel, owner request) **Superseded**:
Disconnect used to also stay disabled while reconnecting ("nothing safe to act on
mid-connect" — there was no way to cancel an in-flight connect). Two things changed
together: `session::AppState::start_connect` now wraps `ScreenshareClient::connect` in a
15-second `tokio::time::timeout` (`AppState::connect_timeout`, default
`DEFAULT_CONNECT_TIMEOUT`) so a connect to an unreachable/hung server gives up on its own
instead of hanging forever; and a new `cancel_connect` command
(`AppState::cancel_connect`, aborting the pending connect's `tokio::task::AbortHandle`)
lets the user give up on it sooner. Disconnect is now enabled while reconnecting too —
clicking it while mid-connect skips `openLeaveConfirm` (there's no live
connection/destinations/watches to lose, so the confirm doesn't apply) and calls
`cancel_connect` directly. Safe to abort at any point pre-success: `net.rs`'s
`ScreenshareClient::connect` doesn't spawn any fan-out tasks
(`control_reader_loop`/`datagram_reader_loop`/`preview_uni_loop`) until *after* it
returns `Ok`, so aborting mid-handshake leaves nothing half-registered to unwind. Also
closed a related visibility gap: a first-time join with a **blank** Room Name field
never got a `room_store` entry pre-connect, so before this pass it had no row at all to
show "Reconnecting…" on or right-click — `dto.rs`'s `saved_room_rows` now also
synthesizes a display-only `Reconnecting` row straight from `AppState::pending_connect`
when its join string has no saved entry yet (never touches `room_store` — the row is
gone the instant the connect resolves either way, same as it always was for a named
first-time join).

Expanding a joined room lists each *other* member's announced streams: owner name,
right-aligned `WxH@FPS`, and one icon-only watch/listen toggle.

`Why` remote rows always render `@—`: `StreamInfo`/`AnnounceStream` carry width and
height but **no framerate field**, and a subscriber genuinely cannot derive one before
subscribing and decoding. Adding that field is tracked in the plan doc's gap list and
needs coordinating with the relay and the browser viewer. Rendering the placeholder
keeps the row honest and keeps the column's width stable for the day the field lands —
better than inventing a number, and better than omitting the column and having the
sidebar's alignment shift later.

`Why` a room row never renders its join string: the join string contains the room
**key**. A sidebar is exactly the kind of thing that ends up in a screenshot or a screen
share, so the label falls back to a short room-id prefix (`Room 00112233`) when a room
has no saved name. Pinned by
`ui::session_model_tests::room_label_falls_back_to_an_id_prefix_and_never_leaks_the_key`.

#### "+ Create / Join a Room"

Pinned to the bottom of the ROOMS section (not the bottom of the sidebar — STREAMS lives
below it). Clicking expands an inline mini-card **directly below it, not a modal**. The
card's top row is a `pillSwitch` segmented control (`components.js`, the same widget the
Bandwidth section's "Absolute Maximum"/"Per-Stream Maximum" toggle uses) — **Join** first
and pre-selected by default, **Create** second.

`Why` inline: joining/creating is the most routine action in the app, and a modal would
blank out the very list the new room is about to appear in.

**Equal-width tabs (2026-07-28).** The Join/Create `pillSwitch` gets its own `.add-room-tabs`
modifier class (`shell.js`, styled in `shell.css`) so each segment is exactly 50% of the
row instead of sizing to its own text — "Join" and "Create" used to render at different
widths. Same scoped-modifier pattern `settings.css`'s `.settings-segmented` already uses
for the Settings tab's language switch: a class on *this* `pillSwitch` instance, not a
change to `.mode-toggle-btn`/`pillSwitch` itself, so other call sites (the bandwidth mode
toggle, the language switch, the encoder preset picker) keep sizing to content.

**Join tab.** Room Name (optional, local-only nickname) above Join link, `[Join]`/
`[Cancel]`, wired to the `join_room` command. No "Display name" field — see "Global
display name" below. No placeholder/hint text on either field (2026-07-28 — see "Field
reorder + Room Name on Join tab" below); the Join link field's label used to spell out the
`server|room-id|key` format inline, shortened to just "Join link".

`Why` no staging step, and why success saves: `RoomStore` already *is* this app's
persistent "rooms I care about" state. A separate "saved but never joined" concept would
be a third thing to reconcile with the two that already exist. So Join connects
immediately; on success the room is upserted into the store — under the typed Room Name if
one was given, otherwise an id-derived name (see "Field reorder + Room Name on Join tab"
below) — and appears live in the list; on failure the card stays open with the error
inline and **nothing is saved**. The Stream Control tab's "Room name" row can still rename
that entry afterwards (remove-then-upsert, since `RoomStore` is keyed by name and a plain
upsert would leave the old entry behind as a phantom second row).

**Create tab (2026-07-28).** Room Name + Server IP (Room Name first as of the 2026-07-28
field reorder below — see that note for why), `[Create]`/`[Cancel]`, wired to a new
`create_room` command (`src-tauri/src/commands.rs`, `create_room_impl`/`create_room`).

`Why` "create" is really just "join a room nobody's used yet": the relay server has no
create verb on the wire at all — `Registry::get_or_create_room`
(`simple-screensharing-server/src/room.rs`) auto-creates a room the instant anyone joins a
room-id it hasn't seen, per spec §10. So `create_room` mints a fresh, never-used room-id +
key pair, formats them into a `server|room-id|key` string with `net::format_join_string`
(the same encoding `net::parse_join_string` reads back — hex room-id, base64url room key),
and then runs through the exact same `join_room_impl` the Join tab uses. The first joiner
of a freshly-generated id/key implicitly becomes the room's effective creator; there is
nothing else "creation" could mean client-side.

`Why` the key is generated in Rust, not JS: the room key is a real AES-256 AEAD secret
(`net.rs`'s frame en/decryption use it directly), not a display label. `create_room_impl`
generates both the 16-byte room-id and the 32-byte key with `rand::random()` — the same
call `end_to_end_test.rs`/`audio.rs`'s own test room-id/key generation already uses
elsewhere in this codebase — which is backed by a CSPRNG, unlike anything available to
client-side JS in this app. There is no equivalent secure-randomness primitive in the
frontend, so the secret is never minted there.

`Why` Room Name (not an id-derived fallback) survives to the saved row: `create_room_impl`
upserts `RoomStore` under the typed name *before* calling `join_room_impl`, so by the time
`AppState::poll_connect_result` runs its usual "auto-name if nothing saved yet" check, the
name is already taken — no change needed to that path. Room Name is explicitly the same
kind of local-only nickname `SavedRoomRow.name` already is elsewhere (never sent over the
wire), not a display name.

`Why` no display-name field on the Create tab: `create_room` passes an empty display name
through to `join_room_impl`, which — same as the sidebar's own silent auto-reconnect join
call (`shell.js`) — leaves `AppState::display_name` untouched, i.e. whatever it currently
is: see "Global display name" below.

#### Field reorder + Room Name on Join tab (2026-07-28)

The "+ Add Room" card header text gained spaces around the slash ("Create / Join a Room"),
and both tabs now lead with the local-nickname field before the connection-detail field:
Create's Room Name now sits above Server IP (was Server IP then Room Name), and the Join
tab gained its own Room Name field above Join link. Placeholder/hint text was removed from
every field in the card (Server IP, Create's Room Name, and the Join link label's
`(server|room-id|key)` parenthetical, now just "Join link").

`Why` the Join tab's Room Name is optional, unlike Create's (required): leaving it blank
preserves the pre-existing "auto-name from the joined room's id" behavior this tab has
always had (see "Auto-generated saved-room names (Join tab)" below), so nothing regresses
for a user who doesn't care to name the room up front. Create's field stays required —
unchanged by this pass, and arguably the more consistent choice for a from-scratch redesign
given both fields now occupy the same "local nickname first" slot, but that's a judgment
call this pass didn't override.

- **Backend:** `join_room`/`join_room_impl` (`src-tauri/src/commands.rs`) gained a third
  `room_name: String` parameter, same "empty string means unset" convention
  `display_name` already used in that function. When non-empty (after trimming),
  `join_room_impl` pre-seeds `RoomStore` via `state.room_store.upsert(room_name,
  join_string.clone())` + `state.room_store.save()` **before** calling `start_connect` —
  same shape as `create_room_impl`'s own pre-seed, and for the same reason: by the time
  `poll_connect_result`'s "auto-name if nothing saved yet" check runs, the name is already
  taken, so no change was needed there. Upsert is unconditional whenever a name is given
  (matching `create_room_impl`'s own unconditional call), not "only if not already saved"
  (`poll_connect_result`'s own guard) — a name typed into the Join tab is treated as a
  deliberate override for this join, same as Create's Room Name always has been; `upsert`
  keys by name (not join string), so this can only overwrite a room already saved under
  that exact name, not collide with an unrelated one. `create_room_impl` itself now passes
  `String::new()` for this new parameter into `join_room_impl` (it already pre-seeded the
  name itself, before this parameter existed, so this keeps that pre-seed the sole write).
  `api_server.rs`'s HTTP dispatch table gained a matching `opt_string` helper (mirrors the
  existing `opt_u32`) so `room_name` is optional there too.
- **Frontend:** `shell.js`'s `renderAddRoomCard` Join tab gained a `roomNameInput` field,
  threaded through `submitJoin` (now `submitJoin(joinString, roomName, errorEl)` — the
  ROOMS empty state's own join form, `renderRoomsEmptyState`, is unaffected in behavior but
  its call site needed updating for the new parameter, passing `""`) into
  `join_room`'s `roomName` invoke arg. The mock (no-`window.__TAURI__`) branch uses the
  typed name when given, falling back to the existing random mock name otherwise, so manual
  browser testing of this field matches real behavior.
- **`renderRoomsEmptyState` (first-launch empty state) intentionally untouched** beyond the
  `submitJoin` call-site fix above: it's a separate, simpler join-only screen (no tabs, no
  room-name field) not part of the "+ Add Room" card this pass redesigned, and adding a
  room-name field there wasn't asked for.

#### Global display name (2026-07-28)

Display name is one setting, set once in the Settings tab, used for every room join/create
— not asked per-room anymore. Removed the "Display name" text field from both the Join
tab's form (`shell.js`'s `renderAddRoomCard`) and the ROOMS empty state's inline join form
(`renderRoomsEmptyState`).

`Why` a global setting is enough, not a per-room override: the wire protocol has no concept
of changing a display name after joining a given room — it's sent once, at connect
(`session.rs`'s `start_connect` reads `AppState::display_name` at that point, same as
before this setting existed) — so a per-room value was never anything other than "the same
string, typed again" in the first place.

- **Storage:** new `display_name: String` field on `rooms::AppSettings` (same TOML file as
  `bandwidth_mode`/`max_jitter_buffer_ms`/etc. — `src/rooms.rs`). Empty means "never set";
  `AppState::default()` (`src/session.rs`) falls back to the pre-existing
  `whoami_fallback()` OS-username default in that case, so a user who never touches the new
  Settings field sees no behavior change.
- **Commands:** new `set_display_name` (`src-tauri/src/commands.rs`, registered in
  `main.rs`'s `generate_handler!` and `api_server.rs`'s dispatch table) — mirrors
  `set_language`'s shape: updates the live `AppState::display_name` and persists via a new
  `session::save_display_name`. Exposed for reading through the existing `get_settings` /
  `SettingsDto` (`dto.rs`) rather than a dedicated getter, matching how `language`/
  `max_jitter_buffer_ms` are already exposed.
- **Frontend:** `settings.js` gets a DISPLAY NAME card (text input, save-on-`change` — same
  commit-on-blur pattern the jitter-buffer slider uses) ahead of LANGUAGE.
- **`join_room`'s Tauri command signature is unchanged w.r.t. `display_name`** (still takes
  it, kept for its existing Rust test call sites) — every frontend caller (`submitJoin`,
  `submitCreate`, the sidebar's silent auto-reconnect) now always passes `""`, which
  `join_room_impl` already treated as "leave `AppState::display_name` alone"; with the
  global setting now loaded into it at startup and kept live-current by `set_display_name`,
  "leave it alone" now means "use the global setting" instead of "reuse whatever the last
  join happened to set". (`join_room` did later gain a *fourth* parameter, `room_name` —
  see "Field reorder + Room Name on Join tab" above, a separate same-day change unrelated
  to display name.)

#### Leaving a room

Leaving raises a confirm dialog **iff** the room has an outgoing destination *or* a live
watch (video or audio). A leave with neither is immediate — there is nothing to lose and
a confirm would just be a speed bump on the most common case.

`Why` the streaming case counts too: per the plan's final verify pass, leaving a room
you're streaming *to* gets the same treatment as leaving one you're watching — "you're
killing an outgoing broadcast, not just navigating". Watching counts whether the tile is
inline or popped out into its own OS window; a popped-out window vanishing unannounced
is if anything the more startling of the two.

### STREAMS

This client's own outgoing destinations, **across every room** — not scoped to whichever
room the content tabs happen to be showing.

`Why` cross-room: a destination is "a thing I am broadcasting right now", and the point
of the multi-room model is that several can be live in different rooms at once. Scoping
this list to the selected room would hide outgoing streams the user is still paying
bandwidth for.

Rows read `<room-label>: WxH@FPS`. `WxH@FPS` comes from the live `DestinationRuntime`'s
params (the pipeline's real negotiated caps) rather than the `Destination`'s requested
ones, which are `0` — "whatever the source is" — for every destination this stage creates.

**2026-07-29:** a video destination with audio attached renders as **one combined row**, not two: the
audio destination's row is folded into its paired video row (matched via
`DestinationRow.paired_capture_id` — same `capture_id`+`room_handle` pairing
`stream-control.js`'s own destination cards already use to attach an audio row to its
video card), with a speaker glyph appended at the end of the row instead of getting a row
of its own. Stop on a combined row stops only the video destination — `stop_destination`
operates per-destination and a video capture's destinations are independent of its paired
audio capture's, so the audio keeps flowing on its own if the caller wants it stopped
separately. An audio destination with no video row present in the list (audio-only share,
or a stranded pairing) still renders as its own row, unchanged.

The empty state ("You're not sharing anything" + a primary `[+ Start a Stream]` that
switches to Stream Control) is the **common** case on every launch, not an edge case:
nothing about active streams or watches is persisted across sessions.

Capped height with its own scroll, per the plan — multi-destination fan-out makes this
list's length a function of how many rooms each capture feeds, so an uncapped section
could take over the whole rail.

## Content tabs

### Stream Control

**Stage 2 filled this in — see `superdoc/features/stream-control.md` for the feature
detail and its `Why:` notes.** Top to bottom:

1. **Bandwidth** — the Absolute Maximum / Per-Stream Maximum mode toggle (a pair of
   `selectable_label`s; `theme.rs` has no radio-group helper and a two-option exclusive
   choice is what the Settings tab's language switcher already does this way), that mode's
   own settings, the fan-out multiplication note ("streaming to N rooms → ~Nx bandwidth"),
   and the overshoot warning banner (`theme::warning_banner`, new).
2. **Start a stream** — a room picker over *joined* rooms, framerate, and (Per-Stream mode
   only) the starting bitrate. Offering a bitrate in Absolute mode would be a lie: the
   allocator overwrites it within a second of the stream going live.
3. **Active shares** — every outgoing destination across **every** room, each with its
   status dot, live geometry/fps/bitrate, per-destination resolution/framerate/bitrate
   edits behind an Apply button, a "Transfer to…" room picker and an "Also send to…"
   (fan-out) room picker, and Stop.
4. The selected room's **Members**/rename card and **Share audio** card.

`Why` the destination list is cross-room like the sidebar's STREAMS section: a destination
is bandwidth the user is paying for right now, so scoping it to the selected room would
make fan-out invisible.

`Why` a scale percentage (100/75/50/33%) rather than absolute resolution presets: the
source is whatever monitor the user picked and the never-upscale rule means an absolute
preset would silently become a no-op on a smaller monitor and would have to invent an
aspect ratio on an unusual one.

Destination rows show the live `DestinationState` in plain words (`Starting` / `Live` /
"Applying…" for `Rebuilding` / "Waiting for capture…" for `Stalled` / `Failed`) — the
minimal honest surfacing of the stall protocol. The full tiered restart placeholder
(<800ms / 800ms–6s / >6s) is stage 3, on the View tab.

### View

**Deliberately minimal at this stage.** The existing single-selected-watch display,
relocated onto `theme::media_frame` — chrome-free, letterboxed against `PANEL_SUNKEN`,
no card border or title, because a video surface must never read as a settings widget.
Below it sits the stream picker the old connected screen's Streams card was, moved here
where the preview thumbnails it renders are actually useful (watch/stop, listen/stop,
pop out).

Pop-out is **unchanged** — it still marks the `WatchEntry` and lets
`RoomSession::show_popouts` give it a real deferred egui viewport (a real OS window).
Only the button moved.

One small addition beyond a pure relocation: when more than one stream in the room is
being watched, a row of chips selects which one the pane shows. Without it, watching a
second stream would strand the first (the old UI had no way back short of stopping and
re-watching). This is a selector over one surface, **not** the tiled grid — that's stage
3, along with capture-restart placeholder tiers, per-tile hover overlays and live
audio-toggle affordances.

### Settings

The pre-existing settings card (language toggle, presentation-mode dropdown with its
restart-to-apply hint), relocated and restyled, plus a DISPLAY NAME card added
2026-07-28 — see "Global display name" under "+ Create / Join a Room" above.

## Decisions that weren't pinned down by the plan

- **Default nav tab on first launch: Stream Control.** With zero rooms all three tabs are
  empty, so the default should be whichever empty state best answers "what do I do now?".
  Sharing is what the app is *for*; Stream Control's empty state points at the sidebar's
  "+ Add Room" and is the same place the STREAMS empty state's "+ Start a Stream" lands
  you, so first launch and first share converge on one screen. View's empty state
  ("nobody's on screen") reads as a dead end on a fresh launch, and opening into Settings
  implies configuration is a prerequisite, which it isn't.
- **Auto-generated saved-room names (Join tab), unless the user types one.** The Join tab
  now has an *optional* Room Name field (added 2026-07-28, see "Field reorder + Room Name
  on Join tab" above) above the join-link field; leaving it blank falls back to the
  original behavior — a successful join there saves under `Room
  <first 4 bytes of the room id in hex>`. Renaming lives on Stream Control, where the old
  "save this room as" control already was, for either case. The Create tab (added
  2026-07-28, see "+ Create / Join a Room" above) still requires its own Room Name field —
  the Join tab's version is deliberately optional instead, to preserve the original
  no-name-required join flow for anyone who doesn't want to bother naming the room up
  front.
- **`active_room` survives this stage.** The sidebar is genuinely multi-room; the content
  *tabs* are not yet, because being multi-room is precisely what stages 2 and 3 build. So
  one designated room drives both tabs, re-pointed by clicking a room row (and by
  watching from one, which also switches to the View tab — otherwise the click would look
  like it did nothing). Nothing in the data model depends on `active_room`; deleting it is
  all that's needed once both tabs can show every room at once.
- **Icon glyphs are restricted to what egui's *bundled* fonts actually cover.** The Geist
  Mono Nerd Font icon fallback the styling guide calls for still isn't installed (see that
  guide's "Fonts" status note), so anything from a richer icon set renders as tofu today.

  `Why:` "it's in the Geometric Shapes block" is **not** a sufficient test — egui bundles
  only the subset its own widgets draw from. Verified by rendering each candidate in the
  real GUI and screenshotting: `▶` U+25B6 and `■` U+25A0 render, but `▸` U+25B8, `▾`
  U+25BE, U+25BC, U+25BD, U+25BF, U+25B3 and U+25B4 all come out as **tofu boxes**. The
  room-row carets are therefore `⏵` U+23F5 / `⏷` U+23F7 (Miscellaneous Technical), which
  render, are a matched pair at caret size, and stay visually distinct from the `▶` watch
  glyph. Check any new icon this way before adding it.
