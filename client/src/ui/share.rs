//! Sharing helpers of the stage's control bar (task I3; this was the bottom action bar until 2026-11-04).
//!
//! - The bar's **Share screen** and **Share audio** dropdown bodies ([`share_menu`], [`audio_menu`]); the
//!   buttons themselves are `stage/bar.rs`. Share is one body for every [`ShareState`]: the Off / Ready /
//!   Live header (pill, size, codec, rate), FPS and Preset for the stream (live: the destination, else the
//!   defaults of the next one), then what the state offers (share, change source, all stream settings, go
//!   live, stop). *Why FPS and Preset in the menu:* the replica's call, the two settings people change in
//!   the middle of a share; the rest stays in the modal.
//! - Level meters of the audio list run only while that dropdown is open ([`sync_level_taps`]).
//! - Formatting, pills, "Also send to / Move to" lists ([`RoomTargets`]) and popover rows shared with the
//!   stream-settings modal, so the lists can't drift.

use egui::{Align, CornerRadius, Margin, Rect, Sense, Stroke, StrokeKind, Ui, vec2};
use simple_screensharing_client::audio::{AudioChoice, AudioGroup};
use simple_screensharing_client::capture::destination::DestinationState;
use simple_screensharing_client::presets::{self, EncoderPreset, EncoderTuning};
use simple_screensharing_client::protocol::StreamKind;
use simple_screensharing_client::rooms::{BandwidthMode, WebcamSettings};
use simple_screensharing_client::session::{
    CaptureSourceId, Destination, DestinationId, LocalCapture, PendingRole, RoomHandle, ShareHandle, ShareStatus,
};

use super::stream_settings::{Target, preset_caption};
use super::{ConnState, Cx, Modal, ShareState, StageView, UiAction, tr, tr_with};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, FILL, HBox, Pill, PopItem, Slot, fixed, paint_icon, paint_text};

/// Popover width (mockup `.popup`: 340).
pub(super) const POPOVER_W: f32 = 340.0;
/// Width of the control bar's dropdowns (replica `.menu`: 280, the share one 320).
pub(super) const MENU_W: f32 = 320.0;

/// The room a share started from the bar goes to, and where our share in it stands: the open room, or for
/// "Watched" the room we are sharing in (live before staged). `None` while we share nowhere (Watched).
pub(super) fn share_target(cx: &Cx, view: &StageView) -> Option<(RoomHandle, ShareState)> {
    match view {
        StageView::Room(h) => Some((*h, super::share_state(cx.state, cx.shell, *h))),
        StageView::Watched => {
            let all: Vec<(RoomHandle, ShareState)> =
                cx.rooms().iter().filter_map(|r| r.handle).map(|h| (h, super::share_state(cx.state, cx.shell, h))).collect();
            let live = all.iter().find(|(_, s)| matches!(s, ShareState::Live { .. }));
            // Sharing only audio counts too: the audio button stays to turn it off. So does a webcam alone.
            let audio_only = || all.iter().find(|(h, _)| cx.audio_share(*h).is_some());
            let webcam_only = || all.iter().find(|(h, _)| cx.webcam_share(*h).is_some());
            live.or_else(|| all.iter().find(|(_, s)| !matches!(s, ShareState::Idle))).or_else(audio_only).or_else(webcam_only).copied()
        }
    }
}

/// The video capture a share state is about.
pub(super) fn capture_of(state: ShareState) -> Option<CaptureSourceId> {
    match state {
        ShareState::Picking { capture } | ShareState::Staged { capture } | ShareState::Live { capture, .. } => Some(capture),
        ShareState::Idle | ShareState::Paused => None,
    }
}

