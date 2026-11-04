//! The central area (owner: I14): room header band, room body, People panel,
//! for every [`View`]: a room (joined / reconnecting / disconnected),
//! "Watched streams", and the welcome state.
//!
//! Regions (allocated by the shell, see `ui/mod.rs`):
//! - [`show_header`]: the padded 56 px header band (pad 16), left-to-right.
//!   `[fill hbox·title: h2, status pill, meta][room controls: Auto-connect,
//!   Copy invite link, Leave, ⋯]`, every control 32 px. Owns
//!   [`PopoverKind::RoomMenu`]. In a narrow window the controls collapse to
//!   icons and the meta line drops, in the order commented in `show_header`.
//! - [`show_stage`]: the whole central region of a joined room and of "Watched
//!   streams": the black stage (`ui/stage/`), no scroll area, no padding.
//! - [`show_body`]: the other states (welcome, disconnected, connecting,
//!   reconnecting) as a vbox inside the central ScrollArea (padding 16, gap 16).
//! - [`show_people`]: a 207 px vbox inside the People ScrollArea (padding
//!   16 top/bottom, 12 left/right), gap 12. Only called while the selected
//!   room is joined or reconnecting.
//!
//! # Strings
//! Every string goes through [`tr`]: `t!(key)`, falling back to the English
//! text until the locale files (I17) have the key.

use egui::{Align, Color32, CornerRadius, Layout, Rect, Response, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::session::{ReconnectState, RoomHandle};

use super::{ConnState, Cx, Modal, MyPhase, PopoverKind, RoomEntry, ShareState, UiAction, View, tr, tr_with};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, Btn, FILL, HBox, IconBtnKind, PersonRow, Pill, fixed, text_width};

/// Central-area UI state.
#[derive(Default)]
pub struct RoomState {
    /// The stage's layout geometry, drags and slider state.
    pub stage: super::stage::StageState,
}

// ---------------------------------------------------------------------------
// Sizes (mockup CSS)
// ---------------------------------------------------------------------------

/// Room-name width the header keeps before it collapses its controls (a
/// longer name truncates with "…" once every control is collapsed).
const TITLE_NAME_MIN: f32 = 140.0;
/// Empty-state action row width (`.empty hbox·actions`).
const EMPTY_ACTIONS_W: f32 = 320.0;
/// Reconnect banner: below this text-column width the buttons move under the
/// text (see [`reconnect_banner`]).
const BANNER_TEXT_MIN: f32 = 320.0;

// ---------------------------------------------------------------------------
// Pure layout math (tested)
// ---------------------------------------------------------------------------


