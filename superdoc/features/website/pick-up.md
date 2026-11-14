# Website: pick up, leave the call, disconnect (row 197, 2026-11-14)

The browser twin of the native "pick up" (rows 195/196, `superdoc/ui/STAGE.md`, `superdoc/architecture/session-model.md`). Part of the browser client, see `superdoc/features/website.md`.
Terms ("In the call / not in the call", "Pick up", "Hang up vs Disconnect"): `superdoc/meta/TERMINOLOGY.md`. **No relay or wire change**: "not in the call" exists only in the page; the
others see you in the room as before.

Why (user, 2026-11-13/14): "Auto-connect only connects to the room: the user sees who is in it and who is sharing, but is not in the call and watches nothing until they press a green
'pick up' button (the counterpart of the leave-call button)." Every connect starts out of the call: "All connects, both auto and manual". The red button "Dont disconnect the room, only
switch to 'not in call'". Both clients get it and are kept as twins.

## Representation

`RoomEntry.inCall` (`rooms.js`), a boolean next to the unchanged `state` (`disconnected | connecting | joined | reconnecting`). `state === "joined"` still means "the stage shows, the
relay is connected"; `inCall` means "may watch, share, hear sounds". Why a second flag and not a new state: a reconnect (`reconnecting` -> `joined`) must keep the call state, and the
whole stage code already reads `state`.

- **Fresh connect** (`connectRoom`, page-load auto-connect and the manual Connect alike): `inCall = false`. `disconnectRoom` also clears it.
- **Reconnect** (`state` event, `d.reconnected`): `stage-state.js:inCallAfterConnect` keeps the flag (in the call: watches and shares come back as before; not in the call: stays so).
- **`pickUp(r)`** (`rooms.js`, the green button / Room menu "Pick up"): `inCall = true`; the sound baseline and `knownStreams` become what is listed now (no sound burst, no
  auto-watch of it); with "Automatically watch new streams" on, `stage-state.js:pickUpWatchKeys` = every video stream of others already running (screens and webcams, never your own, never
  audio-only) is watched; off, they stay listed with Watch buttons.
- **`hangUp(r)`** (the red button in the call / Room menu "Leave call"): `inCall = false`, `resume` cleared, `capture.js:stopDestsInRoom` (shares into this room only; a capture that also feeds
  another room keeps running; a staged, not yet live share here is discarded and its setup modal closes), every watch and listen stopped (`unwatchStream`, which unsubscribes). The room stays
  connected. `app.js:hangUpRoom` hangs up at once, never with a confirm (row 207: cheap to undo, same as native and the `ctl hangup` hotkey); only Disconnect (`leaveRoom`) asks.
- **Disconnect**: `app.js:leaveRoom` (unchanged flow, strings now say "Disconnect"): Room menu item, and the red button while not in the call (no confirm: nothing to lose).

## What is gated (every "may watch / share / hear" reads `inCall`)

| where | rule |
| --- | --- |
| `rooms.js:watchStream` | refuses while not in the call (one central guard, like native `apply.rs`); `resumeWatches` too |
| `rooms.js` `streams` event | no auto-watch and `knownStreams` untouched while out of the call; `RoomSounds.streams(.., joined && inCall)` so no start/stop sounds |
| `capture.js` `startDest`, `stage`, `startWebcam`, `startAudio` | no share into a room out of the call |
| `bar.js:renderBar` | out of the call: only green `#btn-pickup` (icon `call`), red `#btn-leave` (icon `logout`, tooltip "Disconnect from <room>"), the Room menu (`#btn-room`) and the side buttons; no share, audio, webcam, layout, level or people buttons. In the call the red button shows `call_end`, tooltip "Leave the call in <room>" |
| `stage.js:tileEl` | idle tiles and audio-only tiles have no Watch / Listen / Pop out buttons; right click on a tile does nothing |
| `stage.js:renderStage` | the people strip is shown while out of the call even when the user hid it (`S.stage.stripHidden` is kept) |
| `stage-state.js:shareTargets` (`menus.js:targetsMenu`) | "Also send to" / "Move to" list only rooms that are connected and in the call |
| `app.js:renderHead`, `roomStatusTitle` | status pill "In the call" vs "Connected" |
| `menus.js:roomMenu` | joined rooms: "Pick up" or "Leave call", then "Disconnect" |

The CSS is `.cb.pickup` (green, 64x50, the twin of `.cb.leave`) in `docs/css/app.css`. Icons `call` and `logout` are ligatures of the bundled Material Symbols font, like `call_end`.
New i18n keys (en + de, wording copied from native): `ui.bar.hang_up`, `ui.bar.hang_up_item`, `ui.bar.pick_up`, `ui.bar.pick_up_item`, `ui.bar.disconnect_room`, `ui.room.in_call`,
changed: `ui.modal.leave_title|leave_room` and `ui.bar.leave_room_item` is renamed `ui.bar.disconnect_item` (en "Disconnect", de "Verbindung trennen", as native); `ui.bar.leave_room` is gone.

## Differences from native

- **Sound edges at pick-up**: native `session.rs:pick_up` discards pending sound edges (`RoomSounds::drain_for`); the browser only re-bases the stream key set (`RoomSounds.streams(.., false)`).
  Equivalent today: a room out of the call has none of my shares, so no viewer edges exist, and the stream edges are not recorded while out of the call.
- **Red button while Connecting / Reconnecting**: native shows the stage there and the red button is Cancel; the browser has no stage in those states, so the Room menu / sidebar act instead.
  The side buttons stay in both.
- **Reconnect state**: the browser has no reconnect state object; the flag lives on the `RoomEntry`, which survives a reconnect. A fresh `connectRoom` clears `resume` (native drops it
  with the session), so a stale resume cannot re-watch without a click after pick-up with the option off (2026-11-14, QC).

## Tests

`dev/tests.js`: `pickUpWatchKeys`, `shareTargets`, `inCallAfterConnect`, `RoomSounds` pick-up baseline. `dev/e2e/pickup.mjs <relay host:port> <certhash> [--relay-bin <bin> --relay-dir <dir>]`
drives the real UI (video only, a spy where a sound would play): not in the call (people strip, idle tile without Watch, no share button, green and red buttons, nothing watched, hidden strip
shown), pick-up watches and decodes with no sound, hang-up (no confirm, with a watch running) keeps the room and the person in the sharer's strip, red outside the call disconnects without confirm, manual
Connect starts out of the call, option off lists with a Watch button, with the restart option a reconnect keeps both states, the sharer's hang-up stops her share. `dev/e2e-lib.mjs:waitJoined`
presses pick-up by default (`pickUp: false` opts out). With auto-watch on that watches everything at once, so scripts that click Watch on idle tiles (`stage.mjs`, `native-webcam.mjs`, `phone.mjs`) open
their viewer with `settings: { autoWatch: false }`. `pickup.mjs` also covers sounds: out of the call a stop and restart of the share records nothing in the `SoundOut.play` spy, in the call it records `stop` and `start`.
