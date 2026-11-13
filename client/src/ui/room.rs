//! The central area for every [`View`]: a room (joined / reconnecting / disconnected), "Watched streams", and
//! the welcome state. The People panel and the room's controls left with task I3 (2026-11-04): people are the
//! stage's strip, the room's actions its control bar (`ui/stage/`).
//!
//! Regions (allocated by the shell, see `ui/mod.rs`):
//! - [`show_header`]: the padded 56 px header band (pad 16), `[h2 name][status pill][meta]`. A room with no
//!   stage also gets `[⋯]` for the Room settings menu. Owns [`PopoverKind::Bar`]`(Room)` for those rooms.
//! - [`show_stage`]: the whole central region of a joined room and of "Watched streams": the black stage with
//!   its strip and bar (`ui/stage/`), no scroll area, no padding.
//! - [`show_body`]: the other states (welcome, disconnected, connecting, reconnecting: banner, empty states) as a
//!   vbox inside the central ScrollArea (padding 16, gap 16).
//!
//! # Strings
//! Every string goes through [`tr`]: `t!(key)`, falling back to the English
//! text until the locale files (I17) have the key.

use egui::{Align, Color32, CornerRadius, Layout, Rect, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::session::ReconnectState;

use super::{ConnState, Cx, Modal, PopoverKind, RoomEntry, ShareState, UiAction, View, tr, tr_with};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, Btn, FILL, HBox, IconBtnKind, Pill, fixed, text_width};

/// Central-area UI state.
#[derive(Default)]
pub struct RoomState {
    /// The stage's layout geometry, drags and slider state.
    pub stage: super::stage::StageState,
}

// ---------------------------------------------------------------------------
// Sizes (mockup CSS)
// ---------------------------------------------------------------------------

/// Empty-state action row width (`.empty hbox·actions`).
const EMPTY_ACTIONS_W: f32 = 320.0;
/// Reconnect banner: below this text-column width the buttons move under the
/// text (see [`reconnect_banner`]).
const BANNER_TEXT_MIN: f32 = 320.0;

// ---------------------------------------------------------------------------
// Pure layout math (tested)
// ---------------------------------------------------------------------------


