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
//! The rooms well is built in two steps: [`room_lines`] turns the session
//! into a flat list of [`Line`]s (one per room row / sharer row, each with
//! the actions its click pushes), then [`paint_lines`] draws them with the
//! `widgets::room_row` / `widgets::sharer_row` helpers. Every count and fps
//! sits right-aligned in the same 28 px slot, followed by the same 22 px unit
//! slot, so the numbers form one column across room rows, sharer rows and
//! the "Watched streams" row (the slot geometry is pinned by a test below).
//! `Why:` misaligned counts were the user's #1 complaint about the first
//! mockup; one row helper per row type makes drift impossible.
//!
//! Numbers shown, and where they come from (never invented):
//! - room row: other people's shares (`Cx::shares_in`) + 1 if we're live there;
//!   empty when the room isn't joined or nobody shares.
//! - "You": the live destination's frame rate (runtime params, else requested).
//! - other sharers: **decoded** fps while we watch the stream (the protocol's
//!   `StreamInfo` carries no fps — contract "Known lib gaps" #5); empty otherwise.

use egui::{Align, CornerRadius, Rect, Sense, Stroke, Ui, vec2};
use simple_screensharing_client::protocol::StreamKind;

use super::{ConnState, Cx, Modal, PopoverKind, RoomKey, ShareState, StreamKey, UiAction, View, tr, tr_with};
use crate::theme::{self, Weight, font};
use crate::widgets::{
    self, Btn, DotState, FILL, HBox, IconBtnKind, IconTone, LiveSlot, PopItem, RoomLead, RoomRow, SharerRow, Unit, fixed,
};

/// Width of the Outgoing popover (mockup `.popup`).
const POPOVER_W: f32 = 340.0;

/// Sidebar UI state (I13: add fields freely).
#[derive(Default)]
pub struct SidebarState {}

// ---------------------------------------------------------------------------
// Brand band
// ---------------------------------------------------------------------------

/// Brand band content: `[30 mark][fill label]`.
pub fn show_brand(ui: &mut Ui, _cx: &mut Cx, _st: &mut SidebarState) {
    let row = HBox::new("brand", theme::BAND_H).slots(&[fixed(30.0), FILL]).show(ui);
    widgets::paint_brand_mark(ui, row.cell(0), theme::SIDEBAR_RAIL);
    row.text(ui, 1, &tr("app.title", "Simple Screensharing"), font(Weight::Bold, theme::FS_H3), theme::TEXT, Align::Min);
}

// ---------------------------------------------------------------------------
// Body
// ---------------------------------------------------------------------------