/// The Share screen dropdown. Returns true when the dropdown should close (a one-shot item was picked);
/// FPS and Preset keep it open.
pub(super) fn share_menu(ui: &mut Ui, cx: &Cx, room: RoomHandle, state: ShareState, out: &mut Vec<UiAction>) -> bool {
    let cap_id = capture_of(state);
    let capture = cap_id.and_then(|id| cx.capture(id));
    let here = capture.and_then(|c| c.destinations.iter().find(|(_, d)| d.room == room).map(|(id, d)| (*id, d)));
    let target = match here {
        Some((id, d)) => Target::Live(id, d.edit),
        None => Target::Staged,
    };
    let v = target.values(cx.state);
    let room_name = cx.room_name(room);

    // Header: [pill][Screen share / size · codec · rate].
    let (kind, pill_t, sub) = match (state, capture, here) {
        (ShareState::Live { capture: id, .. }, Some(c), Some((did, d))) => {
            let (kind, text) = if cx.state.recovery(id).is_some() { recovering_pill() } else { dest_pill(c, d) };
            let size = source_size(c).map(|(w, h)| format!("{w}×{h} · ")).unwrap_or_default();
            let sub = if awaiting_first_frame(cx, id, c, d) {
                tr("ui.bar.waiting_title", "Waiting for the screen to change")
            } else {
                format!("{size}{} · {}", presets::codec_label(d.live_params().codec), fmt_rate(dest_kbps(cx, did, d)))
            };
            (kind, text, sub)
        }
        (ShareState::Staged { .. }, Some(c), _) => {
            let size = source_size(c).map(|(w, h)| format!("{w}×{h} · ")).unwrap_or_default();
            (PillKind::Muted, tr("ui.bar.ready", "Ready"), format!("{size}{}", presets::codec_label(v.codec)))
        }
        (ShareState::Picking { .. }, ..) => (PillKind::Progress, tr("ui.bar.picking", "Picking"), tr("ui.bar.picking_sub", "Waiting for the screen picker")),
        _ => (PillKind::Muted, tr("ui.bar.off", "Off"), presets::codec_label(v.codec).to_string()),
    };
    let pill_text = pill_t.clone();
    let pill = kind.pill(&pill_text);
    let pw = pill.width(ui);
    let row = HBox::new("share_menu_head", 40.0).pad(theme::S2, theme::S2).slots(&[fixed(pw), FILL]).show(ui);
    pill.paint(ui, Rect::from_center_size(row.cell(0).center(), vec2(pw, theme::PILL_H)));
    let c = row.cell(1);
    let top = c.center().y - 15.0;
    paint_text(ui, Rect::from_x_y_ranges(c.x_range(), top..=top + 16.0), &tr("ui.bar.screen_share", "Screen share"), font(Weight::Bold, theme::FS_ROW), theme::TEXT, Align::Min);
    paint_text(ui, Rect::from_x_y_ranges(c.x_range(), top + 16.0..=top + 30.0), &sub, font(Weight::Mono, theme::FS_TINY), theme::TEXT_DIM, Align::Min);

    // Preset (lit only on an exact match), its caption, then FPS (the live destination's, or the next stream's defaults).
    pop_heading(ui, &tr("ui.stream.preset", "Preset"));
    let mut preset = EncoderPreset::detect(v.framerate, EncoderTuning { tune: v.tune, quality_level: v.quality_level, gop_millis: v.gop_millis });
    let preset_pairs: Vec<(Option<EncoderPreset>, &str)> = EncoderPreset::ALL.iter().map(|p| (Some(*p), p.label())).collect();
    egui::Frame::NONE.inner_margin(Margin::symmetric(theme::S2 as i8, 2)).show(ui, |ui| {
        if widgets::segmented(ui, &mut preset, &preset_pairs)
            && let Some(p) = preset
        {
            out.push(target.preset_action(p));
        }
    });
    let row = HBox::new("share_menu_caption", 18.0).pad(theme::S2, theme::S2).slots(&[FILL]).show(ui);
    row.text(ui, 0, &preset_caption(&v), font(Weight::Regular, theme::FS_TINY), theme::TEXT_DIM, Align::Min);
    // FPS (the live destination's, or the next stream's defaults).
    pop_heading(ui, &tr("ui.stream.fps_label", "FPS"));
    let fps_opts = presets::framerate_options_for(v.framerate);
    let fps_labels: Vec<String> = fps_opts.iter().map(u32::to_string).collect();
    let fps_pairs: Vec<(u32, &str)> = fps_opts.iter().copied().zip(fps_labels.iter().map(String::as_str)).collect();
    let mut fps = v.framerate;
    egui::Frame::NONE.inner_margin(Margin::symmetric(theme::S2 as i8, 2)).show(ui, |ui| {
        if widgets::segmented(ui, &mut fps, &fps_pairs) {
            out.push(target.action(super::NextStream { framerate: fps, ..v }));
        }
    });
    // What went wrong under a live share: the capture is being restarted.
    if let (ShareState::Live { capture: id, .. }, Some(_)) = (state, here) {
        let note = cx.state.recovery(id).map(recovering_tip);
        for line in note.iter().flat_map(|n| n.lines()) {
            pop_note(ui, line);
        }
    }
    pop_sep(ui);

    let mut picked: Option<Vec<UiAction>> = None;
    match (state, cap_id) {
        (ShareState::Idle, _) => {
            let (items, _) = elsewhere_items(cx, room);
            for (c, add, mv, mv_sub) in &items {
                if widgets::pop_item(ui, &PopItem::new("add_to_queue", add).sub(&tr("ui.share_menu.add_sub", "Same capture, one more room"))).clicked() {
                    picked = Some(vec![UiAction::AddDestination { capture: *c, room }]);
                }
                if widgets::pop_item(ui, &PopItem::new("swap_horiz", mv).sub(mv_sub)).clicked() {
                    picked = Some(vec![UiAction::MoveCaptureHere { capture: *c, room }]);
                }
            }
            if !items.is_empty() {
                pop_sep(ui);
            }
            let label = if items.is_empty() { tr("ui.share_menu.share_here", "Share your screen…") } else { tr("ui.share_menu.other", "Share a different screen") };
            let r = widgets::pop_item(ui, &PopItem::new("screen_share", &label).sub(&tr("ui.share_menu.other_sub", "Opens the system picker")));
            crate::ui::drive_dump::rect(|| "menu.share_start".into(), r.rect);
            if r.clicked() {
                picked = Some(vec![UiAction::StageShare(room)]);
            }
        }
        (ShareState::Picking { .. } | ShareState::Paused, _) => {}
        (_, Some(capture)) => {
            let change = tr("ui.bar.change_source", "Change source");
            if widgets::pop_item(ui, &PopItem::new("swap_horiz", &change)).clicked() {
                picked = Some(vec![UiAction::Discard(capture), UiAction::StageShare(room)]);
            }
            let all = tr("ui.bar.all_stream_settings", "All stream settings…");
            if widgets::pop_item(ui, &PopItem::new("tune", &all)).clicked() {
                picked = Some(vec![UiAction::OpenModal(Modal::StreamSettings { capture, room })]);
            }
            match (state, here) {
                (ShareState::Live { .. }, Some((d, _))) => {
                    if widgets::pop_item(ui, &PopItem::new("stop_screen_share", &tr("ui.bar.stop_sharing", "Stop sharing")).danger()).clicked() {
                        picked = Some(vec![UiAction::StopDestination(d)]);
                    }
                }
                _ => {
                    let go = tr_with("ui.stream.go_live_in", "Go live in %{room}", &[("room", &room_name)]);
                    if go_live_item(ui, &go) {
                        picked = Some(vec![UiAction::GoLive { capture, room }]);
                    }
                }
            }
        }
        _ => {}
    }
    let close = picked.is_some();
    out.extend(picked.unwrap_or_default());
    close
}