/// Header band: `[h2 name][status pill][meta]`. A room with no stage (not connected, connecting) also gets a
/// `⋯` that opens the Room settings menu, which the bar would otherwise hold (Auto-connect, Rename, Forget).
pub fn show_header(ui: &mut Ui, cx: &mut Cx, _st: &mut RoomState) {
    let room = cx.selected_room();
    let Some(r) = room else {
        let (title, meta) = match cx.nav.view {
            View::Watching => (tr("ui.room.watched_title", "Watched streams"), tr("ui.room.watched_meta", "Every stream you're watching, from every room")),
            _ => (tr("app.title", "Simple Screensharing"), String::new()),
        };
        let h2 = font(Weight::Bold, theme::FS_H2);
        let tw = text_width(ui, &title, h2.clone(), 0.0).ceil();
        let row = HBox::new("hbox·title", theme::CTL_H).slots(&[fixed(tw), FILL]).show(ui);
        row.text(ui, 0, &title, h2, theme::TEXT, Align::Min);
        row.text(ui, 1, &meta, font(Weight::Regular, theme::FS_SMALL), theme::TEXT_DIM, Align::Min);
        return;
    };

    let joined = r.handle.filter(|_| r.conn == ConnState::Joined);
    let pill_l = match r.conn {
        // Connected but not picked up yet: "Connected"; in the call: "In the call" (`AppState::in_call`).
        ConnState::Joined if joined.is_some_and(|h| cx.state.in_call(h)) => tr("ui.room.in_call", "In the call"),
        ConnState::Joined => tr("ui.room.connected", "Connected"),
        ConnState::Connecting => tr("ui.room.connecting_pill", "Connecting"),
        ConnState::Reconnecting => tr("ui.room.reconnecting_pill", "Reconnecting"),
        ConnState::Disconnected => tr("ui.room.not_connected", "Not connected"),
    };
    let pill = match r.conn {
        ConnState::Joined => Pill::live(&pill_l).lead(widgets::PillLead::Dot),
        ConnState::Connecting => Pill::progress(&pill_l),
        ConnState::Reconnecting => Pill::degraded(&pill_l),
        ConnState::Disconnected => Pill::muted(&pill_l),
    };
    let meta = r.handle.and_then(|h| cx.session(h).map(|s| (h, s.members.len()))).map(|(h, n)| {
        let sharing = cx.shares_in(h).len() + usize::from(matches!(cx.share_state(&r), ShareState::Live { .. }));
        // Singular and plural per half: German conjugates the verb
        // ("1 teilt", "2 teilen"), so one key per count shape, not one string.
        let people = if n == 1 {
            tr("ui.room.meta_people_one", "1 person")
        } else {
            tr_with("ui.room.meta_people", "%{count} people", &[("count", &n.to_string())])
        };
        let sharing = if sharing == 1 {
            tr("ui.room.meta_sharing_one", "1 sharing")
        } else {
            tr_with("ui.room.meta_sharing", "%{count} sharing", &[("count", &sharing.to_string())])
        };
        format!("{people} · {sharing}")
    });
    let h2 = font(Weight::Bold, theme::FS_H2);
    let meta_f = font(Weight::Regular, theme::FS_SMALL);
    let name_w = text_width(ui, &r.name, h2.clone(), 0.0).ceil();
    let pw = pill.width(ui);
    let menu_btn = joined.is_none();
    let slots: Vec<widgets::Slot> = if menu_btn { vec![FILL, fixed(theme::CTL_H)] } else { vec![FILL] };
    let row = HBox::new("hbox·room-title", theme::CTL_H).slots(&slots).show(ui);
    row.show(ui, 0, Align::Min, |ui| {
        // Name, then the pill, then the meta line while it fits (a long name truncates before it).
        let avail = ui.available_width();
        let meta_w = meta.as_ref().map_or(0.0, |m| text_width(ui, m, meta_f.clone(), 0.0).ceil());
        let meta_room = if meta.is_some() && name_w + pw + 2.0 * theme::S3 + meta_w <= avail { theme::S3 + meta_w } else { 0.0 };
        let tw = name_w.min((avail - pw - theme::S3 - meta_room).max(0.0));
        let t = HBox::new("hbox·title", theme::CTL_H).gap(theme::S3).slots(&[fixed(tw), fixed(pw), FILL]).show(ui);
        t.text(ui, 0, &r.name, h2, theme::TEXT, Align::Min);
        pill.paint(ui, Rect::from_center_size(t.cell(1).center(), vec2(pw, theme::PILL_H)));
        if let (Some(m), true) = (meta.as_ref(), meta_room > 0.0) {
            t.text(ui, 2, m, meta_f, theme::TEXT_DIM, Align::Min);
        }
    });
    if menu_btn {
        let tip = tr("ui.room.options", "Room options");
        if row.show(ui, 1, Align::Min, |ui| widgets::icon_btn(ui, "more_horiz", IconBtnKind::Bordered, &tip)).clicked() {
            cx.toggle_popover(PopoverKind::Bar(super::stage::BarMenu::Room), row.cell(1));
        }
        super::stage::room_menu_popover(ui.ctx(), cx, &r);
    }
}

// ---------------------------------------------------------------------------
// Body
// ---------------------------------------------------------------------------

/// Whether the central area is the stage (a joined room, "Watched streams") instead of the scrolling body.
pub fn shows_stage(cx: &Cx) -> bool {
    match &cx.nav.view {
        View::Watching => true,
        View::Room(_) => cx.selected_room().is_some_and(|r| r.conn == ConnState::Joined && r.handle.is_some()),
        View::Welcome => false,
    }
}

/// The stage, filling the whole central region.
pub fn show_stage(ui: &mut Ui, cx: &mut Cx, st: &mut RoomState) {
    super::stage::show_main(ui, cx, &mut st.stage);
}

/// Body for the other states: welcome, disconnected, connecting, reconnecting.
pub fn show_body(ui: &mut Ui, cx: &mut Cx) {
    match &cx.nav.view {
        View::Welcome => {
            let title = tr("ui.room.welcome_title", "Pick a room on the left");
            let text = tr("ui.room.welcome_text", "Or create one and send its invite link to your friends.");
            let create = tr("ui.sidebar.create_or_join", "Create or join a room");
            if empty_state(ui, "meeting_room", &title, Some(&text), &[Btn::primary(&create).icon("add").large()]) == Some(0) {
                cx.act(UiAction::OpenModal(Modal::CreateJoin));
            }
        }
        // Watched streams and joined rooms are the stage (`show_stage`).
        View::Watching => {}
        View::Room(_) => {
            let Some(r) = cx.selected_room() else { return };
            room_body(ui, cx, &r);
        }
    }
}


