//! The stage's menus, drawn from `Nav::popover` while one of ours is open: the control bar's dropdowns
//! (`PopoverKind::Bar`: Share, Audio, Layout, Room settings) and a stream's menu (`PopoverKind::Tile`: the `⋯`
//! button or a right click on a tile). All are `ui::popover`s, the app's own menu style.
//!
//! Called **before** the bar is drawn: a click outside an open menu that lands on another bar button closes it
//! (`ClosePopover`) before that button's `OpenPopover` is queued, so the new menu is not closed again.

use egui::{Align, Margin, Rect, Ui, vec2};

use super::bar::{BarMenu, layout_name};
use super::tile;
use super::{Item, Kind, StageLayout, StageState, StageView};
use crate::theme::{self, Weight, font};
use crate::ui::share::{self, MENU_W};
use crate::ui::{ConnState, Cx, Modal, Popover, PopoverKind, UiAction, drive_dump, menu, popover, share_label, tr, tr_with};
use crate::widgets::{self, PopItem};

/// Draw the open menu of this stage, if it is one of ours; also runs the audio list's level meters.
pub(super) fn show(ctx: &egui::Context, cx: &mut Cx, st: &mut StageState, view: &StageView, items: &[Item]) {
    let pop = cx.nav.popover.clone();
    let audio_open = matches!(&pop, Some(Popover { kind: PopoverKind::Bar(BarMenu::Audio), .. }));
    share::sync_level_taps(cx, &mut st.taps, audio_open);
    let Some(pop) = pop else { return };
    match pop.kind.clone() {
        PopoverKind::Bar(m) => bar_menu(ctx, cx, st, view, &pop, m),
        PopoverKind::Tile(id) => match items.iter().find(|i| i.id == id) {
            Some(it) => tile_menu(ctx, cx, st, view, &pop, it),
            None => cx.act(UiAction::ClosePopover),
        },
        _ => {}
    }
}

fn bar_menu(ctx: &egui::Context, cx: &mut Cx, st: &mut StageState, view: &StageView, pop: &Popover, menu: BarMenu) {
    let mut out: Vec<UiAction> = Vec::new();
    let mut close = false;
    let mut reset = false;
    let cxr: &Cx = cx;
    let target = share::share_target(cxr, view);
    let room = match view {
        StageView::Room(h) => cxr.rooms().into_iter().find(|r| r.handle == Some(*h)),
        StageView::Watched => None,
    };
    let open = popover(ctx, pop, MENU_W, |ui| match menu {
        BarMenu::Share => {
            if let Some((room, state)) = target {
                close = share::share_menu(ui, cxr, room, state, &mut out);
            }
        }
        BarMenu::Audio => {
            if let Some((room, _)) = target {
                close = share::audio_menu(ui, cxr, room, &mut out);
            }
        }
        BarMenu::Webcam => {
            if let Some((room, _)) = target {
                close = share::webcam_menu(ui, cxr, room, &mut out);
            }
        }
        BarMenu::Layout => reset = layout_menu(ui, cxr, view, &mut out, &mut close),
        BarMenu::Room => {
            if let Some(r) = &room {
                room_menu(ui, cxr, r, &mut out, &mut close);
            }
        }
    });
    if reset {
        st.reset_view(view);
    }
    for a in out {
        cx.act(a);
    }
    if close || !open {
        cx.act(UiAction::ClosePopover);
    }
}

/// The Room settings menu of `room` when it is the open popover, for a room that has no stage (its header's
/// `⋯`): the same body the bar's Room dropdown shows.
pub fn room_popover(ctx: &egui::Context, cx: &mut Cx, room: &crate::ui::RoomEntry) {
    let Some(pop) = cx.popover_of(&PopoverKind::Bar(BarMenu::Room)).cloned() else { return };
    let (mut out, mut close) = (Vec::new(), false);
    let cxr: &Cx = cx;
    let open = popover(ctx, &pop, MENU_W, |ui| room_menu(ui, cxr, room, &mut out, &mut close));
    for a in out {
        cx.act(a);
    }
    if close || !open {
        cx.act(UiAction::ClosePopover);
    }
}