/// "Add X here too" / "Move X here" for every capture that is live in other rooms but not in `room`:
/// `(capture, add label, move label, move sub)`, plus whether anything is live elsewhere at all.
fn elsewhere_items(cx: &Cx, room: RoomHandle) -> (Vec<(CaptureSourceId, String, String, String)>, bool) {
    let captures = cx.live_captures();
    let items = captures
        .iter()
        .filter(|(_, c)| c.destinations.values().all(|d| d.room != room))
        .map(|(id, c)| {
            let screen = screen_name(c);
            let rooms: Vec<String> = c.destinations.values().map(|d| cx.room_name(d.room)).collect();
            (
                *id,
                tr_with("ui.share_menu.add", "Add %{screen} here too", &[("screen", &screen)]),
                tr_with("ui.share_menu.move", "Move %{screen} here", &[("screen", &screen)]),
                tr_with("ui.share_menu.move_sub", "Starts here, then stops in %{rooms}", &[("rooms", &rooms.join(", "))]),
            )
        })
        .collect();
    (items, !captures.is_empty())
}

/// Whether our share is live in another room (the Share button then opens its menu instead of the picker,
/// so "add here" and "move here" stay reachable).
pub(super) fn live_elsewhere(cx: &Cx, room: RoomHandle) -> bool {
    !elsewhere_items(cx, room).0.is_empty()
}

