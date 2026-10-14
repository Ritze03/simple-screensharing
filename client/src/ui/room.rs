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
//! - [`show_body`]: a vbox inside the central ScrollArea (padding 16), gap 16
//!   between sections: reconnecting banner / "On screen" tiles / "Sharing in
//!   this room" cards / empty states.
//! - [`show_people`]: a 207 px vbox inside the People ScrollArea (padding
//!   16 top/bottom, 12 left/right), gap 12. Only called while the selected
//!   room is joined or reconnecting.
//!
//! # Tiles
//! A tile is a 16:9 frame with the GPU video letterboxed into it
//! (`cx.videos.show`) and a hover overlay (vignette, statbar, round Pop out /
//! Stop watching buttons, audio pill). Layout (mockup `renderTiles`):
//! - in a room: one tile → one column capped at 46 % of the window height ×
//!   16:9 ([`single_tile_width`]) so the share cards stay in view; more → two
//!   equal columns ([`room_tile_cols`]).
//! - "Watched streams": every watched stream, so the tiles are fitted into
//!   the viewport with `grid_shape` (a port of the former Electron frontend's `computeGridShape`) ([`grid_shape`]).
//!   `Why` not the room rule there: that view has nothing below the tiles, so
//!   the biggest tiles that fit beat a fixed two-column grid that scrolls.
//!
//! # Performance
//! Nothing here requests a repaint: video repaints are frame-driven inside
//! `VideoView`. Decoder stats (and the decoder lock for the bitrate) are only
//! read while a tile's overlay is visible (hover).
//!
//! # Strings
//! Every string goes through [`tr`]: `t!(key)`, falling back to the English
//! text until the locale files (I17) have the key.


use egui::epaint::{Mesh, RectShape, Vertex, WHITE_UV};
use egui::{Align, Color32, CornerRadius, Id, Layout, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::capture::VideoCodec;
use simple_screensharing_client::capture::destination::DestinationState;
use simple_screensharing_client::decode::render::Fit;
use simple_screensharing_client::protocol::{ConnId, StreamInfo, StreamKind};
use simple_screensharing_client::session::{CaptureSourceId, DestinationId, ReconnectState, RoomHandle};

use super::{ConnState, Cx, Modal, MyPhase, PopoverKind, RoomEntry, ShareState, StreamKey, UiAction, View, tr, tr_with};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, Btn, FILL, HBox, IconBtnKind, PersonRow, Pill, fixed, paint_text, text_width};

/// Central-area UI state.
#[derive(Default)]
pub struct RoomState {
    /// A tile volume slider being dragged: the value shown until the
    /// `SetVolume` pushed on release has landed in the session.
    vol_drag: Option<(StreamKey, u32)>,
}

// ---------------------------------------------------------------------------
// Sizes (mockup CSS)
// ---------------------------------------------------------------------------

/// Tile corner radius (`.tile`).
const TILE_R: u8 = 6;
/// Statbar / round button / audio pill height.
const OVL_H: f32 = 29.0;
/// Share-card info block: pad 12 + who 22 + 8 + specs 16 + 8 + buttons 32 + pad 12.
const CARD_INFO_H: f32 = 12.0 + 22.0 + 8.0 + 16.0 + 8.0 + 32.0 + 12.0;
/// A single "On screen" tile is at most this fraction of the window height tall.
const SINGLE_TILE_VH: f32 = 0.46;
/// Smallest tile width the layouts aim for (a grid column's minimum).
const MIN_TILE_W: f32 = theme::GRID_PITCH - theme::GRID_GAP;
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

/// Width of a lone "On screen" tile: the full width, capped so the tile is at
/// most [`SINGLE_TILE_VH`] of the window height tall at 16:9 (mockup
/// `max-width: calc(46vh * 16 / 9)`), never below [`MIN_TILE_W`] unless the
/// area itself is narrower.
pub fn single_tile_width(avail_w: f32, window_h: f32) -> f32 {
    let cap = (window_h * SINGLE_TILE_VH * 16.0 / 9.0).max(MIN_TILE_W);
    avail_w.min(cap).max(0.0)
}

/// Columns for the room's "On screen" tiles (mockup: 1 for one tile, else 2),
/// dropping to 1 when two columns would be narrower than [`MIN_TILE_W`].
pub fn room_tile_cols(n: usize, avail_w: f32) -> usize {
    if n <= 1 || avail_w < 2.0 * MIN_TILE_W + theme::GRID_GAP { 1 } else { 2 }
}

/// Port of the former Electron frontend's `computeGridShape` (removed in the egui rework): the
/// column count and tile width that give `n` 16:9 tiles the largest area in
/// `w × h` with `gap` between them. Shapes whose tiles would be narrower than
/// [`MIN_TILE_W`] are skipped (the view then scrolls instead of shrinking
/// tiles to stamps).
pub fn grid_shape(n: usize, w: f32, h: f32, gap: f32) -> (usize, f32) {
    let w = w.max(0.0);
    let h = h.max(0.0);
    if n <= 1 {
        let tw = if h > 0.0 { w.min(h * 16.0 / 9.0) } else { w };
        return (1, tw.max(MIN_TILE_W.min(w)));
    }
    let mut best: Option<(usize, f32, f32)> = None;
    for cols in 1..=n {
        let rows = n.div_ceil(cols);
        let cell_w = (w - gap * (cols as f32 - 1.0)) / cols as f32;
        let cell_h = (h - gap * (rows as f32 - 1.0)) / rows as f32;
        if cell_h <= 0.0 {
            continue;
        }
        let tw = cell_w.min(cell_h * 16.0 / 9.0);
        if tw < MIN_TILE_W.min(w) {
            continue;
        }
        let area = tw * tw * 9.0 / 16.0;
        if best.is_none_or(|b| area > b.2) {
            best = Some((cols, tw, area));
        }
    }
    match best {
        Some((cols, tw, _)) => (cols, tw),
        // Nothing fits the height: as many full-width-ish columns as fit.
        None => {
            let cols = (((w + gap) / (MIN_TILE_W + gap)).floor() as usize).clamp(1, n);
            (cols, (w - gap * (cols as f32 - 1.0)) / cols as f32)
        }
    }
}