fn room_body(ui: &mut Ui, cx: &mut Cx, r: &RoomEntry) {
    match (r.conn, r.handle) {
        (ConnState::Disconnected, _) => {
            // The last Connect failed: say why, above the usual empty state.
            if let Some(e) = cx.shell.connect_error(&r.key) {
                let text = tr_with("ui.room.connect_failed", "Couldn't connect: %{error}", &[("error", e)]);
                widgets::banner(ui, widgets::BannerKind::Warn, "error", &text, None);
            }
            let title = tr_with("ui.room.offline_title", "You're not connected to %{room}", &[("room", &r.name)]);
            let text = tr("ui.room.offline_text", "It stays in your list so you can come back with one click.");
            let connect = tr("ui.room.connect", "Connect");
            let forget = tr("ui.room.forget", "Forget room");
            let btns = [Btn::primary(&connect).icon("login").large(), Btn::danger(&forget).icon("delete").large()];
            match empty_state(ui, "cloud_off", &title, Some(&text), &btns) {
                Some(0) => cx.act(UiAction::Connect(r.key.clone())),
                Some(1) => cx.act(UiAction::OpenModal(Modal::ForgetConfirm { room: r.key.clone() })),
                _ => {}
            }
        }
        (ConnState::Connecting, _) => {
            let title = tr_with("ui.room.connecting_title", "Connecting to %{room}…", &[("room", &r.name)]);
            let cancel = tr("ui.room.cancel", "Cancel");
            if empty_state(ui, "sync", &title, None, &[Btn::secondary(&cancel).large()]) == Some(0) {
                cx.act(UiAction::CancelReconnect(r.key.clone()));
            }
        }
        (ConnState::Reconnecting, _) | (_, None) => {
            match reconnect_banner(ui, cx.state.reconnect_info(&r.key)) {
                Some(0) => cx.act(UiAction::RetryNow(r.key.clone())),
                Some(1) => cx.act(UiAction::CancelReconnect(r.key.clone())),
                _ => {}
            }
            section(ui, &tr("ui.room.sharing", "Sharing in this room"), 0, |ui| {
                let title = tr("ui.room.paused_title", "No shares right now");
                let text = tr("ui.room.paused_text", "Shares come back when the room reconnects.");
                empty_state(ui, "tv_off", &title, Some(&text), &[]);
            });
        }
        // A joined room is the stage; `shows_stage` keeps it from getting here.
        (ConnState::Joined, Some(_)) => {}
    }
}


/// `vbox·section` (gap 12): `[heading LABEL count][body]`.
fn section(ui: &mut Ui, title: &str, count: usize, body: impl FnOnce(&mut Ui)) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = theme::S3;
        widgets::heading(ui, title, Some(&count.to_string()), false);
        body(ui);
    });
}

/// The second and third banner lines: "Attempt 3 · Retrying in 4 s" (or
/// "Trying now…", or "Room full, still trying"), then a short last error.
fn reconnect_details(info: &ReconnectState, now: std::time::Instant) -> (String, Option<String>) {
    let mut parts = Vec::new();
    if info.attempts > 0 {
        parts.push(tr_with("ui.room.reconnect_attempt", "Attempt %{count}", &[("count", &info.attempts.to_string())]));
    }
    if info.room_full() {
        parts.push(tr("ui.room.reconnect_room_full", "Room full, still trying").to_string());
    }
    match info.retry_in(now) {
        None => parts.push(tr("ui.room.reconnect_trying", "Trying now…").to_string()),
        Some(d) => {
            let secs = d.as_secs_f32().ceil() as u64;
            parts.push(tr_with("ui.room.reconnect_retry_in", "Retrying in %{seconds} s", &[("seconds", &secs.to_string())]));
        }
    }
    // Room full already says it all; otherwise one short line of the error.
    let error = info.last_error.as_deref().filter(|_| !info.room_full()).and_then(|e| e.lines().map(str::trim).find(|l| !l.is_empty())).map(|e| {
        let short: String = if e.chars().count() > 90 { e.chars().take(89).chain(['…']).collect() } else { e.to_string() };
        tr_with("ui.room.reconnect_last_error", "Last error: %{error}", &[("error", &short)])
    });
    (parts.join(" · "), error)
}