/// Header band.
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
    let auto_lbl = tr("ui.room.auto_connect", "Auto-connect");
    let copy_lbl = tr("ui.room.copy_invite", "Copy invite link");
    let leave_lbl = tr("ui.room.leave", "Leave");
    let copy_tip = tr("ui.room.copy_invite_offline_tip", "Connect first: the link includes the relay's certificate");
    let leave_tip = match r.conn {
        ConnState::Joined => tr("ui.room.leave_tip", "Leave room"),
        ConnState::Connecting | ConnState::Reconnecting => tr("ui.room.leave_reconnecting_tip", "Stop connecting to this room"),
        ConnState::Disconnected => tr("ui.room.leave_offline_tip", "You're not connected to this room"),
    };
    let copy_tip_on = if joined.is_none() { format!("{copy_lbl}\n{copy_tip}") } else { copy_lbl.clone() };
    let leave_tip_on = format!("{leave_lbl}\n{leave_tip}");
    // Narrow windows (800 px minimum: the band is ~496 px): the lowest level
    // whose controls leave the title its name (≥ TITLE_NAME_MIN), pill and,
    // below level 2, the whole meta line. Each level keeps the ones before:
    // 1 Copy invite link → icon (it is also in the ⋯ menu), 2 meta line
    // dropped (People and the section headings show the same counts; never
    // half-cut to "2 pe…"), 3 Leave → icon, 4 Auto-connect → bare checkbox
    // (its tooltip says what it is). `Why` this order: redundant things go
    // first, the room's own actions keep their words longest.
    let copy_at = |l: usize| {
        let b = Btn::secondary(&copy_lbl).icon("link").enabled(joined.is_some());
        let b = if joined.is_none() { b.tip(&copy_tip) } else { b };
        if l >= 1 { Btn { label: "", ..b }.tip(&copy_tip_on) } else { b }
    };
    let leave_at = |l: usize| {
        let b = Btn::danger(&leave_lbl).icon("logout").enabled(r.conn != ConnState::Disconnected).tip(&leave_tip);
        if l >= 3 { Btn { label: "", ..b }.tip(&leave_tip_on) } else { b }
    };
    // The checkbox's hover wash reaches 8 px past its text; the mockup leaves
    // 20 px from "Auto-connect" to the next button (12 + the 8 gap).
    let cb_full = theme::MARK + theme::S2 + text_width(ui, &auto_lbl, font(Weight::Regular, theme::FS_LABEL), 0.0).ceil() + theme::S3;
    let cb_at = |l: usize| if l >= 4 { theme::MARK + theme::S3 } else { cb_full };

    // Title: [h2][status pill][meta], gap 12.
    let pill_l = match r.conn {
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
    let meta_w = meta.as_ref().map_or(0.0, |m| text_width(ui, m, meta_f.clone(), 0.0).ceil());
    let title_need = |l: usize| name_w.min(TITLE_NAME_MIN) + theme::S3 + pw + if l < 2 && meta.is_some() { theme::S3 + meta_w } else { 0.0 };
    let controls = |l: usize| cb_at(l) + copy_at(l).width(ui) + leave_at(l).width(ui) + theme::CTL_H + 4.0 * theme::S2;
    let avail = ui.available_width();
    let level = (0..=4).find(|l| avail - controls(*l) >= title_need(*l)).unwrap_or(4);
    let (copy, leave, cb_w) = (copy_at(level), leave_at(level), cb_at(level));
    let show_meta = meta.as_ref().filter(|_| level < 2);
    let slots = [FILL, fixed(cb_w), fixed(copy.width(ui)), fixed(leave.width(ui)), fixed(theme::CTL_H)];
    let row = HBox::new("hbox·room-controls", theme::CTL_H).slots(&slots).show(ui);

    row.show(ui, 0, Align::Min, |ui| {
        let avail = ui.available_width();
        let meta_room = if show_meta.is_some() { theme::S3 + meta_w } else { 0.0 };
        let tw = name_w.min((avail - pw - theme::S3 - meta_room).max(0.0));
        let t = HBox::new("hbox·title", theme::CTL_H).gap(theme::S3).slots(&[fixed(tw), fixed(pw), FILL]).show(ui);
        t.text(ui, 0, &r.name, h2, theme::TEXT, Align::Min);
        let c = t.cell(1);
        pill.paint(ui, Rect::from_center_size(c.center(), vec2(pw, theme::PILL_H)));
        if let Some(m) = show_meta {
            t.text(ui, 2, m, meta_f, theme::TEXT_DIM, Align::Min);
        }
    });

    let mut on = r.auto_connect;
    let auto_tip = tr("ui.room.auto_connect_tip", "Connect to this room when the app starts");
    let (cb_label, auto_tip) = if level >= 4 { (String::new(), format!("{auto_lbl}\n{auto_tip}")) } else { (auto_lbl.clone(), auto_tip) };
    if row.show(ui, 1, Align::Min, |ui| widgets::checkbox(ui, &mut on, &cb_label)).on_hover_text(&auto_tip).changed() {
        cx.act(UiAction::SetAutoConnect { room: r.key.clone(), on });
    }
    let w = row.cell(2).width();
    if row.show(ui, 2, Align::Min, |ui| copy.show_sized(ui, w)).clicked()
        && let Some(h) = joined
    {
        cx.act(UiAction::CopyInviteLink(h));
    }
    let w = row.cell(3).width();
    if row.show(ui, 3, Align::Min, |ui| leave.show_sized(ui, w)).clicked() {
        match (r.conn, r.handle) {
            (ConnState::Joined, Some(h)) => cx.act(UiAction::Leave(h)),
            (ConnState::Connecting | ConnState::Reconnecting, _) => cx.act(UiAction::CancelReconnect(r.key.clone())),
            _ => {}
        }
    }
    let menu_tip = tr("ui.room.options", "Room options");
    if row.show(ui, 4, Align::Min, |ui| widgets::icon_btn(ui, "more_horiz", IconBtnKind::Bordered, &menu_tip)).clicked() {
        cx.toggle_popover(PopoverKind::RoomMenu { room: r.key.clone() }, row.cell(4));
    }
    room_menu(ui, cx, &r, joined);
}

