# Share flow and stream settings (native egui client)

How a share starts, what the stream-settings modal shows, the screen-or-gamescope question, stopping, recovering, and the "Live in" toast. Split out of `APP-SHELL.md` on 2026-11-07 (text moved verbatim); the bar buttons and menus that start a share are described in "The control bar" in `STAGE.md`.

## Share state, stop and recovery

- **Share-state drives the control bar's Share button and menu** (`ShareState`: Idle / Picking / Staged / Live /
  Paused). The bar's Share button opens the OS picker and then goes live at once, no modal
  (`UiAction::ShareNow`, 2026-11-05, "The control bar"); only the menu entry "Share your screen..." opens the picker first (capture
  `Starting` = Picking) and the stream-settings modal then by itself.
- **Stop frees the room at the click (2026-11-02, #98).** `AppState::stop_share` removes the
  capture's destinations from the model immediately (the capture stays in `Stopping` only to own
  the teardown), so `share_state` is `Idle`, the bar shows "Share screen" and the sidebar/tile
  stop claiming a live share while a slow driver teardown is still running. *Why:* the old
  behaviour (LIVE and "Stop sharing" for the whole teardown, up to ~15 s on a stuck VA branch)
  was reported by a user as "it just stays open". No separate "Stopping..." indicator: the
  teardown is invisible by design, and the info-level `capture stopped in N ms` log line is how
  a slow one is found. `features/stream/capture-restart.md` "Stop is instant".
- **Recovering (2026-11-02, #99).** While `AppState::recovery(capture)` is `Some`, the sharer's
  pill for that capture's destinations is *Recovering* (`PillKind::Progress`, `ui::share::recovering_pill`, shown in the
  Share menu's header and per-room rows) and the Share menu carries a note made by `ui::share::recovering_tip`: the
  title (`ui.bar.recovering_title`, "Screen capture lost"), the attempt number ("Attempt 2 of 5"), the countdown to
  the next attempt and the last error. Your own tile's chip reads RECOVERING (`ui.room.chip_recovering`,
  `stage/tile.rs`). The old bar title line and `BAR_PILLS` are gone with the bottom bar. Only a capture that actually
  has a recovery shows it: a plain
  `Stalled` destination (a few seconds of silence that heals itself) keeps the *Stalled* pill.
  When the schedule is exhausted the destinations turn *Failed* with a share-error toast; there
  is no Retry button in the egui UI (`retry_destination` is lib-only), so the user stops sharing
  and shares again. The stream-settings modal's per-room rows still
  use `dest_pill` (Stalled / Failed), not the recovering override.

**"Live in <room>" toast fires when a room goes on air, not on Go live** (2026-10-15, task #73).
`App::poll` compares `Cx::on_air_rooms()` with `Shell.announced_live` (`newly_on_air`): a room
that is only Starting, or whose start failed (the destination is forgotten and the error toast
shown), never gets it. *Why:* it used to fire inside `UiAction::GoLive`, so a share that failed to
start showed "Live in Test" above the "Share error" toast (a run on a machine with a missing
GStreamer element). Test: `live_in_toast_fires_once_per_on_air_spell`.

**Note (2026-11-04, plan D8; rewritten 2026-11-07):** the presets are **Text / Quality / Gaming** (they set the FPS once, the encoder mode and the keyframe interval); the Advanced tuning rows (Tune, Quality level, the Advanced disclosure) and the Balanced / Low-Latency presets are gone, and the modal's right-hand cards are Compression and Video (`features/stream/encoder-tuning.md` "Presets", `REPLICA.md` "Stream settings and webcam"). The 2026-11-01 reasoning about what to hide is under History at the end of this page; the rule it came from is "Hide anything that does nothing" in "Behavioural rules that carried over" (`APP-SHELL.md`).

**Video card: Encoding and Keyframe (2026-11-04).** The card's order is Preset, FPS, Encoding, Keyframe, caption (FPS moved under Preset 2026-11-05). Encoding and Keyframe are the two values a preset sets besides the FPS:
**Encoding** (`widgets::segmented`, Performance | Quality, `ui.stream.ql_*`; dump rects `quality.0` / `quality.1`, value `quality`) and
**Keyframe** (`menu::select`, 250 / 500 / 1000 / 2000 ms, `GOP_OPTIONS`; a stored interval outside the list is listed as it is; dump rect `gop`,
values `gop`, `gop_options`). Both edit `NextStream.quality_level` / `gop_millis` through the same `target.action` as the FPS (staged:
`SetNextStream`; live: `EditDestination`, which rebuilds the branch; the head's "Applying" pill shows). A preset sets all of them at once
(`ApplyPresetToDestination` / `ApplyPresetToNextStream`); editing one of them or the FPS by hand so that no preset matches leaves **no preset lit**, because
`EncoderPreset::detect(framerate, tuning)` compares FPS, quality level and keyframe interval (all three exact; since 2026-11-05, before it left the FPS out and 120 fps + Quality + 250 ms lit Text). Picking the matching FPS again relights it. A fresh app (30 fps default, Quality encoding) therefore lights none. The caption stays and never says "(custom)". The webcam's
fixed Performance / 2000 ms stays fixed (its modal has no such rows). Tooltips: Keyframe says a shorter interval recovers faster after a lost
packet or fast motion but costs bitrate (row 129 measures keyframe lag spikes).
*Why:* the user, after the presets replaced the Advanced rows: "The settings for the keyframe and the Performance/Quality setting should still
be shown (right now only accessible through the presets Text/Quality/Gaming itself)." *Why a select and not the old 250-4000 ms slider:* the
presets use 250 and 1000, the webcam 2000, and the useful range is a handful of steps; a dropdown is one row, matches FPS above it, and
keeps the card's height.
*Why (user, 2026-11-05):* "In the settings, of the stream, it should only show a preset, if the settings match it perfectly. Also, FPS should be below the preset line." (a screenshot of 120 fps + Quality + 250 ms with Text lit).

## Share asks "screen or gamescope" when a gamescope node exists (2026-11-05, row 142)

`Modal::SharePick { room, auto, node }` (`ui/modals.rs` `share_pick`, a small question in the `LeaveConfirm` style): body text naming the node, two
body buttons **Screen** and **Gamescope** (dump rects `share_pick.screen`, `share_pick.gamescope`), footer Cancel. Raised by the Share main button
(`auto`) and the menu entry alike, after a per-click `pw-dump` look-up (`Shell::gamescope_probe`, polled by `poll_staged`, repaint every 30 ms while it
runs); no node means no modal and no visible delay. Answers push `UiAction::SharePicked`. Backend and *why*: `features/stream/capture-sources.md`
"Sharing gamescope directly". Rig: `hs-drive.sh sharepick screen|gamescope`, or `SHARE_PICK=` on `share`/`shareauto`.