/// The reconnecting banner (mockup `hbox·banner`, amber hatch):
/// `[18 icon][fill text][Retry now][Cancel]`. The text is the headline plus,
/// once the lib has a reconnect entry, a dim status line (attempt, countdown,
/// room full) and a last-error line. Returns the clicked button.
/// `Why` not `widgets::banner`: it has one action and this needs two.
///
/// Narrow (the text column beside the buttons would be under
/// [`BANNER_TEXT_MIN`]; German at a 960 px window ran to ~9 lines next to
/// "Jetzt erneut versuchen"): the buttons move under the text, right-aligned,
/// keeping their words: `[18 icon][fill text]` over `[fill][Retry][Cancel]`.
/// `Why` not icon-only: Retry is the banner's whole point, and no icon says
/// "retry *now*" unambiguously next to the sync icon.
fn reconnect_banner(ui: &mut Ui, info: Option<&ReconnectState>) -> Option<usize> {
    let text = tr("ui.room.reconnecting_banner", "Reconnecting to this room. Your shares here are paused until it's back.");
    let details = info.map(|i| reconnect_details(i, std::time::Instant::now()));
    let retry_l = tr("ui.room.retry", "Retry now");
    let cancel_l = tr("ui.room.cancel", "Cancel");
    let btns = [Btn::secondary(&retry_l), Btn::secondary(&cancel_l)];
    let bw = [btns[0].width(ui), btns[1].width(ui)];
    // The text wraps (German runs ~20 % longer); the banner grows to fit it.
    let pad = vec2(theme::S3 + 1.0, theme::S2 + 1.0);
    let inner_w = ui.available_width() - 2.0 * pad.x;
    let beside_w = inner_w - theme::ICON - bw.iter().map(|w| w + theme::S2).sum::<f32>() - theme::S2;
    let stacked = beside_w < BANNER_TEXT_MIN;
    let text_w = if stacked { inner_w - theme::ICON - theme::S2 } else { beside_w };
    let mut job = egui::text::LayoutJob::single_section(
        text.clone(),
        egui::TextFormat { font_id: font(Weight::Regular, theme::FS_SMALL), color: theme::WARNING, line_height: Some(16.0), ..Default::default() },
    );
    if let Some((status, error)) = &details {
        let dim = egui::TextFormat { font_id: font(Weight::Regular, theme::FS_TINY), color: theme::TEXT_DIM, line_height: Some(15.0), ..Default::default() };
        job.append(&format!("\n{status}"), 0.0, dim.clone());
        if let Some(e) = error {
            job.append(&format!("\n{e}"), 0.0, dim);
        }
    }
    job.wrap.max_width = text_w.max(40.0);
    let galley = ui.painter().layout_job(job);
    let text_h = galley.size().y;
    let inner_h = if stacked { text_h.max(theme::ICON) + theme::S2 + theme::CTL_H } else { text_h.max(theme::CTL_H) };
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), inner_h + 2.0 * pad.y), Sense::hover());
    let r = CornerRadius::same(theme::ROUNDING_MD);
    widgets::paint_hatch(ui.painter(), rect, r, 6.0, theme::HATCH_AMBER_SOFT_A, theme::HATCH_AMBER_SOFT_B);
    ui.painter().rect_stroke(rect, r, Stroke::new(1.0_f32, theme::HATCH_AMBER_BORDER), StrokeKind::Inside);
    let inner = rect.shrink2(pad);
    let mut clicked = None;
    if stacked {
        let mut child = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
        child.spacing_mut().item_spacing.y = theme::S2;
        let top = HBox::new("hbox·banner_text", text_h.max(theme::ICON)).slots(&[fixed(theme::ICON), FILL]).show(&mut child);
        // The icon sits on the first line (16 px line box), not centred on the block.
        let ic = top.cell(0);
        widgets::paint_icon(&child, Rect::from_min_size(ic.min, vec2(ic.width(), 16.0)), "sync_problem", theme::ICON, theme::WARNING);
        let tc = top.cell(1);
        child.painter().with_clip_rect(tc.intersect(child.clip_rect())).galley(tc.min, galley, theme::WARNING);
        let row = HBox::new("hbox·banner_btns", theme::CTL_H).slots(&[FILL, fixed(bw[0]), fixed(bw[1])]).show(&mut child);
        for (i, b) in btns.iter().enumerate() {
            let w = row.cell(i + 1).width();
            if row.show(&mut child, i + 1, Align::Min, |ui| b.show_sized(ui, w)).clicked() {
                clicked = Some(i);
            }
        }
        return clicked;
    }
    let mut child = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
    let slots = [fixed(theme::ICON), FILL, fixed(bw[0]), fixed(bw[1])];
    let row = HBox::new("hbox·banner", inner_h).slots(&slots).show(&mut child);
    row.icon(&child, 0, "sync_problem", theme::ICON, theme::WARNING);
    let tc = row.cell(1);
    child.painter().with_clip_rect(tc.intersect(child.clip_rect())).galley(pos2(tc.left(), tc.center().y - galley.size().y / 2.0), galley, theme::WARNING);
    for (i, b) in btns.iter().enumerate() {
        let w = row.cell(i + 2).width();
        if row.show(&mut child, i + 2, Align::Min, |ui| b.show_sized(ui, w)).clicked() {
            clicked = Some(i);
        }
    }
    clicked
}

