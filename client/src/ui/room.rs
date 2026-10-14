//! The central area (owner: I14): room header band, room body, People panel,
//! for every [`View`]: a room (joined / reconnecting / disconnected),
//! "Watched streams", and the welcome state.
//!
//! Regions (allocated by the shell, see `ui/mod.rs`):
//! - `show_header`: the padded 56 px header band (pad 16), left-to-right.
//!   Spec: `[fill hbox·title: h2, status pill, meta][room controls: Auto-connect,
//!   Copy invite link, Leave, ⋯]`, all controls 32 px. Owns
//!   [`PopoverKind::RoomMenu`].
//! - `show_body`: a vbox inside the central ScrollArea (padding 16), gap 16
//!   between sections: reconnecting banner / "On screen" tiles / "Sharing in
//!   this room" cards / empty states. Tiles draw video with `cx.videos`.
//! - `show_people`: a 207 px vbox inside the People ScrollArea (padding
//!   16 top/bottom, 12 left/right), gap 12. Only called while the selected
//!   room is joined or reconnecting.
//!
//! **STUB**: plain rows / buttons so connecting and watching work.

use egui::{Align, Ui, vec2};
use simple_screensharing_client::decode::render::Fit;
use simple_screensharing_client::protocol::StreamKind;

use super::{ConnState, Cx, Modal, PopoverKind, RoomEntry, StreamKey, UiAction, View};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, BannerKind, Btn, FILL, HBox, PersonRow, fixed};

/// Central-area UI state (I14: add fields freely).
#[derive(Default)]
pub struct RoomState {}

/// Header band.
pub fn show_header(ui: &mut Ui, cx: &mut Cx, _st: &mut RoomState) {
    let room = cx.selected_room();
    let title = match (&cx.nav.view, &room) {
        (View::Watching, _) => t!("ui.sidebar.watched_streams").to_string(),
        (View::Room(_), Some(r)) => r.name.clone(),
        _ => t!("app.title").to_string(),
    };
    let Some(r) = room else {
        let row = HBox::new("hbox·title", theme::CTL_H).slots(&[FILL]).show(ui);
        row.text(ui, 0, &title, font(Weight::Bold, theme::FS_H2), theme::TEXT, Align::Min);
        return;
    };
    let copy = t!("ui.room.copy_invite");
    let leave = t!("ui.room.leave");
    let joined = r.handle;
    let btns = [
        Btn::secondary(&copy).icon("link").enabled(joined.is_some()),
        Btn::danger(&leave).icon("logout").enabled(joined.is_some()),
        Btn::secondary("").icon("more_horiz"),
    ];
    let (row, clicked) = super::fill_then_buttons(ui, "hbox·room-controls", &btns);
    let status = match r.conn {
        ConnState::Joined => "connected",
        ConnState::Reconnecting => "reconnecting",
        ConnState::Disconnected => "not connected",
    };
    row.text(ui, 0, &format!("{title}  ·  {status}"), font(Weight::Bold, theme::FS_H2), theme::TEXT, Align::Min);
    match clicked {
        Some(0) => cx.act(UiAction::CopyInviteLink(joined.unwrap())),
        Some(1) => cx.act(UiAction::Leave(joined.unwrap())),
        Some(2) => cx.toggle_popover(PopoverKind::RoomMenu { room: r.key.clone() }, row.cell(3)),
        _ => {}
    }
    if let Some(pop) = cx.popover_of(&PopoverKind::RoomMenu { room: r.key.clone() }) {
        let ctx = ui.ctx().clone();
        let mut picked = None;
        let open = super::popover(&ctx, pop, 240.0, |ui| {
            if widgets::pop_item(ui, &widgets::PopItem::new("edit", &t!("ui.room.rename"))).clicked() {
                picked = Some(UiAction::OpenModal(Modal::Rename { room: r.key.clone() }));
            }
            if widgets::pop_item(ui, &widgets::PopItem::new("delete", &t!("ui.room.forget")).danger()).clicked() {
                picked = Some(UiAction::OpenModal(Modal::ForgetConfirm { room: r.key.clone() }));
            }
        });
        if let Some(a) = picked {
            cx.act(a);
        } else if !open {
            cx.act(UiAction::ClosePopover);
        }
    }
}

/// Body for the current view.
pub fn show_body(ui: &mut Ui, cx: &mut Cx, _st: &mut RoomState) {
    match cx.nav.view.clone() {
        View::Welcome => {
            super::placeholder(ui, "welcome (I14)", 120.0);
            if Btn::primary(&t!("ui.sidebar.create_or_join")).icon("add").show(ui).clicked() {
                cx.act(UiAction::OpenModal(Modal::CreateJoin));
            }
        }
        View::Watching => {
            let keys = cx.watched();
            tiles(ui, cx, &keys);
            if keys.is_empty() {
                super::placeholder(ui, "nothing watched (I14)", 120.0);
            }
        }
        View::Room(_) => {
            let Some(r) = cx.selected_room() else {
                super::placeholder(ui, "room gone", 120.0);
                return;
            };
            room_body(ui, cx, &r);
        }
    }
}