/// The menu's main action: an accent-filled row (`.mi.primary`).
fn go_live_item(ui: &mut Ui, label: &str) -> bool {
    let row = HBox::new("menu_primary", 34.0).pad(theme::S2, theme::S2).slots(&[fixed(18.0), FILL]).sense(Sense::click()).show(ui);
    let fill = if row.response.hovered() { theme::ACCENT_LIGHT } else { theme::ACCENT };
    ui.painter().rect_filled(row.rect, CornerRadius::same(7), fill);
    row.icon(ui, 0, "cast", 16.0, theme::PRIMARY_TEXT);
    row.text(ui, 1, label, font(Weight::Bold, theme::FS_LABEL), theme::PRIMARY_TEXT, Align::Min);
    row.response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

/// The dropdown of the bar's webcam button (replica `camMenu`): only the camera, picked at once (a new camera
/// resets the mode to its default, and a running webcam restarts on it), and "All webcam settings…" for the rest.
/// Returns true when it should close.
pub(super) fn webcam_menu(ui: &mut Ui, cx: &Cx, room: RoomHandle, out: &mut Vec<UiAction>) -> bool {
    pop_heading(ui, &tr("ui.webcam.camera_heading", "Camera"));
    let current = cx.state.webcam_choice().map(|(c, _)| c);
    let mut close = false;
    if cx.state.webcams.is_empty() {
        pop_note(ui, &tr("ui.bar.webcam_none", "No camera found"));
    }
    for (i, cam) in cx.state.webcams.iter().enumerate() {
        if widgets::pop_item(ui, &PopItem::new("videocam", &cam.name).sub(&cam.device).selected(current == Some(i))).clicked() {
            out.push(UiAction::SetWebcam(WebcamSettings { camera: cam.name.clone(), mode: String::new(), ..cx.state.webcam.clone() }));
            close = true;
        }
    }
    pop_sep(ui);
    if widgets::pop_item(ui, &PopItem::new("tune", &tr("ui.webcam.all_settings", "All webcam settings…"))).clicked() {
        out.push(UiAction::OpenModal(Modal::WebcamSettings { room }));
        close = true;
    }
    close
}

/// The label of our audio share in `room`, if there is one ("Desktop audio" or the app's name).
pub(super) fn audio_current(cx: &Cx, room: RoomHandle) -> Option<String> {
    cx.audio_share(room).map(|(_, a)| a.label.clone())
}

/// A source group's heading ("Games", "Browsers", ...).
pub(super) fn group_title(g: AudioGroup) -> String {
    match g {
        AudioGroup::Games => tr("ui.audio.group_games", "Games"),
        AudioGroup::Browsers => tr("ui.audio.group_browsers", "Browsers"),
        AudioGroup::Media => tr("ui.audio.group_media", "Media"),
        AudioGroup::VoiceChat => tr("ui.audio.group_voice", "Voice chat"),
        AudioGroup::Devices => tr("ui.audio.group_devices", "Devices"),
        AudioGroup::Other => tr("ui.audio.group_other", "Other"),
    }
}

/// The sub-line of a source row: what kind of thing it is.
pub(super) fn group_sub(g: AudioGroup) -> String {
    match g {
        AudioGroup::Games => tr("ui.audio.sub_games", "Game"),
        AudioGroup::Browsers => tr("ui.audio.sub_browsers", "Browser"),
        AudioGroup::Media => tr("ui.audio.sub_media", "Media player"),
        AudioGroup::VoiceChat => tr("ui.audio.sub_voice", "Voice chat"),
        AudioGroup::Devices => tr("ui.audio.sub_devices", "Device"),
        AudioGroup::Other => tr("ui.audio.sub_other", "App"),
    }
}

/// The icon of a source row.
pub(super) fn group_icon(g: AudioGroup) -> &'static str {
    match g {
        AudioGroup::Games => "sports_esports",
        AudioGroup::Browsers => "public",
        AudioGroup::Media => "music_note",
        AudioGroup::VoiceChat => "forum",
        AudioGroup::Devices => "smartphone",
        AudioGroup::Other => "apps",
    }
}

/// The sub-line of the "Desktop audio" row: it says how many sources the exclusion list leaves out.
pub(super) fn desktop_sub(excluded: usize) -> String {
    if excluded == 0 {
        tr("ui.audio.desktop_sub", "Everything you hear")
    } else {
        tr_with("ui.audio.desktop_sub_excl", "Everything you hear, minus %{count} excluded", &[("count", &excluded.to_string())])
    }
}

/// The rows of an audio source field (the stream settings' Audio row, the audio settings' Source row): No audio,
/// Desktop audio, then every listed app under its group heading. `choices[i]` is what row `i` picks
/// (`None` = no audio).
pub(super) struct SourceOptions {
    rows: Vec<(String, String, &'static str, Option<String>)>,
    pub choices: Vec<Option<AudioChoice>>,
    /// The row of the current choice.
    pub selected: usize,
}

impl SourceOptions {
    pub(super) fn new(cx: &Cx, room: RoomHandle) -> Self {
        let mut rows = vec![
            (tr("ui.audio.no_audio", "No audio"), tr("ui.audio.no_audio_sub", "Only the picture"), "volume_off", None),
            (tr("ui.audio.desktop", "Desktop audio"), desktop_sub(cx.state.audio_exclusions.get().len()), "desktop_windows", None),
        ];
        let mut choices = vec![None, Some(AudioChoice::Desktop)];
        for s in &cx.state.audio_sources {
            rows.push((s.name.clone(), group_sub(s.group), group_icon(s.group), Some(group_title(s.group))));
            choices.push(Some(AudioChoice::Named(s.name.clone())));
        }
        let current = cx.audio_choice(room);
        let selected = match &current {
            None => 0,
            Some(c) => match choices.iter().position(|x| x.as_ref() == Some(c)) {
                Some(i) => i,
                None => {
                    // The app is not listed right now (it paused): keep showing the choice.
                    rows.push((c.label().to_string(), group_sub(AudioGroup::Other), group_icon(AudioGroup::Other), None));
                    choices.push(current.clone());
                    choices.len() - 1
                }
            },
        };
        Self { rows, choices, selected }
    }