/// Avatar gradient for a member: ours is the accent one (as in the user
/// band), everyone else gets one of the other four, stable per connection.
fn avatar_color(me: bool, conn: ConnId) -> usize {
    if me { 0 } else { 1 + (conn % 4) as usize }
}

fn initial(name: &str) -> String {
    name.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default()
}

fn codec_label(c: VideoCodec) -> &'static str {
    match c {
        VideoCodec::H264 => "H.264",
        VideoCodec::H265 => "H.265",
        VideoCodec::Av1 => "AV1",
    }
}

fn fmt_kbps(kbps: f64) -> String {
    if kbps >= 1000.0 { format!("{:.1} Mbps", kbps / 1000.0) } else { format!("{kbps:.0} kbps") }
}

// ---------------------------------------------------------------------------
// Header band
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
        ConnState::Reconnecting => tr("ui.room.leave_reconnecting_tip", "Stop connecting to this room"),
        ConnState::Disconnected => tr("ui.room.leave_offline_tip", "You're not connected to this room"),
    };
    let copy_tip_on = if joined.is_none() { format!("{copy_lbl}\n{copy_tip}") } else { copy_lbl.clone() };
    let leave_tip_on = format!("{leave_lbl}\n{leave_tip}");
    // Narrow windows (960 px minimum: the band is ~656 px): the lowest level
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
        ConnState::Reconnecting => tr("ui.room.reconnecting_pill", "Reconnecting"),
        ConnState::Disconnected => tr("ui.room.not_connected", "Not connected"),
    };
    let pill = match r.conn {
        ConnState::Joined => Pill::live(&pill_l).lead(widgets::PillLead::Dot),
        ConnState::Reconnecting => Pill::degraded(&pill_l),
        ConnState::Disconnected => Pill::muted(&pill_l),
    };
    let meta = r.handle.and_then(|h| cx.session(h).map(|s| (h, s.members.len()))).map(|(h, n)| {
        let sharing = cx.shares_in(h).len() + usize::from(matches!(cx.share_state(&r), ShareState::Live { .. }));
        if n == 1 {
            tr_with("ui.room.meta_one", "1 person · %{sharing} sharing", &[("sharing", &sharing.to_string())])
        } else {
            tr_with("ui.room.meta", "%{people} people · %{sharing} sharing", &[("people", &n.to_string()), ("sharing", &sharing.to_string())])
        }
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
            (ConnState::Reconnecting, _) => cx.act(UiAction::CancelReconnect(r.key.clone())),
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

/// Body for the current view.
pub fn show_body(ui: &mut Ui, cx: &mut Cx, st: &mut RoomState) {
    match &cx.nav.view {
        View::Welcome => {
            let title = tr("ui.room.welcome_title", "Pick a room on the left");
            let text = tr("ui.room.welcome_text", "Or create one and send its invite link to your friends.");
            let create = tr("ui.sidebar.create_or_join", "Create or join a room");
            if empty_state(ui, "meeting_room", &title, Some(&text), &[Btn::primary(&create).icon("add").large()]) == Some(0) {
                cx.act(UiAction::OpenModal(Modal::CreateJoin));
            }
        }
        View::Watching => {
            let keys = cx.watched();
            if keys.is_empty() {
                let title = tr("ui.room.watching_empty_title", "You're not watching anything");
                let text = tr("ui.room.watching_empty_text", "Open a room and click Watch on a share.");
                empty_state(ui, "visibility_off", &title, Some(&text), &[]);
                return;
            }
            // The viewport minus the body padding (the clip rect is the
            // ScrollArea's viewport).
            let h = ui.clip_rect().height() - 2.0 * theme::S4;
            let (cols, tw) = grid_shape(keys.len(), ui.available_width(), h, theme::GRID_GAP);
            tiles(ui, cx, st, &keys, true, cols, tw);
        }
        View::Room(_) => {
            let Some(r) = cx.selected_room() else { return };
            room_body(ui, cx, st, &r);
        }
    }
}

/// One card in "Sharing in this room".
enum CardItem<'a> {
    Share(&'a StreamInfo),
    Mine { capture: CaptureSourceId, destination: DestinationId },
}

fn room_body(ui: &mut Ui, cx: &mut Cx, st: &mut RoomState, r: &RoomEntry) {
    match (r.conn, r.handle) {
        (ConnState::Disconnected, _) => {
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
        (ConnState::Joined, Some(h)) => joined_body(ui, cx, st, r, h),
    }
}

fn joined_body(ui: &mut Ui, cx: &mut Cx, st: &mut RoomState, r: &RoomEntry, h: RoomHandle) {
    let watched: Vec<StreamKey> = cx.watched().into_iter().filter(|k| k.0 == h).collect();
    if !watched.is_empty() {
        let n = watched.len();
        section(ui, &tr("ui.room.on_screen", "On screen"), n, |ui| {
            let avail = ui.available_width();
            let cols = room_tile_cols(n, avail);
            let tw = if n == 1 { single_tile_width(avail, ui.ctx().content_rect().height()) } else { 0.0 };
            tiles(ui, cx, st, &watched, false, cols, tw);
        });
    }

    // Cards: shares not on screen (audio-only senders always), then ours.
    let mut items: Vec<CardItem> = cx
        .shares_in(h)
        .into_iter()
        .filter(|i| i.kind == StreamKind::Audio || !watched.contains(&(h, i.owner, i.stream_id)))
        .map(CardItem::Share)
        .collect();
    if let ShareState::Live { capture, destination } = cx.share_state(r) {
        items.push(CardItem::Mine { capture, destination });
    }
    let title = if watched.is_empty() { tr("ui.room.sharing", "Sharing in this room") } else { tr("ui.room.also_sharing", "Also sharing in this room") };
    let mut acts = Vec::new();
    section(ui, &title, items.len(), |ui| {
        if items.is_empty() {
            let (title, text) = if watched.is_empty() {
                (tr("ui.room.nobody_title", "Nobody is sharing yet"), Some(tr("ui.room.nobody_text", "Start your own share with the bar below.")))
            } else {
                (tr("ui.room.all_watched_title", "You're watching everything shared here"), None)
            };
            empty_state(ui, "tv_off", &title, text.as_deref(), &[]);
            return;
        }
        let cxr: &Cx = cx;
        widgets::grid_rows(ui, "room_cards", items.len(), |ui, i| match &items[i] {
            CardItem::Share(info) => share_card(ui, cxr, h, info, &mut acts),
            CardItem::Mine { capture, destination } => my_card(ui, cxr, h, *capture, *destination, &mut acts),
        });
    });
    for a in acts {
        cx.act(a);
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

/// A grid of video tiles: `cols` equal columns; `tile_w > 0` caps each tile's
/// width (left-aligned in its column). `cross_room` labels tiles with the
/// room name too ("Watched streams").
fn tiles(ui: &mut Ui, cx: &mut Cx, st: &mut RoomState, keys: &[StreamKey], cross_room: bool, cols: usize, tile_w: f32) {
    let mut acts = Vec::new();
    let cxr: &Cx = cx;
    widgets::grid_rows_with(ui, cols, keys.len(), |ui, i| {
        let key = keys[i];
        let w = if tile_w > 0.0 { ui.available_width().min(tile_w) } else { ui.available_width() };
        let size = vec2(w, (w * 9.0 / 16.0).round());
        if cxr.is_popped(key) {
            popped_tile(ui, cxr, key, size, cross_room, &mut acts);
        } else if let Some(resp) = cxr.videos.show(key, ui, size, Fit::Contain) {
            tile_chrome(ui, resp.rect);
            tile_overlay(ui, cxr, st, key, resp.rect, cross_room, &mut acts);
        }
    });
    for a in acts {
        cx.act(a);
    }
}

/// Rounded corners + the faint green ring around a tile's video. `Why` the
/// corner masks: the video is a wgpu paint callback that can't be clipped to
/// a rounded rect, so the body background is painted over its corners.
fn tile_chrome(ui: &Ui, rect: Rect) {
    let p = ui.painter();
    let r = TILE_R as f32;
    let mut mesh = Mesh::default();
    for (corner, sx, sy) in [(rect.left_top(), 1.0, 1.0), (rect.right_top(), -1.0, 1.0), (rect.left_bottom(), 1.0, -1.0), (rect.right_bottom(), -1.0, -1.0)] {
        // Triangle fan from the corner point over the quarter arc.
        let c = pos2(corner.x + sx * r, corner.y + sy * r);
        let base = mesh.vertices.len() as u32;
        mesh.vertices.push(Vertex { pos: corner, uv: WHITE_UV, color: theme::PANEL_BASE });
        const STEPS: u32 = 6;
        for s in 0..=STEPS {
            let a = std::f32::consts::FRAC_PI_2 * s as f32 / STEPS as f32;
            let pos = pos2(c.x - sx * r * a.cos(), c.y - sy * r * a.sin());
            mesh.vertices.push(Vertex { pos, uv: WHITE_UV, color: theme::PANEL_BASE });
        }
        for s in 0..STEPS {
            mesh.indices.extend_from_slice(&[base, base + 1 + s, base + 2 + s]);
        }
    }
    p.add(Shape::mesh(mesh));
    p.rect_stroke(rect, CornerRadius::same(TILE_R), Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(80, 200, 120, 51)), StrokeKind::Outside);
}

/// "X is open in its own window" placeholder for a popped-out stream:
/// dashed accent 16:9 frame, `[icon][text][hbox·actions 300: Bring back, Stop watching]`.
fn popped_tile(ui: &mut Ui, cx: &Cx, key: StreamKey, size: egui::Vec2, cross_room: bool, acts: &mut Vec<UiAction>) {
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let r = CornerRadius::same(TILE_R);
    let p = ui.painter();
    p.rect_filled(rect, r, theme::ACCENT_GHOST_BG);
    widgets::paint_dashed_rect(p, rect.shrink(0.75), r, Stroke::new(1.5_f32, theme::ACCENT_GHOST_BORDER), 5.0, 4.0);
    let name = cx.member_name(key.0, key.1);
    let who = if cross_room { format!("{name} · {}", cx.room_name(key.0)) } else { name };
    let text = tr("ui.room.popped_text", "is open in its own window");
    let back = tr("ui.room.bring_back", "Bring back");
    let stop = tr("ui.room.stop_watching", "Stop watching");
    let btns = [Btn::secondary(&back).icon("move_down"), Btn::danger(&stop).icon("close")];
    // Centred vbox, gap 12: icon 28 / text line 18 / buttons 32.
    let block_h = 28.0 + theme::S3 + 18.0 + theme::S3 + theme::CTL_H;
    let top = rect.center().y - block_h / 2.0;
    let line = |y: f32, h: f32| Rect::from_x_y_ranges(rect.x_range().shrink(theme::S3), y..=y + h);
    widgets::paint_icon(ui, line(top, 28.0), "open_in_new", 28.0, theme::ACCENT_LIGHTER);
    let ty = top + 28.0 + theme::S3;
    let bold = font(Weight::Bold, theme::FS_LABEL);
    let reg = font(Weight::Regular, theme::FS_LABEL);
    let (ww, tw) = (text_width(ui, &who, bold.clone(), 0.0), text_width(ui, &text, reg.clone(), 0.0) + 4.0);
    let x0 = (rect.center().x - (ww + tw) / 2.0).max(rect.left() + theme::S3);
    let wr = paint_text(ui, Rect::from_x_y_ranges(x0..=rect.right() - theme::S3, ty..=ty + 18.0), &who, bold, theme::TEXT, Align::Min);
    paint_text(ui, Rect::from_x_y_ranges(wr.right() + 4.0..=rect.right() - theme::S3, ty..=ty + 18.0), &text, reg, theme::TEXT_DIM, Align::Min);
    let bw = 300.0_f32.min(rect.width() - 2.0 * theme::S3);
    let by = ty + 18.0 + theme::S3;
    let br = Rect::from_min_size(pos2(rect.center().x - bw / 2.0, by), vec2(bw, theme::CTL_H));
    let mut child = ui.new_child(UiBuilder::new().max_rect(br).layout(Layout::left_to_right(Align::Center)));
    match widgets::btn_row(&mut child, &btns) {
        Some(0) => acts.push(UiAction::BringBack(key)),
        Some(1) => acts.push(UiAction::Unwatch(key)),
        _ => {}
    }
}

/// The hover overlay of a tile: vignette, `[hbox·top: fill statbar, Pop out,
/// Stop watching]`, `[hbox·bottom: audio pill]`, inset 8. Fades in over
/// 0.12 s while the pointer is over the tile; nothing is built at alpha 0.
fn tile_overlay(ui: &mut Ui, cx: &Cx, st: &mut RoomState, key: StreamKey, rect: Rect, cross_room: bool, acts: &mut Vec<UiAction>) {
    let id = Id::new(("tile_overlay", key));
    let hovered = ui.rect_contains_pointer(rect);
    let alpha = ui.ctx().animate_bool_with_time(id, hovered, 0.12);
    if alpha <= 0.0 {
        return;
    }
    let mut ov = ui.new_child(UiBuilder::new().id_salt(id).max_rect(rect).layout(Layout::left_to_right(Align::Center)));
    ov.set_clip_rect(rect.intersect(ui.clip_rect()));
    ov.multiply_opacity(alpha);

    // Vignette: .55 → .1 over the top 40 %, → .82 at the bottom.
    let v = |a: f32| Color32::from_rgba_unmultiplied(10, 11, 13, (a * 255.0) as u8);
    let split = rect.top() + rect.height() * 0.4;
    let tr_ = TILE_R;
    let top_r = CornerRadius { nw: tr_, ne: tr_, sw: 0, se: 0 };
    let bot_r = CornerRadius { nw: 0, ne: 0, sw: tr_, se: tr_ };
    widgets::paint_gradient(ov.painter(), Rect::from_min_max(rect.min, pos2(rect.right(), split)), top_r, 180.0, v(0.55), v(0.10));
    widgets::paint_gradient(ov.painter(), Rect::from_min_max(pos2(rect.left(), split), rect.max), bot_r, 180.0, v(0.10), v(0.82));

    let inner = rect.shrink(theme::S2);
    // --- top: [fill statbar][29 pop out][29 stop], gaps 8 / 4 -------------
    let top = Rect::from_min_size(inner.min, vec2(inner.width(), OVL_H));
    let stop_r = Rect::from_min_size(pos2(top.right() - OVL_H, top.top()), vec2(OVL_H, OVL_H));
    let pop_r = stop_r.translate(vec2(-(OVL_H + theme::S1), 0.0));
    let bar_cell = Rect::from_min_max(top.min, pos2(pop_r.left() - theme::S2, top.bottom()));
    statbar(&mut ov, cx, key, bar_cell, cross_room);
    if round_btn(&mut ov, pop_r, id.with("pop"), "open_in_new", &tr("ui.room.pop_out_tip", "Pop out to its own window"), false).clicked() {
        acts.push(UiAction::PopOut(key));
    }
    if round_btn(&mut ov, stop_r, id.with("stop"), "close", &tr("ui.room.stop_watching", "Stop watching"), true).clicked() {
        acts.push(UiAction::Unwatch(key));
    }

    // --- bottom: audio pill, or "No audio shared" --------------------------
    let bottom = Rect::from_min_size(pos2(inner.left(), inner.bottom() - OVL_H), vec2(inner.width(), OVL_H));
    let has_audio = cx.session(key.0).is_some_and(|s| s.sibling_audio_stream(key.1).is_some());
    if has_audio {
        audio_pill(&mut ov, cx, st, key, bottom, acts);
    } else {
        let mut c = ov.new_child(UiBuilder::new().max_rect(bottom).layout(Layout::left_to_right(Align::Center)));
        widgets::chip_dark(&mut c, Some("volume_off"), false, &tr("ui.room.no_audio", "No audio shared"));
    }
}

/// `hbox·statbar` (29 px pill, pad 10, gap 8): `[7 dot][name][res][fps 6ch][rate 8ch]`,
/// content-sized up to `cell`'s width (the name truncates). The numbers sit in
/// fixed tabular-width slots so the bar doesn't jitter as they change. Hover
/// shows decoded vs presented fps (presentation must keep up with decode).
fn statbar(ui: &mut Ui, cx: &Cx, key: StreamKey, cell: Rect, cross_room: bool) {
    let Some(entry) = cx.videos.get(key) else { return };
    let stats = entry.view.lock().unwrap().source().stats();
    let kbps = cx
        .session(key.0)
        .and_then(|s| s.watches.lock().unwrap().get(&(key.1, key.2)).map(|w| w.decoder.lock().unwrap().received_bitrate_kbps()))
        .unwrap_or(0.0);
    let name = cx.member_name(key.0, key.1);
    let name = if cross_room { format!("{name} · {}", cx.room_name(key.0)) } else { name };
    let res = cx.videos.frame_size(key).map(|(w, h)| format!("{w}x{h}")).unwrap_or_default();
    let fps = format!("{:.0}fps", stats.decoded_fps);
    let rate = fmt_kbps(kbps);

    let nf = font(Weight::Bold, theme::FS_ROW);
    let mf = font(Weight::Mono, theme::FS_SMALL);
    let ch = text_width(ui, "0", mf.clone(), 0.0);
    let (res_w, fps_w, rate_w) = (text_width(ui, &res, mf.clone(), 0.0).ceil(), (6.0 * ch).ceil(), (8.0 * ch).ceil());
    let fixed_w = 10.0 + 7.0 + theme::S2 + theme::S2 + res_w + theme::S2 + fps_w + theme::S2 + rate_w + 10.0;
    let name_w = text_width(ui, &name, nf.clone(), 0.0).ceil().min((cell.width() - fixed_w).max(0.0));
    let w = (fixed_w + name_w).min(cell.width());
    let bar = Rect::from_min_size(cell.min, vec2(w, OVL_H));
    let p = ui.painter();
    p.rect_filled(bar, CornerRadius::same(15), Color32::from_black_alpha(158));
    let mut child = ui.new_child(UiBuilder::new().max_rect(bar).layout(Layout::left_to_right(Align::Center)));
    child.shrink_clip_rect(bar);
    let slots = [fixed(7.0), fixed(name_w), fixed(res_w), fixed(fps_w), fixed(rate_w)];
    let row = HBox::new("hbox·statbar", OVL_H).pad(10.0, 10.0).slots(&slots).show(&mut child);
    let c = row.cell(0).center();
    child.painter().circle_filled(c, 5.0, theme::SUCCESS_GLOW.gamma_multiply(0.25));
    child.painter().circle_filled(c, 3.5, theme::SUCCESS);
    let num = Color32::from_rgba_unmultiplied(232, 233, 236, 217);
    row.text(&child, 1, &name, nf, theme::TEXT, Align::Min);
    row.text(&child, 2, &res, mf.clone(), num, Align::Min);
    row.text(&child, 3, &fps, mf.clone(), num, Align::Max);
    row.text(&child, 4, &rate, mf, num, Align::Max);
    row.response.on_hover_ui(|ui| {
        let codec = stats.info.codec.map(|c| c.name()).unwrap_or("—");
        let dec = stats.info.decoder_name.as_deref().unwrap_or("—");
        ui.label(tr_with(
            "ui.room.stats_tip",
            "Decoded %{decoded} fps · shown %{presented} fps\n%{codec} via %{decoder} · %{dropped} dropped",
            &[
                ("decoded", &format!("{:.0}", stats.decoded_fps)),
                ("presented", &format!("{:.0}", stats.presented_fps)),
                ("codec", codec),
                ("decoder", dec),
                ("dropped", &stats.access_units_dropped.to_string()),
            ],
        ));
    });
}

/// `.round-btn`: 29 px circle, translucent dark fill, faint white ring,
/// 16 px icon; `stop` turns the icon red on hover.
fn round_btn(ui: &mut Ui, rect: Rect, id: Id, icon: &str, tip: &str, stop: bool) -> Response {
    let resp = ui.interact(rect, id, Sense::click());
    let hover = resp.hovered();
    let p = ui.painter();
    let fill = Color32::from_rgba_unmultiplied(16, 17, 20, if hover { 204 } else { 140 });
    p.circle(rect.center(), rect.width() / 2.0 - 0.5, fill, Stroke::new(1.0_f32, Color32::from_white_alpha(41)));
    let col = if hover && stop { theme::DANGER } else { theme::TEXT };
    widgets::paint_icon(ui, rect, icon, 16.0, col);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

/// `hbox·audio-pill` (29 px, pad 2/10, gap 4): `[25 mute][110 volume 0–200 %][34 pct]`,
/// or `[25 mute][Muted]` while muted. The slider pushes `SetVolume` on release.
fn audio_pill(ui: &mut Ui, cx: &Cx, st: &mut RoomState, key: StreamKey, bottom: Rect, acts: &mut Vec<UiAction>) {
    let Some(s) = cx.session(key.0) else { return };
    let vol = s.volume(key.1, key.2);
    let shown = st.vol_drag.filter(|(k, _)| *k == key).map_or(vol.volume_percent, |(_, v)| v);
    let muted_l = tr("ui.room.muted", "Muted");
    let pct = format!("{shown}%");
    let tf = font(Weight::Mono, theme::FS_TINY);
    let tail = if vol.muted { text_width(ui, &muted_l, tf.clone(), 0.0).ceil() } else { 110.0 + theme::S1 + 34.0 };
    let w = 2.0 + 25.0 + theme::S1 + tail + 10.0;
    let pill = Rect::from_min_size(bottom.min, vec2(w.min(bottom.width()), OVL_H));
    ui.painter().rect(pill, CornerRadius::same(15), Color32::from_black_alpha(184), Stroke::new(1.0_f32, Color32::from_white_alpha(46)), StrokeKind::Inside);

    let mut child = ui.new_child(UiBuilder::new().max_rect(pill).layout(Layout::left_to_right(Align::Center)));
    child.shrink_clip_rect(pill);
    let slots_muted = [fixed(25.0), fixed(tail)];
    let slots = [fixed(25.0), fixed(110.0), fixed(34.0)];
    let row = HBox::new("hbox·audio-pill", OVL_H).gap(theme::S1).pad(2.0, 10.0).slots(if vol.muted { &slots_muted } else { &slots }).show(&mut child);

    // Mute toggle: 25 px round, red icon while muted.
    let mr = Rect::from_center_size(row.cell(0).center(), vec2(25.0, 25.0));
    let mresp = child.interact(mr, Id::new(("tile_mute", key)), Sense::click());
    if mresp.hovered() {
        child.painter().circle_filled(mr.center(), 12.5, Color32::from_white_alpha(20));
    }
    let (icon, col, tip) = if vol.muted {
        ("volume_off", theme::DANGER, tr("ui.room.unmute", "Listen"))
    } else {
        ("volume_up", theme::TEXT, tr("ui.room.mute", "Mute this stream"))
    };
    widgets::paint_icon(&child, mr, icon, 16.0, col);
    if mresp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(&tip).clicked() {
        acts.push(UiAction::SetVolume { stream: key, volume_percent: vol.volume_percent, muted: !vol.muted });
    }
    let num = Color32::from_rgba_unmultiplied(232, 233, 236, 217);
    if vol.muted {
        row.text(&child, 1, &muted_l, tf, num, Align::Min);
        return;
    }
    // The slider is 32 px tall; centre it on the 29 px pill.
    let sc = row.cell(1);
    let sr = Rect::from_center_size(sc.center(), vec2(sc.width(), theme::CTL_H));
    let mut sl = child.new_child(UiBuilder::new().id_salt(("tile_vol", key)).max_rect(sr).layout(Layout::left_to_right(Align::Center)));
    let mut v = shown;
    let r = widgets::slider(&mut sl, &mut v, 0..=200, 5.0).on_hover_text(tr("ui.room.volume", "Volume"));
    if r.dragged() || r.changed() {
        st.vol_drag = Some((key, v));
    }
    if r.drag_stopped() || (r.changed() && !r.dragged()) {
        acts.push(UiAction::SetVolume { stream: key, volume_percent: v, muted: false });
    } else if !r.dragged() && st.vol_drag.is_some_and(|(k, d)| k == key && d == vol.volume_percent) {
        // The pushed value has landed: stop overriding it.
        st.vol_drag = None;
    }
    row.text(&child, 2, &pct, tf, num, Align::Max);
}

// ---------------------------------------------------------------------------
// Share cards
// ---------------------------------------------------------------------------

/// Card frame (`.scard`): `PANEL_RAISED`, 1 px border, radius 12, soft shadow;
/// returns `(thumb rect, info rect)` — a 16:9 thumb inside the border, then
/// the info block ([`CARD_INFO_H`]).
fn card_frame(ui: &mut Ui) -> (Rect, Rect) {
    let w = ui.available_width();
    let thumb_h = ((w - 2.0) * 9.0 / 16.0).round();
    let (rect, _) = ui.allocate_exact_size(vec2(w, 1.0 + thumb_h + CARD_INFO_H + 1.0), Sense::hover());
    let r = CornerRadius::same(theme::ROUNDING_WELL);
    let p = ui.painter();
    p.add(egui::Shadow { offset: [0, 6], blur: 18, spread: 0, color: Color32::from_black_alpha(46) }.as_shape(rect, r));
    p.add(egui::Shadow { offset: [0, 1], blur: 2, spread: 0, color: Color32::from_black_alpha(64) }.as_shape(rect, r));
    p.rect(rect, r, theme::PANEL_RAISED, Stroke::new(1.0_f32, theme::BORDER), StrokeKind::Inside);
    let thumb = Rect::from_min_size(rect.min + vec2(1.0, 1.0), vec2(w - 2.0, thumb_h));
    let info = Rect::from_min_max(pos2(rect.left() + 1.0, thumb.bottom()), rect.max - vec2(1.0, 1.0));
    (thumb, info)
}

fn thumb_radius() -> CornerRadius {
    let r = theme::ROUNDING_WELL - 1;
    CornerRadius { nw: r, ne: r, sw: 0, se: 0 }
}

/// `hbox·corners` over a thumb (inset 8): `[chip][fill][audio chip?]`.
fn thumb_corners(ui: &mut Ui, thumb: Rect, live: Option<&str>, audio: bool) {
    let row = Rect::from_min_size(thumb.min + vec2(theme::S2, theme::S2), vec2(thumb.width() - 2.0 * theme::S2, 22.0));
    let mut c = ui.new_child(UiBuilder::new().max_rect(row).layout(Layout::left_to_right(Align::Center)));
    c.shrink_clip_rect(thumb);
    match live {
        Some(t) if t == "LIVE" => widgets::chip_dark(&mut c, None, true, t),
        Some(t) => widgets::chip_dark(&mut c, None, false, t),
        None => return,
    };
    if audio {
        let mut r = ui.new_child(UiBuilder::new().max_rect(row).layout(Layout::right_to_left(Align::Center)));
        widgets::chip_dark(&mut r, Some("volume_up"), false, "").on_hover_text(tr("ui.room.shares_audio", "Shares audio"));
    }
}

/// `vbox·info` (pad 12, gap 8): `[hbox·who 22: avatar 22, bold name, fill
/// dim title][hbox·specs 16][btn_row]`. `who` = (shown name, the display
/// name the avatar initial comes from, avatar colour); they differ only on
/// our own card ("You" / "Du" vs our name). Returns the clicked button.
fn card_info(ui: &mut Ui, info: Rect, who: (&str, &str, usize), title: &str, specs: &str, btns: &[Btn]) -> Option<usize> {
    let inner = info.shrink(theme::S3);
    let mut c = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::top_down(Align::Min)));
    c.spacing_mut().item_spacing.y = theme::S2;
    let nf = font(Weight::Bold, 13.5);
    let nw = text_width(&c, who.0, nf.clone(), 0.0).ceil().min(inner.width() * 0.6);
    let row = HBox::new("hbox·who", theme::AVATAR_SM).slots(&[fixed(theme::AVATAR_SM), fixed(nw), FILL]).show(&mut c);
    widgets::paint_avatar(&c, row.cell(0), &initial(who.1), who.2, false);
    row.text(&c, 1, who.0, nf, theme::TEXT, Align::Min);
    row.text(&c, 2, title, font(Weight::Regular, theme::FS_SMALL), theme::TEXT_DIM, Align::Min);
    let row = HBox::new("hbox·specs", 16.0).slots(&[FILL]).show(&mut c);
    row.text(&c, 0, specs, font(Weight::Mono, theme::FS_TINY), theme::TEXT_DIM, Align::Min);
    widgets::btn_row(&mut c, btns)
}

/// Someone else's share: preview thumbnail (latest decoded preview JPEG),
/// owner, specs, `[Watch][Pop out]` (audio-only: `[Listen]`).
fn share_card(ui: &mut Ui, cx: &Cx, h: RoomHandle, info: &StreamInfo, acts: &mut Vec<UiAction>) {
    let key = (h, info.owner, info.stream_id);
    let Some(s) = cx.session(h) else { return };
    let (thumb, info_r) = card_frame(ui);
    let p = ui.painter();
    p.rect_filled(thumb, thumb_radius(), theme::TILE_BG);
    let audio_only = info.kind == StreamKind::Audio;
    if let Some(tex) = s.preview_textures.get(&(info.owner, info.stream_id)) {
        // Cover-fit the preview into the thumb (crop, never letterbox).
        let [tw, th] = tex.size();
        let (ta, ra) = (tw as f32 / th.max(1) as f32, thumb.width() / thumb.height());
        let uv = if ta > ra {
            let k = ra / ta;
            Rect::from_min_max(pos2(0.5 - k / 2.0, 0.0), pos2(0.5 + k / 2.0, 1.0))
        } else {
            let k = ta / ra;
            Rect::from_min_max(pos2(0.0, 0.5 - k / 2.0), pos2(1.0, 0.5 + k / 2.0))
        };
        p.add(RectShape::filled(thumb, thumb_radius(), Color32::WHITE).with_texture(tex.id(), uv));
    } else {
        let (icon, label) = if audio_only {
            ("music_note", tr("ui.room.audio_only", "Audio only"))
        } else {
            ("screen_share", tr("ui.room.no_preview", "Preview arrives shortly"))
        };
        thumb_placeholder(ui, thumb, icon, theme::TEXT_FAINT, &label);
    }
    let has_audio = audio_only || s.sibling_audio_stream(info.owner).is_some();
    thumb_corners(ui, thumb, Some("LIVE"), has_audio && !audio_only);

    let me = cx.my_conn(h) == Some(info.owner);
    let name = cx.member_name(h, info.owner);
    let specs = if audio_only {
        tr("ui.room.audio_only", "Audio only")
    } else if info.width > 0 && info.height > 0 {
        format!("{}×{}", info.width, info.height)
    } else {
        "—".to_owned()
    };
    let who = (name.as_str(), name.as_str(), avatar_color(me, info.owner));
    if audio_only {
        let listening = cx.is_watching(key);
        let (l_on, l_off) = (tr("ui.room.listen", "Listen"), tr("ui.room.stop_listening", "Stop listening"));
        let b = if listening { Btn::secondary(&l_off).icon("volume_off") } else { Btn::primary(&l_on).icon("volume_up") };
        if card_info(ui, info_r, who, &info.label, &specs, &[b]).is_some() {
            acts.push(UiAction::ToggleListen(key));
        }
    } else {
        let watch = tr("ui.room.watch", "Watch");
        let pop = tr("ui.room.pop_out", "Pop out");
        let pop_tip = tr("ui.room.pop_out_card_tip", "Watch in its own window");
        let btns = [Btn::primary(&watch).icon("play_arrow"), Btn::secondary(&pop).icon("open_in_new").tip(&pop_tip)];
        match card_info(ui, info_r, who, &info.label, &specs, &btns) {
            Some(0) => acts.push(UiAction::Watch(key)),
            Some(1) => acts.push(UiAction::WatchPoppedOut(key)),
            _ => {}
        }
    }
}

/// A centred `[icon 24][label 11]` on a thumb.
fn thumb_placeholder(ui: &Ui, thumb: Rect, icon: &str, icon_col: Color32, label: &str) {
    let cy = thumb.center().y;
    let line = |y: f32, h: f32| Rect::from_x_y_ranges(thumb.x_range().shrink(theme::S2), y..=y + h);
    widgets::paint_icon(ui, line(cy - 22.0, 24.0), icon, 24.0, icon_col);
    paint_text(ui, line(cy + 6.0, 16.0), label, font(Weight::Regular, theme::FS_TINY), theme::TEXT_FAINT, Align::Center);
}

/// Our own share in this room: no preview (self-preview was removed on
/// purpose), screen-hatch thumb, specs of what we send, one full-width
/// "Stream settings" button.
fn my_card(ui: &mut Ui, cx: &Cx, h: RoomHandle, capture: CaptureSourceId, destination: DestinationId, acts: &mut Vec<UiAction>) {
    let Some(c) = cx.capture(capture) else { return };
    let Some(d) = c.destinations.get(&destination) else { return };
    let (thumb, info_r) = card_frame(ui);
    widgets::paint_hatch(ui.painter(), thumb, thumb_radius(), 8.0, theme::ELEV_19, theme::ELEV_14);
    thumb_placeholder(ui, thumb, "cast", theme::SUCCESS, &tr("ui.room.my_screen", "Your screen (no self-preview)"));
    let chip = match cx.destination_state(c, d) {
        DestinationState::Live | DestinationState::Rebuilding { .. } => "LIVE".to_owned(),
        DestinationState::Starting => tr("ui.room.chip_starting", "STARTING"),
        DestinationState::Stalled { .. } => tr("ui.room.chip_stalled", "STALLED"),
        DestinationState::Failed(_) => tr("ui.room.chip_failed", "FAILED"),
    };
    thumb_corners(ui, thumb, Some(&chip), false);
    if cx.destination_state(c, d) == DestinationState::Starting && cx.state.capture_awaiting_first_frame(capture) {
        ui.interact(thumb, Id::new(("my_card_thumb", capture, destination)), Sense::hover())
            .on_hover_text(tr("ui.waiting_frame_hint", "An idle screen sends no picture until something on it changes. Move the mouse over the shared screen and the stream starts."));
    }

    let params = match &d.runtime {
        Some(rt) => rt.lock().unwrap().params(),
        None => d.params,
    };
    let mut specs = String::new();
    if params.width > 0 && params.height > 0 {
        specs = format!("{}×{} · ", params.width, params.height);
    }
    specs.push_str(&format!("{} fps · {}", params.framerate, codec_label(params.codec)));
    let you = tr("ui.room.you", "You");
    // The avatar shows our name's initial, as our People row does (not the
    // "You" label's: German "Du" made it a "D").
    let my_name = cx.my_conn(h).map(|me| cx.member_name(h, me)).unwrap_or_else(|| cx.state.display_name.clone());
    let settings = tr("ui.room.stream_settings", "Stream settings");
    if card_info(ui, info_r, (&you, &my_name, 0), &c.label, &specs,&[Btn::secondary(&settings).icon("tune")]).is_some() {
        acts.push(UiAction::OpenModal(Modal::StreamSettings { capture, room: h }));
    }
}

// ---------------------------------------------------------------------------
// People
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
                let me = PersonRow { name: &cx.state.display_name, color: avatar_color(true, 0), you: Some(&you_tag), live: false, live_label: &live_l, watching: false, idle: true, stale: true };
                widgets::person_row(ui, &me).on_hover_text(&stale_tip);
                for m in &info.members {
                    let live = info.streams.iter().any(|i| i.owner == m.conn_id);
                    let row = PersonRow { name: &m.display_name, color: avatar_color(false, m.conn_id), you: None, live, live_label: &live_l, watching: false, idle: !live, stale: true };
                    widgets::person_row(ui, &row).on_hover_text(&stale_tip);
                }
            });
            return;
        }
        widgets::heading(ui, &tr("ui.room.people", "People"), None, false);
        // Wraps: the German line is wider than the 207 px column.
        let mut job = egui::text::LayoutJob::single_section(
            tr("ui.room.people_paused", "Back when the room reconnects"),
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
                    color: avatar_color(you, m.conn_id),
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
    fn single_tile_is_capped_by_window_height() {
        // 900 px window: 46 % × 16/9 ≈ 736 px wide.
        let w = single_tile_width(1300.0, 900.0);
        assert!((w - 900.0 * 0.46 * 16.0 / 9.0).abs() < 0.01, "{w}");
        // Narrow area: the area wins.
        assert_eq!(single_tile_width(500.0, 900.0), 500.0);
        // Tiny window: never below a grid column unless the area is smaller.
        assert_eq!(single_tile_width(1000.0, 100.0), MIN_TILE_W);
        assert_eq!(single_tile_width(200.0, 100.0), 200.0);
    }

    #[test]
    fn room_tiles_use_one_or_two_columns() {
        assert_eq!(room_tile_cols(1, 2000.0), 1);
        assert_eq!(room_tile_cols(2, 1000.0), 2);
        assert_eq!(room_tile_cols(5, 1000.0), 2);
        // Two columns would be under 260 each.
        assert_eq!(room_tile_cols(2, 2.0 * MIN_TILE_W + theme::GRID_GAP - 1.0), 1);
    }

    #[test]
    fn grid_shape_maximises_tile_area() {
        // One tile: as wide as fits the height at 16:9.
        let (c, w) = grid_shape(1, 1600.0, 450.0, 12.0);
        assert_eq!(c, 1);
        assert!((w - 800.0).abs() < 0.01);
        // Two tiles in a wide, short area: side by side.
        assert_eq!(grid_shape(2, 1600.0, 500.0, 12.0).0, 2);
        // Two tiles in a tall, narrow area: stacked.
        assert_eq!(grid_shape(2, 700.0, 1200.0, 12.0).0, 1);
        // Four tiles in a 16:9-ish area: 2×2.
        let (c, w) = grid_shape(4, 1600.0, 900.0, 12.0);
        assert_eq!(c, 2);
        assert!(w > 700.0 && w <= 794.0, "{w}");
        // Columns never get narrower than a grid column; no fit → scroll.
        let (c, w) = grid_shape(9, 800.0, 100.0, 12.0);
        assert!(w >= MIN_TILE_W && c >= 1, "{c} {w}");
    }

    #[test]
    fn avatar_colors_are_stable_and_ours_is_accent() {
        assert_eq!(avatar_color(true, 7), 0);
        for conn in 0..20u64 {
            let c = avatar_color(false, conn);
            assert!((1..=4).contains(&c));
            assert_eq!(c, avatar_color(false, conn));
        }
    }

    #[test]
    fn kbps_formatting() {
        assert_eq!(fmt_kbps(5500.0), "5.5 Mbps");
        assert_eq!(fmt_kbps(640.4), "640 kbps");
    }

    #[test]
    fn tr_falls_back_to_english() {
        assert_eq!(tr("ui.room.__never_a_key__", "Fallback"), "Fallback");
        assert_eq!(tr_with("ui.room.__never_a_key__", "%{a} and %{b}", &[("a", "1"), ("b", "2")]), "1 and 2");
    }
}