/// One line of the rooms well.
#[derive(Debug, Clone)]
enum Line {
    /// `widgets::room_row`: dot, bold name, live cast, share count.
    Room { name: String, dot: DotState, live: bool, num: Option<u32>, selected: bool, tip: String, click: Vec<UiAction> },
    /// `widgets::sharer_row` under its room: icon, name, fps.
    Sharer { icon: &'static str, tone: IconTone, name: String, fps: Option<u32>, tip: Option<String>, click: Vec<UiAction> },
}

/// Sidebar body: watched-streams well, ROOMS heading, rooms well, create/join.
pub fn show_body(ui: &mut Ui, cx: &mut Cx, _st: &mut SidebarState) {
    // "Watched streams": the cross-room grid (View::Watching).
    let watched = cx.watched().len() as u32;
    let sel_watching = cx.nav.view == View::Watching;
    let clicked = widgets::well(ui, |ui| {
        widgets::room_row(
            ui,
            &RoomRow {
                lead: RoomLead::Icon("grid_view"),
                name: &tr("ui.sidebar.watched_streams", "Watched streams"),
                live: LiveSlot::Absent,
                num: Some(watched),
                unit: Unit::Icon("visibility"),
                selected: sel_watching,
            },
        )
        .on_hover_text(tr("ui.sidebar.watched_streams_tip", "Every stream you're watching, from every room"))
        .clicked()
    });
    if clicked {
        cx.act(UiAction::ShowWatching);
    }

    // Mockup: the heading has `margin-top: var(--s1)` on top of the vbox gap.
    ui.add_space(theme::S1);
    let lines = room_lines(cx);
    let n_rooms = lines.iter().filter(|l| matches!(l, Line::Room { .. })).count();
    widgets::heading(ui, &tr("ui.sidebar.rooms", "Rooms"), Some(&n_rooms.to_string()), true);
    if !lines.is_empty() {
        let clicked = widgets::well(ui, |ui| paint_lines(ui, &lines));
        for a in clicked.into_iter().flatten() {
            cx.act(a);
        }
    }

    if widgets::btn_row(ui, &[Btn::ghost(&tr("ui.sidebar.create_or_join", "Create or join a room")).icon("add")]).is_some() {
        cx.act(UiAction::OpenModal(Modal::CreateJoin));
    }
}

/// Draw the rooms well's lines; returns the clicked line's actions.
fn paint_lines(ui: &mut Ui, lines: &[Line]) -> Option<Vec<UiAction>> {
    let mut clicked = None;
    for line in lines {
        let (resp, click) = match line {
            Line::Room { name, dot, live, num, selected, tip, click } => {
                let row = RoomRow {
                    lead: RoomLead::Dot(*dot),
                    name,
                    live: if *live { LiveSlot::On } else { LiveSlot::Off },
                    num: *num,
                    unit: if num.is_some() { Unit::Icon("screen_share") } else { Unit::None },
                    selected: *selected,
                };
                (widgets::room_row(ui, &row).on_hover_text(tip.as_str()), click)
            }
            Line::Sharer { icon, tone, name, fps, tip, click } => {
                let row = SharerRow { icon, tone: *tone, name, num: *fps, unit: if fps.is_some() { "fps" } else { "" } };
                let mut resp = widgets::sharer_row(ui, &row);
                if let Some(t) = tip {
                    resp = resp.on_hover_text(t.as_str());
                }
                (resp, click)
            }
        };
        if resp.clicked() {
            clicked = Some(click.clone());
        }
    }
    clicked
}

/// The rooms well as data: every room (saved and connected, list order),
/// each joined room followed by its sharers ("You" first when live there).
fn room_lines(cx: &Cx) -> Vec<Line> {
    let mut out = Vec::new();
    for r in cx.rooms() {
        let (dot, tip) = match r.conn {
            ConnState::Joined => (DotState::Joined, tr("ui.sidebar.status_connected", "Connected")),
            ConnState::Connecting => (DotState::Reconnecting, tr("ui.sidebar.status_connecting", "Connecting")),
            ConnState::Reconnecting => (DotState::Reconnecting, tr("ui.sidebar.status_reconnecting", "Reconnecting")),
            ConnState::Disconnected => (DotState::Disconnected, tr("ui.sidebar.status_disconnected", "Not connected")),
        };
        let handle = r.handle.filter(|_| r.conn == ConnState::Joined);
        // Our live destination here: (capture, destination fps).
        let mine = match handle.map(|_| cx.share_state(&r)) {
            Some(ShareState::Live { capture, destination }) => cx
                .capture(capture)
                .and_then(|c| c.destinations.get(&destination))
                .map(|d| (capture, d.runtime.as_ref().map_or(d.params, |rt| rt.lock().unwrap().params()).framerate)),
            _ => None,
        };
        let shares = handle.map(|h| cx.shares_in(h)).unwrap_or_default();
        let n = shares.len() + usize::from(mine.is_some());
        out.push(Line::Room {
            name: r.name.clone(),
            dot,
            // The cast icon only once it's on air, not while Starting.
            live: mine.is_some() && handle.and_then(|h| cx.my_phase(h)) == Some(super::MyPhase::OnAir),
            num: (handle.is_some() && n > 0).then_some(n as u32),
            selected: super::view_is_room(&cx.nav.view, &r.key),
            tip: tip.to_string(),
            click: vec![UiAction::SelectRoom(r.key.clone())],
        });
        let Some(h) = handle else { continue };
        if let Some((capture, fps)) = mine {
            out.push(Line::Sharer {
                icon: "cast",
                // Green only once it's on air; dim while still Starting.
                tone: if cx.my_phase(h) == Some(super::MyPhase::OnAir) { IconTone::Live } else { IconTone::Dim },
                name: tr("ui.sidebar.you", "You").to_string(),
                fps: Some(fps),
                tip: Some(tr("ui.sidebar.you_tip", "Your stream here — open its settings").to_string()),
                click: vec![UiAction::SelectRoom(r.key.clone()), UiAction::OpenModal(Modal::StreamSettings { capture, room: h })],
            });
        }
        for s in shares {
            let key: StreamKey = (h, s.owner, s.stream_id);
            let watching = cx.is_watching(key);
            let audio = s.kind == StreamKind::Audio;
            let icon = match (audio, watching) {
                (true, _) => "volume_up",
                (false, true) => "visibility",
                (false, false) => "screen_share",
            };
            let fps = if watching && !audio { decoded_fps(cx, key) } else { None };
            out.push(Line::Sharer {
                icon,
                tone: if watching { IconTone::Watch } else { IconTone::Dim },
                name: cx.member_name(h, s.owner),
                fps,
                tip: (!s.label.is_empty()).then(|| super::share_label(&s.label)),
                click: vec![UiAction::SelectRoom(r.key.clone())],
            });
        }
    }
    out
}

/// Decoded frames per second of a watched video stream, rounded.
fn decoded_fps(cx: &Cx, key: StreamKey) -> Option<u32> {
    let entry = cx.videos.get(key)?;
    let fps = entry.view.lock().unwrap().source().stats().decoded_fps;
    Some(fps.round().max(0.0) as u32)
}

// ---------------------------------------------------------------------------
// User band + Outgoing popover
// ---------------------------------------------------------------------------

/// User band: `[32 avatar][fill who(name, status)][32 gear]`. Clicking "who"
/// toggles the Outgoing popover.
pub fn show_user_band(ui: &mut Ui, cx: &mut Cx, _st: &mut SidebarState) {
    let row = HBox::new("user_band", theme::BAND_H).slots(&[fixed(theme::AVATAR), FILL, fixed(theme::CTL_H)]).show(ui);
    let name = cx.state.display_name.clone();
    let initial: String = name.chars().next().map(|c| c.to_string()).unwrap_or_default();
    // Only rooms we're actually on air in; a share still Starting (e.g. an
    // idle screen waiting for its first frame) says "Starting…" instead.
    let n_live = cx.on_air_rooms().len();
    let starting = n_live == 0 && cx.live_rooms().iter().any(|h| cx.my_phase(*h) == Some(super::MyPhase::Starting));
    let online = cx.rooms().iter().any(|r| r.conn == ConnState::Joined);
    let av = Rect::from_center_size(row.cell(0).center(), vec2(theme::AVATAR, theme::AVATAR));
    widgets::paint_avatar(ui, av, &initial, theme::avatar_colors(&name), online);

    // "who": vbox(name 13 bold / 16 line, status 11 / 14 line); the hover
    // wash is padded 3/6 around it (mockup `.p-user .who`, margin 0 −6).
    let who = row.cell(1);
    let hot = Rect::from_center_size(who.center(), vec2(who.width() + 12.0, 36.0));
    let resp = ui.interact(hot, ui.id().with("sidebar_who"), Sense::click());
    if resp.hovered() || cx.popover_of(&PopoverKind::Outgoing).is_some() {
        ui.painter().rect_filled(hot, CornerRadius::same(theme::ROUNDING_XS), theme::WASH_HOVER_1);
    }
    let status = match n_live {
        0 if starting => tr("ui.sidebar.starting", "Starting…").to_string(),
        0 => tr("ui.sidebar.not_sharing", "Not sharing").to_string(),
        1 => tr("ui.sidebar.live_in_one_room", "Live in 1 room").to_string(),
        n => tr_with("ui.sidebar.live_in_rooms", "Live in %{count} rooms", &[("count", &n.to_string())]),
    };
    let top = Rect::from_x_y_ranges(who.x_range(), who.center().y - 15.0..=who.center().y + 1.0);
    let bottom = Rect::from_x_y_ranges(who.x_range(), who.center().y + 1.0..=who.center().y + 15.0);
    widgets::paint_text(ui, top, &name, font(Weight::Bold, theme::FS_ROW), theme::TEXT, Align::Min);
    let status_col = if n_live > 0 { theme::SUCCESS } else if starting { theme::ACCENT_LIGHTER } else { theme::TEXT_DIM };
    widgets::paint_text(ui, bottom, &status, font(Weight::Regular, theme::FS_TINY), status_col, Align::Min);
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tr("ui.sidebar.outgoing_tip", "Your outgoing streams"));
    if resp.clicked() {
        cx.toggle_popover(PopoverKind::Outgoing, hot);
    }