    pub(super) fn opts(&self) -> Vec<super::menu::Opt<'_>> {
        self.rows
            .iter()
            .map(|(label, sub, icon, group)| super::menu::Opt { label, sub: Some(sub), icon: Some(icon), group: group.as_deref() })
            .collect()
    }

    /// The action for picking row `idx` (when it differs from the shown one).
    pub(super) fn action(&self, room: RoomHandle, idx: usize) -> UiAction {
        match &self.choices[idx] {
            None => UiAction::StopSharingAudio { room },
            Some(choice) => UiAction::ShareAudio { room, choice: choice.clone() },
        }
    }
}

/// The audio dropdown of the bar's Share audio button (replica `audioMenu`): No audio, Desktop audio, then
/// every app producing audio under its group's heading with a live meter, the current choice lit; mute (while
/// sharing) and "All audio settings…" at the bottom. Picking a row starts, switches or stops the share in
/// `room` at once; audio needs no screen share. Returns true when it should close.
pub(super) fn audio_menu(ui: &mut Ui, cx: &Cx, room: RoomHandle, out: &mut Vec<UiAction>) -> bool {
    let state = cx.state;
    let current = cx.audio_choice(room);
    let mut close = false;
    pop_heading(ui, &tr("ui.audio.heading", "Audio source"));
    let none = tr("ui.audio.no_audio", "No audio");
    if widgets::pop_item(ui, &PopItem::new("volume_off", &none).sub(&tr("ui.audio.no_audio_sub", "Only the picture")).selected(current.is_none())).clicked() {
        out.push(UiAction::StopSharingAudio { room });
        close = true;
    }
    let desktop = tr("ui.audio.desktop", "Desktop audio");
    let sub = desktop_sub(state.audio_exclusions.get().len());
    if widgets::pop_item(ui, &PopItem::new("desktop_windows", &desktop).sub(&sub).selected(current == Some(AudioChoice::Desktop))).clicked() {
        out.push(UiAction::ShareAudio { room, choice: AudioChoice::Desktop });
        close = true;
    }
    if state.audio_sources.is_empty() {
        pop_note(ui, &state.audio_sources_error.clone().unwrap_or_else(|| tr("ui.audio.none", "No app is playing audio")));
        if widgets::pop_item(ui, &PopItem::new("refresh", &tr("ui.audio.refresh", "Refresh"))).clicked() {
            out.push(UiAction::RefreshAudioSources);
        }
    }
    egui::ScrollArea::vertical().max_height(360.0).auto_shrink([true, true]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let mut last = None;
        for s in &state.audio_sources {
            if last != Some(s.group) {
                pop_heading(ui, &group_title(s.group));
                last = Some(s.group);
            }
            let lit = cx.source_level(s.id).map(level_lit).unwrap_or(0);
            let sel = current == Some(AudioChoice::Named(s.name.clone()));
            let sub = group_sub(s.group);
            if widgets::pop_item(ui, &PopItem::new(group_icon(s.group), &s.name).sub(&sub).selected(sel).meter(lit)).clicked() {
                out.push(UiAction::ShareAudio { room, choice: AudioChoice::Named(s.name.clone()) });
                close = true;
            }
        }
    });
    pop_sep(ui);
    if let Some((id, _)) = cx.audio_share(room) {
        let mut muted = state.is_audio_share_muted(id);
        let icon = if muted { "volume_off" } else { "volume_up" };
        if super::menu::switch_row(ui, icon, &tr("ui.bar.mute_audio", "Mute shared audio"), &mut muted) {
            out.push(UiAction::SetSharedAudioMuted { room, muted });
        }
    }
    if widgets::pop_item(ui, &PopItem::new("tune", &tr("ui.audio.settings_item", "All audio settings…"))).clicked() {
        out.push(UiAction::OpenModal(Modal::AudioSettings { room }));
        close = true;
    }
    close
}