/// The Layout menu: the three layouts, the stage options, reset. Returns true for "Reset sizes and positions".
fn layout_menu(ui: &mut Ui, cx: &Cx, view: &StageView, out: &mut Vec<UiAction>, close: &mut bool) -> bool {
    let cur = cx.shell.stage.layout(view);
    share::pop_heading(ui, &tr("ui.stage.layout_heading", "Layout"));
    for (layout, icon, sub) in [
        (StageLayout::Split, "grid_view", tr("ui.stage.layout_split_sub", "Even grid, every picture whole")),
        (StageLayout::Crop, "crop", tr("ui.stage.layout_crop_sub", "Fills each cell, edges cropped around the centre")),
        (StageLayout::Floating, "filter_none", tr("ui.stage.layout_floating_sub", "Move and resize each stream yourself")),
    ] {
        let name = layout_name(layout);
        if widgets::pop_item(ui, &PopItem::new(icon, &name).sub(&sub).selected(cur == layout)).clicked() {
            out.push(UiAction::SetStageLayout { view: view.clone(), layout });
            *close = true;
        }
    }
    share::pop_sep(ui);
    let pad = Margin::symmetric(8, 0);
    egui::Frame::NONE.inner_margin(pad).show(ui, |ui| {
        let mut pip = cx.shell.stage.pip(view);
        let r = widgets::checkbox(ui, &mut pip, &tr("ui.stage.pip", "Webcam in picture"));
        drive_dump::rect(|| "menu.pip".into(), r.rect);
        if r.on_hover_text(tr("ui.stage.pip_tip_menu", "A webcam sits inside its owner's screen share, if they have one running")).changed() {
            out.push(UiAction::SetStagePip { view: view.clone(), on: pip });
        }
        let mut names = cx.shell.stage.always_names;
        let r = widgets::checkbox(ui, &mut names, &tr("ui.stage.always_names", "Always show names"));
        drive_dump::rect(|| "menu.always_names".into(), r.rect);
        if r.on_hover_text(tr("ui.stage.always_names_tip", "Off: name and audio status only show while you hover a stream"))
            .changed()
        {
            out.push(UiAction::SetAlwaysShowNames(names));
        }
        let mut stats = cx.shell.stage.show_stats;
        let r = widgets::checkbox(ui, &mut stats, &tr("ui.stage.show_stats", "Show stats"));
        drive_dump::rect(|| "menu.show_stats".into(), r.rect);
        if r.on_hover_text(tr("ui.stage.show_stats_tip", "Resolution, frame rate and bitrate on every stream's name tag while you hover it")).changed() {
            out.push(UiAction::SetShowStats(stats));
        }
        let mut hide = cx.shell.stage.hide_own;
        let r = widgets::checkbox(ui, &mut hide, &tr("ui.stage.hide_own", "Hide my own streams"));
        drive_dump::rect(|| "menu.hide_own".into(), r.rect);
        if r.on_hover_text(tr("ui.stage.hide_own_tip", "Your screen and webcam leave the stage here; others still see them"))
            .changed()
        {
            out.push(UiAction::SetHideOwnStreams(hide));
        }
    });
    share::pop_sep(ui);
    let reset = widgets::pop_item(ui, &PopItem::new("restart_alt", &tr("ui.stage.reset_layout", "Reset sizes and positions"))).clicked();
    *close |= reset;
    reset
}