/// `vbox·empty` (dashed card, pad 24, gap 8, centred): icon 26, bold title,
/// optional dim line, optional 320 px equal-width button row. Returns the
/// clicked button. Title and line wrap with every row centred (German at a
/// 960 px window: "Geteilte Bildschirme kommen zurück, sobald sich der Raum
/// wieder verbindet." is longer than the card); the card grows to fit.
fn empty_state(ui: &mut Ui, icon: &str, title: &str, text: Option<&str>, btns: &[Btn]) -> Option<usize> {
    const LINE: f32 = 18.0;
    let wrap_w = (ui.available_width() - 48.0).max(40.0);
    let centred = |t: &str, f: egui::FontId, color: Color32| {
        let fmt = egui::TextFormat { font_id: f, color, line_height: Some(LINE), ..Default::default() };
        let mut job = egui::text::LayoutJob::single_section(t.to_owned(), fmt);
        job.wrap.max_width = wrap_w;
        job.halign = Align::Center;
        ui.painter().layout_job(job)
    };
    let title_g = centred(title, font(Weight::Bold, 13.5), theme::TEXT);
    let text_g = text.map(|t| centred(t, font(Weight::Regular, theme::FS_LABEL), theme::TEXT_DIM));
    let title_h = title_g.size().y.max(LINE);
    let text_h = text_g.as_ref().map_or(0.0, |g| theme::S2 + g.size().y.max(LINE));
    let btn_h = btns.iter().map(|b| b.height()).fold(0.0, f32::max);
    let h = 24.0 + 26.0 + theme::S2 + title_h + text_h + if btns.is_empty() { 0.0 } else { 2.0 * theme::S2 + btn_h } + 24.0;
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), h), Sense::hover());
    let r = CornerRadius::same(theme::ROUNDING_WELL);
    widgets::paint_dashed_rect(ui.painter(), rect.shrink(0.5), r, Stroke::new(1.0_f32, theme::BORDER), 4.0, 3.0);
    let inner = rect.shrink(24.0);
    let mut y = inner.top();
    widgets::paint_icon(ui, Rect::from_x_y_ranges(inner.x_range(), y..=y + 26.0), icon, 26.0, theme::TEXT_FAINT);
    y += 26.0 + theme::S2;
    // A centre-aligned galley's rows are laid out around x = 0: paint it at
    // the centre line.
    let mid = inner.center().x;
    let painter = ui.painter().with_clip_rect(inner.intersect(ui.clip_rect()));
    painter.galley(pos2(mid, y), title_g, theme::TEXT);
    y += title_h;
    if let Some(g) = text_g {
        y += theme::S2;
        let gh = g.size().y.max(LINE);
        painter.galley(pos2(mid, y), g, theme::TEXT_DIM);
        y += gh;
    }
    if btns.is_empty() {
        return None;
    }
    y += 2.0 * theme::S2;
    let w = EMPTY_ACTIONS_W.min(inner.width());
    let br = Rect::from_min_size(pos2(inner.center().x - w / 2.0, y), vec2(w, btn_h));
    let mut child = ui.new_child(UiBuilder::new().max_rect(br).layout(Layout::left_to_right(Align::Center)));
    widgets::btn_row(&mut child, btns)
}

// ---------------------------------------------------------------------------
// Tiles
// ---------------------------------------------------------------------------


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tr_falls_back_to_english() {
        assert_eq!(tr("ui.room.__never_a_key__", "Fallback"), "Fallback");
        assert_eq!(tr_with("ui.room.__never_a_key__", "%{a} and %{b}", &[("a", "1"), ("b", "2")]), "1 and 2");
    }
}