/// Start the level meters when the audio dropdown opens (and refresh the source list), restart them when
/// the list changes, stop them when it closes. `taps` is the stage's record of what is running.
pub(super) fn sync_level_taps(cx: &mut Cx, taps: &mut Option<Vec<u32>>, open: bool) {
    let ids: Vec<u32> = cx.state.audio_sources.iter().map(|s| s.id).collect();
    match (&*taps, open) {
        (None, true) => {
            cx.act(UiAction::RefreshAudioSources);
            cx.act(UiAction::StartLevelTaps(ids.clone()));
            *taps = Some(ids);
        }
        (Some(running), true) if *running != ids => {
            cx.act(UiAction::StartLevelTaps(ids.clone()));
            *taps = Some(ids);
        }
        (Some(_), false) => {
            cx.act(UiAction::StopLevelTaps);
            *taps = None;
        }
        _ => {}
    }
}

/// The other joined rooms (in the call: no sharing into one you have not picked up) a capture could go to, for the Also send to / Move
/// to lists (shared with the stream-settings modal's two popovers).
pub(super) struct RoomTargets {
    capture: CaptureSourceId,
    /// The destination in the room the list is opened from (Move to moves it).
    here: Option<DestinationId>,
    /// (room, name, sub-line people count, our destination there).
    rooms: Vec<(RoomHandle, String, String, Option<DestinationId>)>,
    none: String,
    live_there: String,
    move_sub: String,
    already: String,
}

/// A room a share may go to: connected and picked up. Why: a room you are not "in the call" of is look-only.
pub(super) fn is_target(conn: ConnState, in_call: bool) -> bool {
    conn == ConnState::Joined && in_call
}

impl RoomTargets {
    pub(super) fn of(cx: &Cx, capture: CaptureSourceId, room: RoomHandle) -> Self {
        let c = cx.capture(capture);
        let dest_in = |r: RoomHandle| c.and_then(|c| c.destinations.iter().find(|(_, d)| d.room == r).map(|(id, _)| *id));
        let rooms = cx
            .rooms()
            .into_iter()
            .filter_map(|e| e.handle.filter(|h| is_target(e.conn, cx.state.in_call(*h))).map(|h| (h, e.name)))
            .filter(|(h, _)| *h != room)
            .map(|(h, name)| {
                let n = cx.session(h).map(|s| s.members.len()).unwrap_or(0);
                let people = if n == 1 {
                    tr("ui.rooms.one_person", "1 person")
                } else {
                    tr_with("ui.rooms.people", "%{count} people", &[("count", &n.to_string())])
                };
                (h, name, people, dest_in(h))
            })
            .collect();
        Self {
            capture,
            here: dest_in(room),
            rooms,
            none: tr("ui.rooms.none", "No other connected rooms"),
            live_there: tr("ui.rooms.live_there", "Live there"),
            move_sub: tr("ui.rooms.move_sub", "Starts there first, then stops here"),
            already: tr("ui.rooms.already_live", "Already live there"),
        }
    }

    /// One checkbox row per room: checked = live there (click stops it),
    /// unchecked = click adds a destination there.
    pub(super) fn also_items(&self, ui: &mut Ui, picked: &mut Option<UiAction>) {
        if self.rooms.is_empty() {
            pop_note(ui, &self.none);
        }
        for (h, name, people, dest) in &self.rooms {
            let sub = if dest.is_some() { &self.live_there } else { people };
            if pop_check_item(ui, dest.is_some(), name, sub).clicked() {
                *picked = Some(match dest {
                    Some(d) => UiAction::StopDestination(*d),
                    None => UiAction::AddDestination { capture: self.capture, room: *h },
                });
            }
        }
    }

    /// One row per room: moves this room's destination there (disabled
    /// where the capture is already live).
    pub(super) fn move_items(&self, ui: &mut Ui, picked: &mut Option<UiAction>) {
        if self.rooms.is_empty() {
            pop_note(ui, &self.none);
        }
        for (h, name, _, dest) in &self.rooms {
            let sub = if dest.is_some() { &self.already } else { &self.move_sub };
            let item = PopItem::new("meeting_room", name).sub(sub).enabled(dest.is_none() && self.here.is_some());
            if widgets::pop_item(ui, &item).clicked()
                && let Some(d) = self.here
            {
                *picked = Some(UiAction::MoveDestination { destination: d, room: *h });
            }
        }
    }