/// The Room settings menu: a header, Auto-connect, Rename, Copy invite link, Leave, Forget.
fn room_menu(ui: &mut Ui, cx: &Cx, r: &crate::ui::RoomEntry, out: &mut Vec<UiAction>, close: &mut bool) {
    let joined = r.handle.filter(|_| r.conn == ConnState::Joined);
    let people = joined.and_then(|h| cx.session(h)).map_or(0, |s| s.members.len());
    let status = if joined.is_some() {
        tr_with("ui.bar.room_status", "Connected · %{count} people", &[("count", &people.to_string())])
    } else {
        tr("ui.room.not_connected", "Not connected")
    };
    let row = widgets::HBox::new("room_menu_head", 40.0).pad(theme::S2, theme::S2).slots(&[widgets::fixed(14.0), widgets::FILL]).show(ui);
    let dot = match r.conn {
        ConnState::Joined => widgets::DotState::Joined,
        ConnState::Connecting | ConnState::Reconnecting => widgets::DotState::Reconnecting,
        ConnState::Disconnected => widgets::DotState::Disconnected,
    };
    widgets::paint_status_dot(ui, row.cell(0).center(), dot);
    let c = row.cell(1);
    let top = c.center().y - 15.0;
    widgets::paint_text(ui, Rect::from_x_y_ranges(c.x_range(), top..=top + 16.0), &r.name, font(Weight::Bold, theme::FS_ROW), theme::TEXT, Align::Min);
    widgets::paint_text(ui, Rect::from_x_y_ranges(c.x_range(), top + 16.0..=top + 30.0), &status, font(Weight::Regular, theme::FS_TINY), theme::TEXT_DIM, Align::Min);
    share::pop_sep(ui);
    let mut auto = r.auto_connect;
    if menu::switch_row(ui, "bolt", &tr("ui.room.auto_connect", "Auto-connect"), &mut auto) {
        out.push(UiAction::SetAutoConnect { room: r.key.clone(), on: auto });
    }
    if widgets::pop_item(ui, &PopItem::new("edit", &tr("ui.room.rename", "Rename"))).clicked() {
        out.push(UiAction::OpenModal(Modal::Rename { room: r.key.clone() }));
    }
    if widgets::pop_item(ui, &PopItem::new("link", &tr("ui.room.copy_invite", "Copy invite link")).enabled(joined.is_some())).clicked()
        && let Some(h) = joined
    {
        out.push(UiAction::CopyInviteLink(h));
        *close = true;
    }
    let leave = tr("ui.bar.leave_room_item", "Leave room");
    if widgets::pop_item(ui, &PopItem::new("logout", &leave).enabled(r.conn != ConnState::Disconnected)).clicked() {
        match (r.conn, r.handle) {
            (ConnState::Joined, Some(h)) => out.push(UiAction::Leave(h)),
            (ConnState::Connecting | ConnState::Reconnecting, _) => out.push(UiAction::CancelReconnect(r.key.clone())),
            _ => {}
        }
        *close = true;
    }
    share::pop_sep(ui);
    let forget = tr("ui.room.forget", "Forget room");
    if widgets::pop_item(ui, &PopItem::new("delete", &forget).sub(&tr("ui.room.forget_sub", "Leave and remove it from the list")).danger()).clicked() {
        out.push(UiAction::OpenModal(Modal::ForgetConfirm { room: r.key.clone() }));
    }
}

/// A stream's menu (`⋯` on its tile, or a right click): who and what, volume and level loudness, then
/// pop out / fullscreen / stop watching (your own share: Stream settings and Stop sharing). Volume acts during
/// the drag and the wheel steps it (5 % up to 200 %, 10 % above).
fn tile_menu(ctx: &egui::Context, cx: &mut Cx, st: &mut StageState, view: &StageView, pop: &Popover, it: &Item) {
    let mut out: Vec<UiAction> = Vec::new();
    let mut close = false;
    let cxr: &Cx = cx;
    let StageState { vol_drag, .. } = st;
    let combined = matches!(view, StageView::Watched);
    let open = popover(ctx, pop, 280.0, |ui| {
        tile_menu_body(ui, cxr, it, combined, vol_drag, &mut out, &mut close);
    });
    for a in out {
        cx.act(a);
    }
    if close || !open {
        cx.act(UiAction::ClosePopover);
    }
}

