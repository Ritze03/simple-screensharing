# Settings and viewer-side controls (native egui client)

The Settings modal's cards (Logs, Relay diagnostics, Sounds, Avatar), the Boost and Level loudness controls, and the native Playback sliders: what each does, and why. Split out of `APP-SHELL.md` on 2026-11-07 (text moved verbatim); the modal's tabs are summarised in "Design" there.

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

**Boost slider** (2026-11-03, task #114; since #119 without the Calibrate row and capped at 200 %; **moved on 2026-11-04 (I7) from Settings > Shared audio to the audio settings menu's Volume card**, the Settings card is gone): a `ctrl_row_value` (Boost: `widgets::slider`
in dB, 50-200 %, 100 % mid-track, 0.5 dB knob steps, a `value_box` reading `150%`; applied live on every changed frame of a drag as
`UiAction::PreviewAudioGain`, saved on pointer release as `UiAction::SetAudioGain`, #135). The calibration was removed in #119 together with `UiAction::CalibrateAudio` / `CancelAudioCalibration`.
How it works, and why: `features/audio/levelling-and-volume.md` "sender-side audio boost" and the #119 block. Tests: `ui::tests::audio_boost::*`. Rig dump: `audio.boost`, `boost N`.

**Level loudness** (2026-11-03, task #119; user: "the auto levelling should happen on the viewers
side"). Since 2026-11-04 (row 134) a **control-bar button** (below); it was a checkbox on the Playback tab. **On by default**; `UiAction::SetLevelLoudness` -> `AppState::set_level_loudness` (persists
`level_loudness`, flips the process-wide `leveller::ENABLED`, live for every watched stream). The browser has the same
bar button (`#btn-level`). Design and numbers: `features/audio/levelling-and-volume.md` (2026-11-03, #119). Tests:
`ui::tests::audio_boost::level_loudness_defaults_on_persists_and_drives_the_leveller_switch`.

**Settings > Sounds** (2026-11-04, task I9; `modals.rs`): a card with one **Volume** row (slider 0-100 %, default 50, applied live while dragged, saved on release; `UiAction::PreviewSoundVolume` / `SetSoundVolume`) and one **Test** button per notification sound (join / leave my stream, someone starts / stops a stream; `UiAction::TestSound`). Separate from every stream's volume. Design, edge rules and why: `features/audio/desktop-audio.md` "Notification sounds". Rig dump: `sound.volume`, `sound.test.*`, `sound_volume N`.

**Level-loudness button next to every viewer volume slider** (2026-11-03, task #126; user: "make sure the dynamic volume is
something the viewer can actually select, whether he wants it or not ... next to the volume slider, have a little button
that indicates the dynamic volume setting"). The audio pill is `[25 mute][110 slider][34 %][25 level]` (pop-out / fullscreen pill in `popout.rs`; hidden with the slider while
muted). Since task I3 the stage's tiles have no pill: the same global switch is the bar's **Level loudness** button. The button is
`ui::level_toggle` (25 px round, `graphic_eq` icon): **on** = accent ring + tinted disc + light-blue icon, **off** = faint
icon; tooltip "Level loudness: on/off" (`ui.room.level_on/off`). A click sends `UiAction::SetLevelLoudness(!on)`, the same
action and the same persisted global `level_loudness` setting as the bar button, so both stay in sync and it is live.
In a pop-out viewport the click goes through `Shared::pending_level` (like `pending_volume`) to the main window. *Why one
global switch, not per stream:* the user asked for the *setting* to be visible and reachable where the volume is, not for a
per-stream mode; per-stream state (storage per owner like `audio_volume_by_owner`, a flag on each decoder) is possible if
it is ever wanted. The browser had the same button on its tile pill (`levelToggle` in `app.js`, `.audio-pill button.level`); since I6 its tiles have no pill either and the switch is the bar's **Level loudness** button (`#btn-level`, `docs/js/ui/bar.js`).
The viewer volume maximum stays 500 % (the same request first asked to cap it at 200 %, then the user said "leave it at 500%").
Tests: `ui::tests::level_toggle_reports_a_click_only_on_the_button`; gallery card "Viewer volume" shows both states.
*Geometry (2026-11-03, #130; user: "The dynamic volume button should be all the way in the right half-circle"):* the pill
pads 2 px on both ends while the slider shows (10 px on the right only after the "Muted" text), so the 25 px mute and level
buttons are concentric with the 29 px pill's half-circle ends, 2 px in all round. Browser: 1 px border + 1 px padding
(`.audio-pill`, `.audio-pill:not(.muted)`; the pill is gone from the browser since I6, the geometry now only applies to the pop-out's pill).

**Settings > Diagnostics > Relay: "Send diagnostics to the relay"** (2026-11-03, task #110; user: "he or all of my
friends could just tick the checkbox while we are still in heavy development, and then I can just see it.
Everyone knows this data is being collected."). A checkbox row (`ctrl_row` with the label "Relay", `widgets::checkbox`)
under the Logs row in the same card; `UiAction::SetSendDiagnostics(bool)` -> `AppState::set_send_diagnostics`
(persists `AppSettings::send_diagnostics`, default **false**, opt-in; flips `diag_upload::set_enabled`). The
explanation is the row's tooltip (the styling guide forbids helper text under options): off by default, sends the
same log text as "Logs" with your display name and no room keys, to the relay of each room you are in, and only a
relay that supports it receives anything. No UI for the state beyond the checkbox (no "uploading" indicator: the
upload is a background courtesy, not something to watch). What goes where, cadence and the wire format:
`architecture/protocol-spec.md` §6.7; the operator's side: `features/relay-transport.md` "Client diagnostics upload".
Tests: `ui::tests::the_send_diagnostics_checkbox_is_off_by_default_and_persists`, `rooms::tests::send_diagnostics_is_off_by_default`;
rig: `HS_SEND_DIAG_<n>=1` seeds the setting (`client/scripts/README.md`).

**Settings > Profile > Avatar** (2026-11-04, task #114): a row under Display name with my avatar as a button; clicking it
opens the picker as a popover in the menu style (`modals.rs` `avatar_row`, `menu::frame()`, closes on a pick or an outside
click): header (my avatar, "Your avatar", "Friends see it instead of your initial"), the 8-wide grid of the 56 emoji (three groups set apart by a small gap: 1 row, 5 rows, 1 row)
(`widgets::emoji_picker`), then "Use my initial" (`text_fields` icon). A choice is `UiAction::SetEmoji(String)` ("" = clear),
applied in `apply.rs` through `AppState::set_emoji` (saved, announced to every connected room). The list is the user's own choice (2026-11-05, `Why:` they picked these 56, in three groups, replacing the replica's 40). The "Paste any emoji" field is there too (row 120): any emoji of one grapheme, at most 16 bytes, that shapes to one bitmap glyph in the embedded Noto font can be drawn, not only the 56.

## Level loudness: a control-bar button, the native Playback tab removed (2026-11-04, row 134)

User: "Maybe remove the playback tab and move level loudness (since its global) to the left of the "Hide the people"
button?" **What:** the native Settings modal had three tabs (Profile, Interface, Diagnostics; `SETTINGS_BODY_H` unchanged; superseded in part by row 136 the same day: Playback came back, four tabs again, next section);
Level loudness is a `graphic_eq` `group_btn` (rig rect `ctl.level`) in the Layout / Room settings / People group,
directly left of the people button, in a room and in Watched (it is global like the people toggle). Accent when on;
click sends `UiAction::SetLevelLoudness(!on)`; the tooltip says "Level loudness: on/off" plus the explanation
(`ui.bar.level_loudness_on/off`, `ui.settings.level_loudness_tip`). The stream menu has no Level loudness row (global,
not per stream: the same reasoning as Show stats moving to the Layout menu). **Why:** a global setting does not belong on
a tab of its own, nor in a per-stream menu; the bar is where a viewer reaches it at any time.
**Narrow windows:** the extra 48 px makes the middle part 548 px, wider than the 800 px window's 528 px stage minus
padding. `arrange` then sets `Arrange::leave_down`: still two rows, but Leave moves to the second row, between the side
buttons (40 px high there, not group height). Test: `bar::tests::the_label_goes_first_then_the_bar_takes_two_rows`.
The browser keeps its Playback tab (the jitter and catch-up sliders work there) minus the Level loudness row; its bar
wraps by CSS so it needed no rule.

## Native Playback: jitter buffer and catch-up sliders removed (2026-11-04, #125), restored with a real buffer (row 136)

The native viewer had no playout pacing, so the Settings > Playback sliders "Jitter buffer" and "Catch-up speed" did
nothing. **Why removed:** the user was told so and said "Remove them", under their rule "If a setting doesn't do
anything, hide it" (#125). **Why back (row 136, the same day):** the native viewer got a playout buffer
(`decode/playout.rs`, `features/video/playout-buffer.md`); the user: "Also add the settings back to the
Settings menu." Restored: both sliders (Settings > **Playback** tab, between Interface and Diagnostics; the tab row is
four `FILL` slots, rig rects `settings.tab.profile|interface|playback|diagnostics`), `UiAction::SetJitterBuffer` /
`SetCatchUpSpeed`, `AppState::set_max_jitter_buffer_ms` / `set_catch_up_speed_percent`, `AppSettings.max_jitter_buffer_ms`
(default 2000, 0-5000) / `catch_up_speed_percent` (default 5, 5-10), the locale keys (the browser's words, German as in
the browser). Both setters also call `decode::playout::set_limits`, which every running stream reads on its next frame
(the sliders push on release, as before). An old `settings.toml` with the keys keeps them; one without gets the
defaults. The tab holds only these two sliders, so `SETTINGS_BODY_H` (set by the Interface tab) is unchanged. Level
loudness stays on the control bar (above).

## Native Playback: "Automatically watch new streams" (2026-11-11, row 175)

Settings > **Playback** > card "Playback", row "New streams" (checkbox "Automatically watch new streams", rig rect `settings.auto_watch`), between the
Jitter buffer and Catch-up speed sliders. **On by default.** `UiAction::SetAutoWatch` -> `Shell::auto_watch`, saved as `AppSettings::auto_watch`
(`#[serde(default = true)]`, so an older `settings.toml` is on). Locale keys `ui.settings.auto_watch*` (en, de).

**Behaviour.** Before this nothing watched on its own: a stream was listed and the viewer clicked it (only a sender's audio
follows their video watch, `sync_audio_watches`). Now `App::auto_watch_new_streams` (`ui/mod.rs`, right after
`drain_events`) compares each joined room's roster with `AutoWatchSeen` and calls `RoomSession::toggle_watch` for each
stream that is new: **video streams only** (screen or webcam); audio-only streams are never auto-listened (same as the
browser, `features/website/auto-watch.md`; a sender's audio still rides along with the video watch). Rules:
- **Nothing is watched while you are not in the call** (row 192): `auto_watch_new_streams` skips the room (and marks nothing
  seen). **Picking up** with the option on watches **every video stream already running** (screens and webcams, never
  your own, never audio-only; `App::pick_up_room`); off, they are listed with Watch buttons. This replaces the earlier
  rule "join list: webcams watched, screens only listed" (row 181). *Why (user, D7):* "press on this new button to actually
  watch all of the screen shares if it is automatic for him"; the old rule was a judgement call against surprise decoding, and
  pick-up is now the explicit consent. Your own streams are never watched; a stream already watched is left alone
  (`toggle_watch` would otherwise stop it).
- The roster is recorded **also while the option is off**, so switching it on does not pull in what was already there.
- "Seen" is the roster of the last pass by `(owner, stream id)`: a stream that keeps running is never started twice, so a
  watch you stopped stays stopped; a stream that stops and starts again is new. A **reconnect** (own `ConnId` changes) is a
  new baseline: re-announced streams are listed, not watched here, so a webcam stopped by hand stays stopped. Reconnect restoration of what you watched (`Wanted stream`,
  `RoomSession::apply_rewatch`) is separate and works whatever this says.
- Off: new streams are listed until clicked, as before.

**History (row 181):** user request 2026-11-11 ("When autoconnect is on and someone connects the room it should automatically
connect to all of the cameras") made joining watch the webcams already there; since row 192 (D7) the pick-up watches every running
video stream instead, and the first roster pass is a plain baseline (`AutoWatchSeen::take_new` returns nothing for it).
Tests: `auto_watch_seen_tests::{baseline_returns_no_webcam_either, a_reconnect_baseline_watches_nothing}`.

**Why:** user request 2026-11-11 ("automatically watch new streams ... on by default"). **Why in the UI layer:** the gate
needs a setting and `session.rs` was held by another row; `toggle_watch` is the public entry the click uses, so a
auto-watched stream is indistinguishable from a clicked one. Pure rule and tests: `ui::auto_watch_seen_tests`,
`rooms::tests::auto_watch_defaults_to_on`. The browser client has its own option (row 176).