    /// Push the picked action (closing the popover after a move) or close
    /// on a click outside.
    pub(super) fn finish(&self, cx: &mut Cx, picked: Option<UiAction>, open: bool) {
        match picked {
            Some(a) => {
                let close = matches!(a, UiAction::MoveDestination { .. });
                cx.act(a);
                if close {
                    cx.act(UiAction::ClosePopover);
                }
            }
            None if !open => cx.act(UiAction::ClosePopover),
            None => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (also used by the stream-settings modal)
// ---------------------------------------------------------------------------

/// "1 room" / "N rooms".
pub(super) fn rooms_count(n: usize) -> String {
    if n == 1 { tr("ui.bar.one_room", "1 room") } else { tr_with("ui.bar.n_rooms", "%{count} rooms", &[("count", &n.to_string())]) }
}

/// kbit/s as the mockup prints a rate: "6.0 Mbps", "800 kbps".
pub(super) fn fmt_rate(kbps: u32) -> String {
    if kbps >= 1000 { format!("{:.1} Mbps", kbps as f32 / 1000.0) } else { format!("{kbps} kbps") }
}

/// Meter bars lit for a 0..1 level (same curve as the old dropdown meters:
/// bar `i` lights once the level passes `i / 5`).
pub(super) fn level_lit(level: f32) -> u8 {
    (0..5).filter(|i| level.clamp(0.0, 1.0) > *i as f32 / 5.0).count() as u8
}

/// The capture's display name ("Screen share": the portal gives no monitor
/// name, so this is the lib's label), in the current language
/// ([`super::share_label`]).
pub(super) fn screen_name(c: &LocalCapture) -> String {
    super::share_label(&c.label)
}

/// Source resolution of a running video capture, if known yet.
pub(super) fn source_size(c: &LocalCapture) -> Option<(u32, u32)> {
    match &c.status {
        ShareStatus::Active(ShareHandle::Video(h)) => {
            let caps = h.source_caps();
            (caps.width > 0 && caps.height > 0).then_some((caps.width, caps.height))
        }
        _ => None,
    }
}

/// What a destination's bitrate is right now: the allocator's share in
/// Absolute mode (once it has run), else the destination's own rate.
pub(super) fn dest_kbps(cx: &Cx, id: DestinationId, d: &Destination) -> u32 {
    let own = d.live_params().bitrate_kbps;
    match cx.state.bandwidth_mode {
        BandwidthMode::Absolute => cx.state.allocated_kbps(id).unwrap_or(own),
        BandwidthMode::PerStream => own,
    }
}

/// Pill colour family (a [`Pill`] borrows its text, so screens build it at
/// the use site from this plus a `String`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PillKind {
    Live,
    Progress,
    Degraded,
    Muted,
}

impl PillKind {
    pub(super) fn pill(self, text: &str) -> Pill<'_> {
        match self {
            PillKind::Live => Pill::live(text),
            PillKind::Progress => Pill::progress(text),
            PillKind::Degraded => Pill::degraded(text),
            PillKind::Muted => Pill::muted(text),
        }
    }
}

/// A destination's status pill: Live / Starting / Moving / Applying /
/// Stalled / Failed.
pub(super) fn dest_pill(c: &LocalCapture, d: &Destination) -> (PillKind, String) {
    if matches!(d.pending, Some(PendingRole::TransferTarget(_) | PendingRole::TransferSource(_))) {
        return (PillKind::Progress, tr("ui.pill.moving", "Moving"));
    }
    match c.destination_state(d) {
        DestinationState::Live => (PillKind::Live, tr("ui.pill.live", "Live")),
        DestinationState::Starting => (PillKind::Progress, tr("ui.pill.starting", "Starting")),
        DestinationState::Rebuilding { .. } => (PillKind::Progress, tr("ui.pill.applying", "Applying")),
        DestinationState::Stalled { .. } => (PillKind::Degraded, tr("ui.pill.stalled", "Stalled")),
        DestinationState::Failed(_) => (PillKind::Degraded, tr("ui.pill.failed", "Failed")),
    }
}

/// The pill of a destination whose capture is being restarted (#99).
pub(super) fn recovering_pill() -> (PillKind, String) {
    (PillKind::Progress, tr("ui.pill.recovering", "Recovering"))
}

/// Tooltip of the recovering bar and pill: attempt number, countdown to the
/// next attempt, and the last error.
pub(super) fn recovering_tip(recovery: &simple_screensharing_client::session::CaptureRecovery) -> String {
    let mut lines = vec![tr("ui.bar.recovering_title", "Screen capture lost")];
    lines.push(tr_with(
        "ui.bar.recovering_attempt",
        "Attempt %{n} of %{total}",
        &[("n", &recovery.attempt_number().to_string()), ("total", &recovery.attempts_total().to_string())],
    ));
    if let Some(wait) = recovery.retry_in(std::time::Instant::now()).filter(|w| !w.is_zero()) {
        lines.push(tr_with("ui.bar.recovering_next", "Next attempt in %{secs} s", &[("secs", &wait.as_secs().max(1).to_string())]));
    }
    lines.push(tr_with("ui.bar.recovering_cause", "Cause: %{reason}", &[("reason", recovery.last_error())]));
    lines.join("\n")
}

/// `true` while `d` is still Starting because its capture has not produced a
/// first frame yet: an idle monitor (the portal only sends on repaint) until
/// something on it changes. See `AppState::capture_awaiting_first_frame`.
pub(super) fn awaiting_first_frame(cx: &Cx, capture: CaptureSourceId, c: &LocalCapture, d: &Destination) -> bool {
    matches!(c.destination_state(d), DestinationState::Starting) && cx.state.capture_awaiting_first_frame(capture)
}

/// The long form of the "waiting for the screen to change" hint (bar tooltip,
/// stream-settings banner).
pub(super) fn waiting_hint() -> String {
    tr(
        "ui.waiting_frame_hint",
        "An idle screen sends no picture until something on it changes. Move the mouse over the shared screen and the stream starts.",
    )
}

/// Every video destination this client sends, across captures.
pub(super) fn video_destination_count(cx: &Cx) -> usize {
    cx.state.captures.values().filter(|c| c.kind == StreamKind::Video).map(|c| c.destinations.len()).sum()
}


/// Popover section heading (`.ph`): 28 px, pad 8, accent uppercase label.
pub(super) fn pop_heading(ui: &mut Ui, title: &str) {
    let row = HBox::new("pop_heading", 28.0).pad(theme::S2, theme::S2).slots(&[FILL]).show(ui);
    let c = row.cell(0);
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_center_size(c.center(), vec2(c.width(), theme::HEADING_H))));
    widgets::heading(&mut child, title, None, false);
}