fn room_body(ui: &mut Ui, cx: &mut Cx, r: &RoomEntry) {
    match (r.conn, r.handle) {
        (ConnState::Disconnected, _) => {
            super::placeholder(ui, "not connected (I14)", 120.0);
            let connect = t!("ui.room.connect");
            let forget = t!("ui.room.forget");
            match widgets::btn_row(ui, &[Btn::primary(&connect).icon("login"), Btn::danger(&forget).icon("delete")]) {
                Some(0) => cx.act(UiAction::Connect(r.key.clone())),
                Some(1) => cx.act(UiAction::OpenModal(Modal::ForgetConfirm { room: r.key.clone() })),
                _ => {}
            }
        }
        (ConnState::Reconnecting, _) | (_, None) => {
            if widgets::banner(ui, BannerKind::Warn, "sync_problem", &t!("ui.room.reconnecting"), Some(&t!("ui.room.cancel"))) {
                cx.act(UiAction::CancelConnect);
            }
        }
        (ConnState::Joined, Some(h)) => {
            let watched: Vec<StreamKey> = cx.watched().into_iter().filter(|k| k.0 == h).collect();
            if !watched.is_empty() {
                widgets::heading(ui, &t!("ui.room.on_screen"), Some(&watched.len().to_string()), false);
                tiles(ui, cx, &watched);
            }
            let shares = cx.shares_in(h);
            widgets::heading(ui, &t!("ui.room.sharing"), Some(&shares.len().to_string()), false);
            if shares.is_empty() {
                super::placeholder(ui, "nobody is sharing yet (I14)", 80.0);
            }
            for info in shares {
                let key = (h, info.owner, info.stream_id);
                let name = cx.member_name(h, info.owner);
                let label = format!("{name} · {} · {}", info.label, super::remote_stream_geometry(info));
                let watching = cx.is_watching(key);
                let (watch, pop) = if info.kind == StreamKind::Audio {
                    (if watching { t!("ui.room.stop_listening") } else { t!("ui.room.listen") }, None)
                } else {
                    (if watching { t!("ui.room.stop_watching") } else { t!("ui.room.watch") }, Some(t!("ui.room.pop_out")))
                };
                let mut btns = vec![Btn::secondary(&watch)];
                if let Some(p) = &pop {
                    btns.push(Btn::secondary(p).enabled(!cx.is_popped(key)));
                }
                let (row, clicked) = super::fill_then_buttons(ui, "share_row", &btns);
                row.text(ui, 0, &label, font(Weight::Regular, theme::FS_ROW), theme::TEXT, Align::Min);
                match (clicked, info.kind) {
                    (Some(0), StreamKind::Audio) => cx.act(UiAction::ToggleListen(key)),
                    (Some(0), _) if watching => cx.act(UiAction::Unwatch(key)),
                    (Some(0), _) => cx.act(UiAction::Watch(key)),
                    (Some(1), _) if watching => cx.act(UiAction::PopOut(key)),
                    (Some(1), _) => cx.act(UiAction::WatchPoppedOut(key)),
                    _ => {}
                }
            }
        }
    }
}

/// 16:9 tiles, one or two per row, video via `cx.videos`.
fn tiles(ui: &mut Ui, cx: &mut Cx, keys: &[StreamKey]) {
    let cols = if keys.len() == 1 { 1 } else { 2 };
    let mut acts = Vec::new();
    widgets::grid_rows_with(ui, cols, keys.len(), |ui, i| {
        let key = keys[i];
        let w = ui.available_width();
        let size = vec2(w, w * 9.0 / 16.0);
        if cx.is_popped(key) {
            let r = super::placeholder(ui, "popped out (I14)", size.y);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(egui::Rect::from_center_size(r.center() + vec2(0.0, 30.0), vec2(160.0, theme::CTL_H))));
            if Btn::secondary(&t!("ui.room.bring_back")).show_sized(&mut child, 160.0).clicked() {
                acts.push(UiAction::BringBack(key));
            }
            return;
        }
        if let Some(resp) = cx.videos.show(key, ui, size, Fit::Contain) {
            let name = cx.member_name(key.0, key.1);
            let fps = cx
                .session(key.0)
                .and_then(|s| s.watches.lock().unwrap().get(&(key.1, key.2)).map(|e| e.decoder.lock().unwrap().decoded_fps()))
                .unwrap_or(0.0);
            let bar = egui::Rect::from_min_size(resp.rect.min + vec2(8.0, 8.0), vec2(resp.rect.width() - 16.0, 29.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar));
            let row = HBox::new("tile_top", 29.0).slots(&[FILL, fixed(80.0), fixed(80.0)]).show(&mut child);
            row.text(&child, 0, &format!("{name}  {fps:.0} fps"), font(Weight::Bold, theme::FS_ROW), theme::TEXT, Align::Min);
            if row.show(&mut child, 1, Align::Min, |ui| Btn::secondary(&t!("ui.room.pop_out")).show_sized(ui, 80.0)).clicked() {
                acts.push(UiAction::PopOut(key));
            }
            if row.show(&mut child, 2, Align::Min, |ui| Btn::secondary(&t!("ui.room.stop_watching")).show_sized(ui, 80.0)).clicked() {
                acts.push(UiAction::Unwatch(key));
            }
        }
    });
    for a in acts {
        cx.act(a);
    }
}

/// People panel.
pub fn show_people(ui: &mut Ui, cx: &mut Cx, _st: &mut RoomState) {
    let Some(r) = cx.selected_room() else { return };
    let Some(h) = r.handle else {
        widgets::heading(ui, &t!("ui.room.people_last_known"), None, false);
        return;
    };
    let Some(s) = cx.session(h) else { return };
    let me = s.client.conn_id();
    widgets::heading(ui, &t!("ui.room.people"), Some(&s.members.len().to_string()), false);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        for (i, m) in s.members.iter().enumerate() {
            let watching = s.watches.lock().unwrap().keys().any(|k| k.0 == m.conn_id);
            widgets::person_row(
                ui,
                &PersonRow {
                    name: &m.display_name,
                    color: i,
                    you: m.conn_id == me,
                    live: m.stream_count > 0,
                    watching,
                    idle: m.stream_count == 0,
                    stale: false,
                },
            );
        }
    });
}
