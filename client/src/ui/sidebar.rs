//! Left sidebar (owner: I13): brand band, the scrolling body ("Watched
//! streams" well, ROOMS heading, rooms well with sharer sub-rows, "Create or
//! join a room"), and the user band (avatar, name + "Live in N rooms" /
//! "Not sharing" which opens [`PopoverKind::Outgoing`], settings gear).
//!
//! Regions (allocated by the shell, see `ui/mod.rs`):
//! - `show_brand`: the padded 56 px brand band (pad 16), left-to-right.
//! - `show_body`: a 247 px wide vbox, gap 8, inside the sidebar ScrollArea
//!   (padding 0 left/right 12, bottom 12).
//! - `show_user_band`: the padded 56 px user band (pad 12), left-to-right.
//!
//! **STUB**: plain rows / buttons so rooms can be joined and selected.

use egui::{Align, Ui};

use super::{ConnState, Cx, Modal, PopoverKind, UiAction, View};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, Btn, DotState, FILL, HBox, LiveSlot, RoomLead, RoomRow, Unit, fixed};

/// Sidebar UI state (I13: add fields freely).
#[derive(Default)]
pub struct SidebarState {}

/// Brand band content: `[mark][label]`.
pub fn show_brand(ui: &mut Ui, _cx: &mut Cx, _st: &mut SidebarState) {
    let row = HBox::new("brand", theme::BAND_H).slots(&[fixed(30.0), FILL]).show(ui);
    row.icon(ui, 0, "screen_share", 20.0, theme::ACCENT_LIGHT);
    row.text(ui, 1, &t!("app.title"), font(Weight::Bold, 15.0), theme::TEXT, Align::Min);
}

/// Sidebar body: watched-streams well, rooms, create/join.
pub fn show_body(ui: &mut Ui, cx: &mut Cx, _st: &mut SidebarState) {
    let watched = cx.watched().len() as u32;
    let sel_watching = cx.nav.view == View::Watching;
    let clicked = widgets::well(ui, |ui| {
        widgets::room_row(
            ui,
            &RoomRow {
                lead: RoomLead::Icon("grid_view"),
                name: &t!("ui.sidebar.watched_streams"),
                live: LiveSlot::Absent,
                num: Some(watched),
                unit: Unit::Icon("visibility"),
                selected: sel_watching,
            },
        )
        .clicked()
    });
    if clicked {
        cx.act(UiAction::ShowWatching);
    }

    let rooms = cx.rooms();
    widgets::heading(ui, &t!("ui.sidebar.rooms"), Some(&rooms.len().to_string()), true);
    if !rooms.is_empty() {
        let live = cx.live_rooms();
        widgets::well(ui, |ui| {
            for r in &rooms {
                let dot = match r.conn {
                    ConnState::Joined => DotState::Joined,
                    ConnState::Reconnecting => DotState::Reconnecting,
                    ConnState::Disconnected => DotState::Disconnected,
                };
                let shares = r.handle.map(|h| cx.shares_in(h).len() as u32);
                let mine = r.handle.is_some_and(|h| live.contains(&h));
                let resp = widgets::room_row(
                    ui,
                    &RoomRow {
                        lead: RoomLead::Dot(dot),
                        name: &r.name,
                        live: if mine { LiveSlot::On } else { LiveSlot::Off },
                        num: shares.filter(|n| *n > 0),
                        unit: if shares.unwrap_or(0) > 0 { Unit::Icon("screen_share") } else { Unit::None },
                        selected: cx.nav.view == View::Room(r.key.clone()),
                    },
                );
                if resp.clicked() {
                    cx.act(UiAction::SelectRoom(r.key.clone()));
                }
            }
        });
    }
    let w = ui.available_width();
    if Btn::ghost(&t!("ui.sidebar.create_or_join")).icon("add").show_sized(ui, w).clicked() {
        cx.act(UiAction::OpenModal(Modal::CreateJoin));
    }
}

/// User band: `[32 avatar][fill who][32 gear]`.
pub fn show_user_band(ui: &mut Ui, cx: &mut Cx, _st: &mut SidebarState) {
    let row = HBox::new("user_band", theme::BAND_H).slots(&[fixed(theme::AVATAR), FILL, fixed(theme::CTL_H)]).show(ui);
    let name = cx.state.display_name.clone();
    let initial: String = name.chars().next().map(|c| c.to_string()).unwrap_or_default();
    let n_live = cx.live_rooms().len();
    row.show(ui, 0, Align::Center, |ui| widgets::avatar(ui, &initial, theme::AVATAR, 0, n_live > 0));
    let who = row.cell(1);
    let status = if n_live > 0 { t!("ui.sidebar.live_in_rooms", count = n_live).to_string() } else { t!("ui.sidebar.not_sharing").to_string() };
    let top = egui::Rect::from_x_y_ranges(who.x_range(), who.center().y - 16.0..=who.center().y);
    let bottom = egui::Rect::from_x_y_ranges(who.x_range(), who.center().y..=who.center().y + 14.0);
    widgets::paint_text(ui, top, &name, font(Weight::Bold, theme::FS_ROW), theme::TEXT, Align::Min);
    widgets::paint_text(ui, bottom, &status, font(Weight::Regular, theme::FS_TINY), if n_live > 0 { theme::SUCCESS } else { theme::TEXT_DIM }, Align::Min);
    let who_resp = ui.interact(who, ui.id().with("who"), egui::Sense::click());
    if who_resp.clicked() {
        cx.toggle_popover(PopoverKind::Outgoing, who);
    }
    if row.show(ui, 2, Align::Center, |ui| widgets::icon_btn(ui, "settings", widgets::IconBtnKind::Plain, &t!("ui.sidebar.settings"))).clicked() {
        cx.act(UiAction::OpenModal(Modal::Settings));
    }
    if let Some(pop) = cx.popover_of(&PopoverKind::Outgoing) {
        let ctx = ui.ctx().clone();
        let open = super::popover(&ctx, pop, 340.0, |ui| {
            super::placeholder(ui, "outgoing popover (I13)", theme::ROW_H);
        });
        if !open {
            cx.act(UiAction::ClosePopover);
        }
    }
}