    if row.show(ui, 2, Align::Center, |ui| widgets::icon_btn(ui, "settings", IconBtnKind::Plain, &tr("ui.sidebar.settings", "Settings"))).clicked() {
        cx.act(UiAction::OpenModal(Modal::Settings));
    }

    if let Some(pop) = cx.popover_of(&PopoverKind::Outgoing) {
        let ctx = ui.ctx().clone();
        let mut clicked: Vec<UiAction> = Vec::new();
        let open = super::popover(&ctx, pop, POPOVER_W, |ui| clicked = outgoing_body(ui, cx));
        for a in clicked {
            cx.act(a);
        }
        if !open {
            cx.act(UiAction::ClosePopover);
        }
    }
}

/// Outgoing popover body (mockup `outgoing`): "You're sharing" heading, one
/// item per live destination (click: go to the room and open its stream
/// settings), then "Stop all sharing". Returns the clicked item's actions.
fn outgoing_body(ui: &mut Ui, cx: &Cx) -> Vec<UiAction> {
    let mut out = Vec::new();
    pop_heading(ui, &tr("ui.sidebar.outgoing.heading", "You're sharing"));
    let mut any = false;
    for (capture, c) in cx.live_captures() {
        for d in c.destinations.values() {
            any = true;
            let p = d.runtime.as_ref().map_or(d.params, |rt| rt.lock().unwrap().params());
            let room = cx.room_name(d.room);
            let sub = tr_with("ui.sidebar.outgoing.sub", "%{screen} · %{fps} fps · %{kbps} kbps", &[("screen", &super::share_label(&c.label)), ("fps", &p.framerate.to_string()), ("kbps", &p.bitrate_kbps.to_string())]);
            if widgets::pop_item(ui, &PopItem::new("cast", &room).sub(&sub)).clicked() {
                let key: Option<RoomKey> = cx.session(d.room).map(|s| s.join_string.clone());
                out.extend(key.map(UiAction::SelectRoom));
                out.push(UiAction::OpenModal(Modal::StreamSettings { capture, room: d.room }));
            }
        }
    }
    if !any {
        let r = HBox::new("pop_none", theme::ROW_H).pad(theme::S2, theme::S2).slots(&[FILL]).show(ui);
        r.text(ui, 0, &tr("ui.sidebar.outgoing.none", "Nothing right now"), font(Weight::Regular, theme::FS_LABEL), theme::TEXT_DIM, Align::Min);
        return out;
    }
    pop_rule(ui);
    if widgets::pop_item(ui, &PopItem::new("stop_screen_share", &tr("ui.sidebar.outgoing.stop_all", "Stop all sharing")).danger()).clicked() {
        out.push(UiAction::ClosePopover);
        out.push(UiAction::StopAllSharing);
    }
    out
}

