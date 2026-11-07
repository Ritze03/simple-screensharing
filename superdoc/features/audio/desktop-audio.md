# Desktop audio, audio-only shares and exclusions

What a share captures (Desktop audio or one named app), source groups, the exclusion list, the audio settings menu and notification sounds (tasks I7, I9). Hub: `../audio-sharing.md`.

## 2026-11-04: desktop audio, audio-only shares, the exclusion list, source groups, the audio settings menu (task I7)

User requirements (D1, the plan's own words) and the design decision (D9):

- The audio button turns on **Desktop audio** with one click and off with the next. Its arrow opens the source list; at
  the bottom **All audio settings…** opens a menu of its own (source, Boost, the exclusion list; Boost left the general
  Settings). Every dropdown in it is the app's own menu style (`ui/menu.rs` `select`: icons, sub-lines, under the field,
  as wide as it).
- Sources are listed under headings: **Games, Browsers, Media, Voice chat, Devices** (and Other).
- An **exclusion list** (Add: pick one of the sources detected right now or type any text; every entry has a remove
  button; **Discord and TeamSpeak are on it by default**). An entry matches any source whose name contains it, ignoring
  case: that source stays out of desktop audio and off every source list.
- Sharing **only audio** must work with no screen share (board #102).
- D9: *"Desktop audio links every app into the share sink except excluded ones, through the existing `link_poll_loop`;
  audio-only shares replace the 'paired video' invariant in `start_audio_share` — a plain output monitor couldn't leave
  Discord out. Desktop audio, source groups and the exclusion list are native Linux only: a browser can't list apps,
  and Windows has no audio capture."*
- **Browser twin (I10):** the website has the audio button too, as an audio-only share of a picked tab or the system audio (`docs/js/ui/capture.js` `startAudio`) and the Listen tile for an audio-only sender; it has no Desktop audio, no groups and no exclusion list. Details and the browser's notification sounds: `features/website.md` "Webcam, audio-only shares and webcam in picture" and "Sounds".

### What a share captures (`audio.rs`)

`AudioChoice::Desktop | Named(name)` replaces "the selected `AudioSource`". Both go through the same 1 s
`link_poll_loop` and `link_diff`; only the matching set differs (`wanted_ids` over one `pw-dump` snapshot):

| choice | set linked into the share's sink |
| --- | --- |
| `Named(n)` (#105, unchanged) | every `Stream/Output/Audio` stream whose name is `n` |
| `Desktop` | every `Stream/Output/Audio` stream |

minus, for both: **excluded streams** (`Stream::is_excluded`: the exclusion list is matched against the display name,
`node.name`, `application.name` and `application.process.binary`, ignoring case, blank entries ignored), **this client's
own streams** and the share's own sink. For Desktop also the **plumbing** (EasyEffects' output stream: it sits between
an app and the speakers, so capturing it next to the app's own stream would play everything twice).

- **Our own playback.** A viewer in this same client plays through `autoaudiosink`; a Desktop share that linked that
  stream would feed the room to itself. `streams()` leaves out every stream whose `application.process.id` is this process
  (`std::process::id()`); the pid is read from the stream node or, when only the client object has it (java), from the
  `Client` object named by `client.id`. Never `pipewire.sec.pid`: for a pulse client that is `pipewire-pulse`. Also true
  of the source list, so a viewer's playback is not offered either.
- **Exclusion changes apply to a running share.** `Exclusions` is an `Arc<RwLock<Vec<String>>>` shared by the settings and
  every share (`AppState::audio_exclusions`); the poll re-reads it each tick. `sync_links` now also **unlinks** a stream
  that is linked, still in the graph and no longer wanted (`unlink_node_from_sink`: `pw-link -d` of the links from that
  node into our sink only; the app's own routing is untouched). Without that an exclusion added mid-share would only take
  effect for streams that appear later. If the app on the list is the share's own named app, the share switches to
  Desktop (which leaves it out) instead of going quiet (`UiAction::SetAudioExclusions`, replica `addExcl`).
- **Why a name list and the unlink instead of a monitor of the output:** a plain monitor of the default sink is the whole
  mix and "couldn't leave Discord out"; linking per stream can.
- **Unchanged and still guarded:** the private null sink (#121), `wait_for_capture_link` (the capture can never fall through
  to the microphone), unlink-before-removal when a share stops (#123), the system clock (#128), no dither (#129).

### Source groups (`audio.rs` `classify`)

`classify(&GroupInput { media_role, node_name, app_name, binary, has_steam_env, device }) -> AudioGroup`, pure and
unit-tested (`desktop_audio_tests`). First match wins:

| # | rule | group |
| - | ---- | ----- |
| 1 | a device's stream: `node.name` starts with `bluez_`, or `device.api` = bluez5, or any `api.bluez5.*` prop | Devices |
| 2 | `media.role` = Game | Games |
| 3 | `media.role` = Communication or Phone | Voice chat |
| 4 | a whole word of Discord, Vesktop, TeamSpeak, Mumble, Zoom, Skype, Teams in the binary, application or node name | Voice chat |
| 5 | ... of Firefox, Chromium, Chrome, Zen, Brave, Vivaldi, Opera, LibreWolf, Waterfox, Edge, Epiphany, Falkon | Browsers |
| 6 | ... `gamescope` | Games |
| 7 | ... of mpv, VLC, Rhythmbox, Celluloid, Totem, Clementine, Strawberry, Audacious, Elisa, Amberol, ... | Media |
| 8 | `SteamAppId` in the process environment | Games |
| 9 | `media.role` = Music, Movie or Video | Media |
| 10 | anything else | Other |

**This app's own streams are shadow-excluded** (`audio.rs` `streams()`, `OWN_APP_NAMES`): any Output stream whose display name, `node.name`, `application.name` or process binary contains `simple-screensharing` (or `simple_screensharing`, the cargo test binaries), ignoring case, is dropped from every consumer (source lists, the Add popover's detected sources, `wanted_ids` for Desktop and Named). This catches another instance, a pop-out or an older build; this process's own streams were already dropped by pid. It is not in `AppSettings.audio_exclusions`, so the visible list and its "minus N excluded" count stay the user's, and it cannot be removed. Real names seen in a VM: the viewer's `autoaudiosink` stream has `application.name`, `application.process.binary` and `node.name` all equal to the binary name (`simple_screensharing_client-<hash>` for the test binary, so `simple-screensharing-client` for the app), `media.name` `Playback Stream`. Verified with `desktop-audio.sh desktop`, which now also runs an emulated `simple-screensharing-client` source: it plays to the sink but never appears in the `linked [...]` line. *Why:* user, 2026-11-04: "The audio stream "simple-screensharing" should always be excluded from the audio (add to the list, but not visible). So shadow disable it for desktop streaming and dont show it in the GUI sound dropdown".

*Why games are detected and never named* (user, 2026-11-04): a list of titles is never complete. `SteamAppId` in the
environment means a Steam game, native or through Proton (confirmed: Skill Up! via Proton, cs2 native), and `media.role`
Game means a game; neither says which. A wine binary alone is not a game (`wine64-preloader` without `SteamAppId` is Other);
`java` is a game only when its `media.role` is Game, as Minecraft's is (the name stays `java`); any other java is Other. The
command line is never read (it can hold login tokens).

*The environment read* (`process_has_steam_app_id`): `/proc/<pid>/environ` is read only for a stream that rules 1-7
leave to rule 8 (`needs_steam_probe`), once per (pid, binary), on the blocking pool (`list_audio_sources` runs there), and
only the *names* are looked at: each NUL-separated entry is tested for the prefix `SteamAppId=` and dropped; no value is
copied, kept or logged (the environment can hold secrets). Known binaries (rules 4-7) win over the environment so a
browser is a browser and is never probed. Whole words, not substrings, so `zen` does not match `zenity`.

The list is de-duplicated by name (#105) and ordered by group; each row shows the group's icon and a sub-line ("Game",
"Browser", "Media player", "Voice chat", "Device", "App"). Replica differences: no "Steam game" / "Gamescope" sub-lines.

### Audio is a share of its own (`session.rs`, `ui/`)

- `AppState::start_audio_share(room, for_capture: Option<..>, choice, muted)`. `for_capture` is `None` for an audio-only
  share and what the control bar always passes: audio is independent of the screen (replica: stopping the screen leaves
  the audio on). `Some(video)` still pairs a share with a live capture (the liveness check stays: a pairing to a staged
  capture is rejected) and then Stop on the video stops the audio too. The old hard invariant "no audio-only streams,
  ever" (every audio share needed a live video capture) is gone: the wire already carried audio without video and a
  viewer already lists such a sender with Listen (I1).
- One audio share per room, found with `AppState::audio_share_in(room)` (a `Stopping` one loses to a running one, #115).
  Choosing another source replaces it and keeps it muted if it was (#118). `selected_audio_source` and `StagedShare::audio`
  are gone: a pick acts at once, staged screen or not (also the stream settings' Audio row).
- **Reconnect** (`SuspendedShare::Audio { for_capture: Option<..>, choice, muted }`): an audio-only share is restarted on the
  new connection with the same choice. **Leave** with an audio share asks like with a screen ("Your audio to X stops.").
  **Stop all sharing** stops it. A start still in flight is torn down on arrival (`stop_share_one`), so Stop leaves no
  sink behind (`live_audio_only_share_through_the_app_state` asserts it).
- **Not done (replica does not either):** no own tile for an audio-only share on the stage (the audio button is the
  indicator: green with the source in its tooltip; the sidebar user band says "Sharing audio" and the Outgoing popover lists
  it, click opens the audio settings); `MyPhase` / "Live in N rooms" stay about screens.
- **Settings key:** `audio_exclusions` (`AppSettings`, array of strings). A file without the key gets
  `["Discord", "TeamSpeak"]`; a saved empty list stays empty so the user can remove both.
- **UI** (`ui/audio_settings.rs`, `ui/share.rs`, `ui/stage/bar.rs`): the audio button (one click = Desktop audio on/off;
  green + `music_note` when on, tooltip names the source), the dropdown (No audio, Desktop audio with "minus N excluded",
  groups with live meters, mute while sharing, All audio settings…), the modal (Source, Volume/Boost, Desktop audio/Exclude)
  and the Add popover. Dump names in `drive_dump`: `audio.source`, `audio.boost`, `audio.excl_add`, `audio.excl.<i>(.remove)`,
  `audio.excl_text`, `audio.excl_add_text`, lines `boost`, `excl`.
- **Windows:** all of it is Linux only. `audio.rs` still compiles there (std only, the existing `cfg` for `die_with_parent`;
  `/proc` is simply absent), but Windows has no audio capture, so nothing starts one. The browser cannot list apps.

### Measured (Fedora 44 VM, PipeWire 1.6.2, WirePlumber 0.5.13, Firefox 150; real in-process sharer and
viewer through a real relay; `desktop-run.sh`, `tones.py`)

Sources: Firefox playing a 440 Hz tone (real, MPRIS), an emulated app called **Discord** at 880 Hz (`media.role=Communication`,
`emu_src.py`), plus quiet fake streams for the groups. Desktop audio, exclusion list `["Discord"]`; the share sink's own
monitor (what is in the share), per second:

| time | event | 440 Hz (Firefox) | 880 Hz (Discord) |
| --- | --- | --- | --- |
| 0-11 s | Discord excluded | -21.1 dBFS | **-99 (absent; at the noise floor)** |
| 12 s | entry removed during the share | -21.1 | **-6.0** (linked at the next poll) |
| 24 s | entry added back | -21.1 | **-99** (unlinked: log `stream node 77 is no longer wanted (excluded); unlinked 2 link(s)`) |
| 36 s | "Firefox" added too | **-99** | -99 (rms -58 dB: only the quiet fake streams) |
| 48 s | "Firefox" removed | -21.1 | -99 |

The viewer's recording (`viewer-out`'s monitor) agrees: 440 Hz and not 880 Hz in the first stretch. **Firefox's
`PlaybackStatus` stayed `Playing` the whole run**, through every unlink and when the share stopped at the end (#123's
unlink-before-removal holds for Desktop audio, and unlinking one stream does not make WirePlumber pause the player).
- **Viewer's own playback not captured:** the viewer's stream (`simple_screensharing_client-<hash>`, `Stream/Output/Audio`,
  linked to `viewer-out`) is in the graph and never in `linked [...]` (the log prints `audio_handle.linked_stream_names()`
  every second); `list_audio_sources` does not list it.
- **Groups** (the log's `source` lines, real classification of real streams): `Games / FakeGame` (`media.role=Game`),
  `Games / SteamThing` (process environment `SteamAppId=480`, `media.role=Production`), `Browsers / Firefox`,
  `VoiceChat / Discord` (excluded), `Devices / <phone>` (a `Stream/Output/Audio` node named `bluez_input.AA_BB_...`
  made with `pw-cli`), `Other / PlainApp`.
- **Audio-only share** (`live_audio_only_share_through_the_app_state`, named `<browser>` and Desktop): no screen capture anywhere,
  the viewer sees the audio stream with `owner_has_video` false, 200 packets in ~4 s (50/s), a 440 Hz recording at about
  -17 dBFS rms, and after `stop_share` the capture is gone, no `ssshare-sink-` object is left in `pw-dump`, the relay
  announced the stop and the room forgot the destination. A named share (`<browser>` 440 Hz with Discord on and off the list)
  carries only the Firefox-based browser: the old #105 behaviour is unchanged.
- **Known limit, measured:** while the only streams linked into the sink are suspended ones (the fake phone node, a paused
  device) no packets are sent (`viewer packets` stood still for 11 s after the last active stream was unlinked, and resumed
  at once when Firefox came back). With *nothing* linked at all the share sends ~50 packets/s (measured); an idle
  linked stream (a paused Firefox) too (#129). Nothing audible is lost; only the packet counter stands still.

### Tests

Unit (`audio::desktop_audio_tests`, `rooms` and `ui` tests): the group table with the brief's real examples (a music player,
the Firefox-based browser/Chromium/Zen, gamescope, `Skill Up!.exe`/cs2 with and without `SteamAppId`, java with and without the Game role,
Discord, TeamSpeak, a Bluetooth phone), that the environment is read only for streams no known app claims, the `SteamAppId`
prefix test, the exclusion match (case, blanks), the matching set of Desktop vs Named, the stream list of a snapshot (pid on
the client object, a string pid, own streams dropped), the links to unlink, the settings default (old file, empty list).
Live (VM only, `#[ignore]`): `live_audio_share_soak` (new knobs `SSS_LIVE_CHOICE=desktop`, `SSS_LIVE_EXCLUDE`,
`SSS_LIVE_EXCL_AT`) and `live_audio_only_share_through_the_app_state`; harness `client/scripts/vm-audio/desktop-run.sh`
(host) / `desktop-audio.sh` (VM; scenarios `desktop`, `named`, `empty`, `only`), `tones.py` (Goertzel per second),
`emu_src.py` got VOLUME and PROPS arguments (volume 0 makes the stream vanish: use 0.001). In the rig
(`SSS_UI_FAKE_AUDIO_SOURCES=1`: the replica's examples through the real grouping, level taps and sharing switched off so
the rig creates no audio) the menu, the groups, the modal, Add and remove (settings.toml checked) and the Boost drag were
driven for real.

### Notification sounds (task I9, same day)

User requirement (D1): four different sounds, *someone joins my stream, leaves my stream, someone starts a stream, someone stops a
stream*, each with a Test button, and one volume slider in the general settings, separate from the stream volumes. Hard rule (D13): they
are checked by unit tests only and **never played on the host**; the real listen is the user's own check.

**What and where.** No sound files: the four sounds are generated from the replica's definitions (`superdoc/ui/replica/index.html`
`SOUNDS` / `playSound`), the same table in three places that must stay equal: native `client/src/sounds.rs`, browser
`docs/js/ui/sounds.js`, the replica. join = sine 659 + 880 Hz, leave = sine 880 + 587, start = triangle 523 + 659 + 784, stop = triangle
392 + 294; a note every 90 ms, each 240 ms long with an exponential attack to the peak in 12 ms and release to -80 dB at 220 ms;
`peak = volume / 100 * 0.3`. Settings > **Sounds** holds the volume (0-100, step 5, default 50, `AppSettings::sound_volume` /
browser `soundVol` in `settings`; 0 = silent and creates no pipeline) and a Test button per sound. *Why a 0.3 cap:* a notice, never a
shout; at 100 % a note peaks at -10 dBFS. *Why its own slider:* a notice must not follow the volume of whatever is being watched.

**Native playback** (`sounds::play`): `synth(sound, volume)` is a pure function to 48 kHz mono f32; one short burst
`appsrc ! audioconvert ! audioresample ! autoaudiosink` (element replaceable by `SSS_SOUND_SINK=<element>`, the rig sets `fakesink`) on its
own thread, one buffer, EOS, dropped at the end; a failure is logged once and ignored. Guards: a `Gate` allows one burst of a sound at a
time and the same sound again only a second after its last start (8 members joining over a second are one sound), and
`RoomSounds::drain` yields each sound at most once per event pass. *Why output is off until `ui::App::new` calls `sounds::enable_output()`*:
the `ui` tests run the real `AppState` and `UiAction::TestSound` with the lib compiled without `cfg(test)`, so a `cfg(test)` switch would
not have covered them; the only way no test ever plays is that only the real binary can turn playback on. The Settings slider acts during the drag
(`PreviewSoundVolume`, applied through `sounds::set_volume`) and saves once on release (`SetSoundVolume`), the Boost slider's pattern.
*Not in the user's own Desktop audio share:* the notification stream is created by this process, and `audio::streams()` drops every
stream whose `application.process.id` (also read through the PipeWire client object) is this process, so it is never offered or captured;
this was already covered for the viewer's own playback and nothing new was needed.

**The edge rules** (`sounds::RoomSounds` native, `RoomSounds` in `sounds.js` browser, the same tests in both):
- *Someone joins / leaves my stream:* from the relay's viewer lists (spec 9.1, `RoomSession::viewers` / the core's `ShareHandle.viewerIds`),
  compared **as sets** once per event pass, because the relay repeats a list every 2 s. A person counts across all my streams in the room
  once (watching the screen and the webcam is one join; stopping one of the two is no leave). An old relay sends no lists: these two
  sounds never fire.
- *Someone starts / stops a stream:* native: the `StreamStarted` / `StreamStopped` deltas (a started stream the roster already has, a label
  change, is silent; so is a stop for a stream the roster never had); browser: the difference of the stream keys between two `streams`
  events. A webcam and a screen are two streams, an audio-only share is one; a screen share with its paired audio arrives as two and the
  gate makes it one sound.
- *Never for:* my own streams; the snapshot at join (native: it is the baseline, not an event; browser: the snapshot arrives while the room
  is not yet `joined`, so it only sets the baseline); my own stream stopping (the viewers that go with it are not "leaving": nothing sounds for
  the people of a stream that disappeared from my streams since the last pass); a resumed room, which stays quiet for 15 s
  (`RESUME_QUIET`) after a reconnect while its baselines still follow, because a restarted relay has everybody re-announce and re-watch.
- A room counts while it is joined, on screen or not, focused or not (the user asked for no filter).
*Deviation from the brief, "not the first list after I start sharing":* built as "the baseline of a new stream is nobody". A first list that
names someone is a real join (nobody can watch a stream before it is announced), so silencing it would lose the first viewer's sound; the
list the relay sends right after an announce is empty and is no change.

**Browser** (`docs/js/ui/sounds.js`): `OscillatorNode` + `GainNode` with the replica's ramps. The `AudioContext` is created only when a sound
is actually played, never at boot or on a click. Browsers block audio until the page has had a click or key press: an event sound arriving
before that is **dropped** (a late sound would be wrong; `navigator.userActivation.hasBeenActive`), from the first click on they play; a Test
button is itself a click and always plays. `SoundOut.play` is the single exit and is replaced by the harness.

**Tests** (nothing plays): `sounds::tests` (four sounds differ, first-note pitch by zero crossings, peak follows the volume within the cap,
starts at 0 and ends at the -80 dB floor with no step, the edge detector over sequences, the gate, a burst reaching EOS through a `fakesink`),
`rooms` (old settings file gets 50), `ui::tests::sound_volume_...`; `session::reconnect_e2e::notification_sounds_follow_the_room_events`
(`#[ignore]`, real relay, two `AppState`s, counts `sounds::requested`: one start for the watcher, one join, one leave, one stop, nothing for
the sharer's own start or stop, no leave when the stale window passes). JS: `dev/tests.js` "sounds" group (definitions, plan, the same sequences,
gate, setting). Real pages (headless Chromium, `SoundOut.play` replaced, `window.AudioContext` trapped): the sequence above through a
throwaway relay. Rig: the Settings card, a drag (live `Preview`, `Set` on release, `settings.toml` written), the four Test buttons with
`SSS_SOUND_SINK=fakesink` (no audio stream in the rig's PipeWire). New dump rects: `side.settings`
(sidebar gear), `sound.volume` and `sound.test.<join|leave|start|stop>` are new dump rects, `sound_volume` a new value line.