/// Popover separator (`hr`): 1 px `DIVIDER` with 4 px air above and below.
pub(super) fn pop_sep(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
    ui.painter().hline(r.x_range().shrink(4.0), r.center().y.round() + 0.5, Stroke::new(1.0_f32, theme::DIVIDER));
}

/// A non-interactive faint line in a popover ("No other connected rooms").
pub(super) fn pop_note(ui: &mut Ui, text: &str) {
    let row = HBox::new("pop_note", theme::ROW_H).pad(theme::S2, theme::S2).slots(&[FILL]).show(ui);
    row.text(ui, 0, text, font(Weight::Regular, theme::FS_SMALL), theme::TEXT_FAINT, Align::Min);
}

/// A [`widgets::pop_item`]-shaped row (44 px, `[18 mark][fill label/sub]`)
/// whose lead is a checkbox mark (`menuitemcheckbox`), for Also send to.
pub(super) fn pop_check_item(ui: &mut Ui, checked: bool, label: &str, sub: &str) -> egui::Response {
    const SLOTS: [Slot; 2] = [fixed(18.0), FILL];
    let r = HBox::new("pop_item", theme::POP_ITEM_TWO_H).pad(theme::S2, theme::S2).slots(&SLOTS).sense(Sense::click()).show(ui);
    if r.response.hovered() {
        ui.painter().rect_filled(r.rect, CornerRadius::same(7), theme::WASH_HOVER_2);
    }
    let bx = Rect::from_center_size(r.cell(0).center(), vec2(theme::MARK, theme::MARK));
    if checked {
        ui.painter().rect_filled(bx, CornerRadius::same(5), theme::ACCENT);
        for dx in [0.0, 0.5] {
            paint_icon(ui, bx.translate(vec2(dx, 0.0)), "check", 15.0, egui::Color32::WHITE);
        }
    } else {
        ui.painter().rect_stroke(bx, CornerRadius::same(5), Stroke::new(1.0_f32, theme::BTN_BORDER), StrokeKind::Inside);
    }
    let cell = r.cell(1);
    let top = Rect::from_x_y_ranges(cell.x_range(), cell.center().y - 17.0..=cell.center().y);
    let bot = Rect::from_x_y_ranges(cell.x_range(), cell.center().y..=cell.center().y + 15.0);
    paint_text(ui, top, label, font(Weight::Regular, theme::FS_LABEL), theme::TEXT, Align::Min);
    paint_text(ui, bot, sub, font(Weight::Regular, theme::FS_TINY), theme::TEXT_FAINT, Align::Min);
    r.response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_lit_maps_the_meter_fraction_onto_five_bars() {
        assert_eq!(level_lit(0.0), 0);
        assert_eq!(level_lit(0.25), 2);
        assert_eq!(level_lit(0.7), 4);
        assert_eq!(level_lit(1.0), 5);
    }

    #[test]
    fn only_rooms_in_the_call_are_share_targets() {
        assert!(is_target(ConnState::Joined, true));
        assert!(!is_target(ConnState::Joined, false), "connected but not picked up: no sharing into it");
        assert!(!is_target(ConnState::Reconnecting, true) && !is_target(ConnState::Disconnected, true));
    }
}