/// Popup section heading (mockup `.popup .ph`): 24 px, pad 8, accent, bold
/// 10 px uppercase, tracking 1.1.
fn pop_heading(ui: &mut Ui, title: &str) {
    let r = HBox::new("pop_heading", 24.0).pad(theme::S2, theme::S2).slots(&[FILL]).show(ui);
    let cell = r.cell(0);
    let g = widgets::text_galley(ui, &title.to_uppercase(), font(Weight::Bold, theme::FS_SECTION), theme::ACCENT, cell.width(), 1.1);
    widgets::paint_galley(ui, cell, g, Align::Min);
}

/// Popup separator (mockup `.popup hr`): 1 px `DIVIDER`, 4 px margin above
/// and below, inset 2 px.
fn pop_rule(ui: &mut Ui) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), theme::S1 * 2.0 + 1.0), Sense::hover());
    ui.painter().hline((r.left() + 2.0)..=(r.right() - 2.0), r.center().y, Stroke::new(1.0_f32, theme::DIVIDER));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sharer(name: &str, fps: Option<u32>) -> Line {
        Line::Sharer { icon: "visibility", tone: IconTone::Watch, name: name.into(), fps, tip: None, click: Vec::new() }
    }

    fn room(name: &str, num: Option<u32>, live: bool) -> Line {
        Line::Room { name: name.into(), dot: DotState::Joined, live, num, selected: false, tip: String::new(), click: Vec::new() }
    }

    /// Room rows are 32 px, sharer rows 24 px, and every number slot (28)
    /// and unit slot (22) has the same x-range in every row type — the
    /// "counts and fps form one column" requirement.
    #[test]
    fn numbers_and_units_share_one_column_and_rows_have_exact_heights() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        widgets::set_debug_slots(&ctx, true);
        let lines = [
            room("Movie Night", Some(3), true),
            sharer("You", Some(60)),
            sharer("Lea with an overly long display name that must truncate", Some(144)),
            room("A room with a very very long name indeed", None, false),
            room("Gaming", Some(12), false),
            sharer("Ben", None),
        ];
        let mut slots = Vec::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.set_width(247.0);
                ui.spacing_mut().item_spacing.y = 0.0;
                // The "Watched streams" row sits in its own well, like in `show_body`.
                widgets::well(ui, |ui| {
                    widgets::room_row(
                        ui,
                        &RoomRow {
                            lead: RoomLead::Icon("grid_view"),
                            name: "Watched streams",
                            live: LiveSlot::Absent,
                            num: Some(2),
                            unit: Unit::Icon("visibility"),
                            selected: false,
                        },
                    )
                });
                widgets::well(ui, |ui| paint_lines(ui, &lines));
            });
            slots = widgets::slot_log(ctx);
        });
        let rows: Vec<_> = slots.iter().filter(|s| s.row == "room_row" || s.row == "sharer_row").collect();
        assert!(!rows.is_empty());
        // Last two slots of each row are [28 num][22 unit].
        let mut nums = Vec::new();
        let mut units = Vec::new();
        let mut row_rects: Vec<(&str, Rect)> = Vec::new();
        for s in &rows {
            if !row_rects.iter().any(|(_, r)| *r == s.row_rect) {
                row_rects.push((s.row, s.row_rect));
            }
        }
        for (name, rr) in &row_rects {
            let mine: Vec<_> = rows.iter().filter(|s| s.row_rect == *rr).collect();
            let n = mine.len();
            nums.push(mine[n - 2].rect.x_range());
            units.push(mine[n - 1].rect.x_range());
            let want = if *name == "room_row" { theme::ROW_H } else { theme::SUBROW_H };
            assert_eq!(rr.height(), want, "{name} height");
        }
        assert_eq!(row_rects.len(), 7, "watched row + 6 lines");
        assert!(nums.windows(2).all(|w| w[0] == w[1]), "number slots differ between rows: {nums:?}");
        assert!(units.windows(2).all(|w| w[0] == w[1]), "unit slots differ between rows: {units:?}");
        assert_eq!(nums[0].span(), 28.0);
        assert_eq!(units[0].span(), 22.0);
    }
}