/// The `⋯` popover: Rename / Copy invite link / ─ / Forget room.
fn room_menu(ui: &Ui, cx: &mut Cx, r: &RoomEntry, joined: Option<RoomHandle>) {
    let Some(pop) = cx.popover_of(&PopoverKind::RoomMenu { room: r.key.clone() }) else { return };
    let ctx = ui.ctx().clone();
    let mut picked = None;
    let rename = tr("ui.room.rename", "Rename");
    let copy = tr("ui.room.copy_invite", "Copy invite link");
    let forget = tr("ui.room.forget", "Forget room");
    let forget_sub = tr("ui.room.forget_sub", "Leave and remove it from the list");
    let open = super::popover(&ctx, pop, 340.0, |ui| {
        if widgets::pop_item(ui, &widgets::PopItem::new("edit", &rename)).clicked() {
            picked = Some(UiAction::OpenModal(Modal::Rename { room: r.key.clone() }));
        }
        if widgets::pop_item(ui, &widgets::PopItem::new("content_copy", &copy).enabled(joined.is_some())).clicked()
            && let Some(h) = joined
        {
            picked = Some(UiAction::CopyInviteLink(h));
        }
        // `<hr>`: 1 px DIVIDER with 4 px above and below.
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 9.0), Sense::hover());
        ui.painter().hline(rect.x_range().shrink(theme::S1), rect.center().y.round() + 0.5, Stroke::new(1.0_f32, theme::DIVIDER));
        if widgets::pop_item(ui, &widgets::PopItem::new("delete", &forget).sub(&forget_sub).danger()).clicked() {
            picked = Some(UiAction::OpenModal(Modal::ForgetConfirm { room: r.key.clone() }));
        }
    });
    match picked {
        Some(a @ UiAction::CopyInviteLink(_)) => {
            cx.act(UiAction::ClosePopover);
            cx.act(a);
        }
        Some(a) => cx.act(a),
        None if !open => cx.act(UiAction::ClosePopover),
        None => {}
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
    super::stage::show(ui, cx, &mut st.stage);
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


/// Our row while our share here is still Starting: an accent "in progress"
/// pill (marching bar, no text) in the row's 44 px live slot, where the green
/// LIVE pill goes once it is on air. `Why no text:` "STARTING" does not fit
/// 44 px; the row's tooltip says it. The slot is `person_row`'s
/// `[22][fill][44][18]`, pad 4/4, gap 8, so it ends 4 + 18 + 8 px from the right.
fn starting_badge(ui: &Ui, row: &Response) {
    let pill = Pill::progress("");
    let w = pill.width(ui).min(44.0);
    let right = row.rect.right() - theme::S1 - 18.0 - theme::S2;
    pill.paint(ui, Rect::from_min_size(pos2(right - w, row.rect.center().y - theme::PILL_H / 2.0), vec2(w, theme::PILL_H)));
}

/// People panel: `[heading PEOPLE n][vbox·people → person_row …]`, you first.
pub fn show_people(ui: &mut Ui, cx: &mut Cx, _st: &mut RoomState) {
    let Some(r) = cx.selected_room() else { return };
    let Some((h, s)) = r.handle.and_then(|h| cx.session(h).map(|s| (h, s))) else {
        // Reconnecting: the roster as it was when the room dropped, dimmed
        // (`stale`), us first. The lib's copy leaves us out (`suspend_room`
        // filters our conn id), so our row comes from the display name; it is
        // never live (our shares are paused).
        if let Some(info) = cx.state.reconnect_info(&r.key) {
            widgets::heading(ui, &tr("ui.room.people", "People"), Some(&(info.members.len() + 1).to_string()), false);
            let live_l = tr("ui.pill.live", "Live");
            let you_tag = tr("ui.room.you", "You").to_lowercase();
            let stale_tip = tr("ui.room.people_stale_tip", "As it was before the connection dropped");
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let me = PersonRow { name: &cx.state.display_name, colors: theme::avatar_colors(&cx.state.display_name), you: Some(&you_tag), live: false, live_label: &live_l, watching: false, idle: true, stale: true };
                widgets::person_row(ui, &me).on_hover_text(&stale_tip);
                for m in &info.members {
                    let live = info.streams.iter().any(|i| i.owner == m.conn_id);
                    let row = PersonRow { name: &m.display_name, colors: theme::avatar_colors(&m.display_name), you: None, live, live_label: &live_l, watching: false, idle: !live, stale: true };
                    widgets::person_row(ui, &row).on_hover_text(&stale_tip);
                }
            });
            return;
        }
        widgets::heading(ui, &tr("ui.room.people", "People"), None, false);
        // Wraps: the German line is wider than the 207 px column.
        let text = if r.conn == ConnState::Connecting {
            tr("ui.room.people_connecting", "Shown once the room is connected")
        } else {
            tr("ui.room.people_paused", "Back when the room reconnects")
        };
        let mut job = egui::text::LayoutJob::single_section(
            text,
            egui::TextFormat { font_id: font(Weight::Regular, theme::FS_SMALL), color: theme::TEXT_FAINT, line_height: Some(16.0), ..Default::default() },
        );
        job.wrap.max_width = ui.available_width();
        let galley = ui.painter().layout_job(job);
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), galley.size().y.max(theme::ROW_H)), Sense::hover());
        ui.painter().galley(pos2(rect.left(), rect.center().y - galley.size().y / 2.0), galley, theme::TEXT_FAINT);
        return;
    };
    let me = s.client.conn_id();
    let my_phase = cx.my_phase(h);
    let starting_l = tr("ui.room.people_starting", "Starting…");
    let starting_tip = if cx.live_captures().iter().any(|(id, c)| c.destinations.values().any(|d| d.room == h) && cx.state.capture_awaiting_first_frame(*id)) {
        format!("{starting_l}\n{}", tr("ui.waiting_frame_hint", "An idle screen sends no picture until something on it changes. Move the mouse over the shared screen and the stream starts."))
    } else {
        starting_l.to_string()
    };
    widgets::heading(ui, &tr("ui.room.people", "People"), Some(&s.members.len().to_string()), false);
    let watches = s.watches.lock().unwrap();
    let audio = s.audio_watches.lock().unwrap();
    // The mockup's faint lowercase "you" tag / "LIVE" pill, from existing keys.
    let you_tag = tr("ui.room.you", "You").to_lowercase();
    let live_l = tr("ui.pill.live", "Live");
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let order = s.members.iter().filter(|m| m.conn_id == me).chain(s.members.iter().filter(|m| m.conn_id != me));
        for m in order {
            let you = m.conn_id == me;
            let live = if you { my_phase == Some(MyPhase::OnAir) } else { s.streams.iter().any(|i| i.owner == m.conn_id) };
            let starting = you && my_phase == Some(MyPhase::Starting);
            let watching = !you && (watches.keys().any(|k| k.0 == m.conn_id) || audio.iter().any(|(k, a)| k.0 == m.conn_id && !a.auto));
            let resp = widgets::person_row(
                ui,
                &PersonRow {
                    name: &m.display_name,
                    colors: theme::avatar_colors(&m.display_name),
                    you: you.then_some(you_tag.as_str()),
                    live,
                    live_label: &live_l,
                    watching,
                    idle: !live && !starting,
                    stale: false,
                },
            );
            if starting {
                starting_badge(ui, &resp);
                resp.on_hover_text(&starting_tip);
            }
        }
    });
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tr_falls_back_to_english() {
        assert_eq!(tr("ui.room.__never_a_key__", "Fallback"), "Fallback");
        assert_eq!(tr_with("ui.room.__never_a_key__", "%{a} and %{b}", &[("a", "1"), ("b", "2")]), "1 and 2");
    }
}