#[allow(clippy::too_many_arguments)]
fn tile_menu_body(
    ui: &mut Ui,
    cx: &Cx,
    it: &Item,
    combined: bool,
    vol_drag: &mut super::VolDrag,
    out: &mut Vec<UiAction>,
    close: &mut bool,
) {
    let (who, sub) = header_text(cx, it, combined);
    let row = widgets::HBox::new("tile_menu_head", 40.0).pad(theme::S2, theme::S2).slots(&[widgets::fixed(28.0), widgets::FILL]).show(ui);
    crate::ui::paint_person(ui, Rect::from_center_size(row.cell(0).center(), vec2(28.0, 28.0)), &(if it.mine { cx.state.display_name.clone() } else { it.id.who.clone() }), &(if it.mine { cx.state.emoji.clone() } else { cx.emoji_of(it.id.room, it.conn) }), false);
    let c = row.cell(1);
    let top = c.center().y - 15.0;
    widgets::paint_text(ui, Rect::from_x_y_ranges(c.x_range(), top..=top + 16.0), &who, font(Weight::Bold, theme::FS_ROW), theme::TEXT, Align::Min);
    widgets::paint_text(ui, Rect::from_x_y_ranges(c.x_range(), top + 16.0..=top + 30.0), &sub, font(Weight::Regular, theme::FS_TINY), theme::TEXT_DIM, Align::Min);
    share::pop_sep(ui);
    let mut volume = |ui: &mut Ui, key: crate::ui::StreamKey, out: &mut Vec<UiAction>| {
        share::pop_heading(ui, &tr("ui.stage.volume", "Volume"));
        tile::menu_volume_row(ui, cx, vol_drag, key, out);
    };
    match &it.kind {
        Kind::Video(key) => {
            if cx.stream_has_audio(*key) {
                volume(ui, *key, out);
            } else {
                share::pop_note(ui, &tr("ui.stage.no_audio_stream", "No audio in this stream"));
            }
            share::pop_sep(ui);
            if widgets::pop_item(ui, &PopItem::new("open_in_new", &tr("ui.room.pop_out", "Pop out"))).clicked() {
                out.push(UiAction::PopOut(*key));
                *close = true;
            }
            if widgets::pop_item(ui, &PopItem::new("fullscreen", &tr("ui.stage.fullscreen", "Fullscreen"))).clicked() {
                out.push(UiAction::EnterStreamFullscreen(*key));
                *close = true;
            }
            if widgets::pop_item(ui, &PopItem::new("visibility_off", &tr("ui.room.stop_watching", "Stop watching")).danger()).clicked() {
                out.push(UiAction::Unwatch(*key));
                *close = true;
            }
        }
        Kind::Idle(key, _) => {
            if widgets::pop_item(ui, &PopItem::new("play_arrow", &tr("ui.stage.watch_stream", "Watch stream"))).clicked() {
                out.push(UiAction::Watch(*key));
                *close = true;
            }
            if widgets::pop_item(ui, &PopItem::new("open_in_new", &tr("ui.room.pop_out", "Pop out")).sub(&tr("ui.room.pop_out_card_tip", "Watch in its own window"))).clicked() {
                out.push(UiAction::WatchPoppedOut(*key));
                *close = true;
            }
        }
        Kind::Audio(key, _) => {
            let listening = cx.is_watching(*key);
            if listening {
                volume(ui, *key, out);
                share::pop_sep(ui);
            }
            let (icon, label) = if listening {
                ("volume_off", tr("ui.room.stop_listening", "Stop listening"))
            } else {
                ("volume_up", tr("ui.room.listen", "Listen"))
            };
            if widgets::pop_item(ui, &PopItem::new(icon, &label)).clicked() {
                out.push(UiAction::ToggleListen(*key));
                *close = true;
            }
        }
        Kind::Mine(capture, destination) if it.is_cam() => {
            if widgets::pop_item(ui, &PopItem::new("tune", &tr("ui.webcam.settings_item", "Webcam settings…"))).clicked() {
                out.push(UiAction::OpenModal(Modal::WebcamSettings { room: it.id.room }));
                *close = true;
            }
            let _ = (capture, destination);
            if widgets::pop_item(ui, &PopItem::new("videocam_off", &tr("ui.webcam.stop_item", "Stop webcam")).danger()).clicked() {
                out.push(UiAction::StopWebcam);
                *close = true;
            }
        }
        Kind::Mine(capture, destination) => {
            if widgets::pop_item(ui, &PopItem::new("tune", &tr("ui.bar.all_stream_settings", "Stream settings…"))).clicked() {
                out.push(UiAction::OpenModal(Modal::StreamSettings { capture: *capture, room: it.id.room }));
                *close = true;
            }
            if widgets::pop_item(ui, &PopItem::new("stop_screen_share", &tr("ui.bar.stop_sharing", "Stop sharing")).danger()).clicked() {
                out.push(UiAction::StopDestination(*destination));
                *close = true;
            }
        }
    }
}

/// `(who, label · size)` of the menu's header.
fn header_text(cx: &Cx, it: &Item, combined: bool) -> (String, String) {
    let place = |label: String, size: String| {
        let tail = if combined { cx.room_name(it.id.room) } else { size };
        if tail.is_empty() { label } else { format!("{label} · {tail}") }
    };
    match &it.kind {
        Kind::Mine(capture, _) => {
            let who = if it.is_cam() { tr("ui.stage.your_webcam", "Your webcam") } else { tr("ui.stage.your_stream", "Your stream") };
            (who, place(cx.capture(*capture).map(|c| share_label(&c.label)).unwrap_or_default(), String::new()))
        }
        Kind::Video(key) | Kind::Idle(key, _) | Kind::Audio(key, _) => {
            let info = cx.session(key.0).and_then(|s| s.streams.iter().find(|i| i.owner == key.1 && i.stream_id == key.2));
            let size = cx.videos.frame_size(*key).map(|(w, h)| format!("{w}×{h}")).unwrap_or_default();
            (cx.member_name(key.0, key.1), place(info.map(|i| share_label(&i.label)).unwrap_or_default(), size))
        }
    }
}
