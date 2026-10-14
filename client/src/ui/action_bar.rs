//! Bottom action bar (owner: I15), room-scoped, one layout per
//! [`ShareState`] of the selected room (`cx.share_state(&room)`). Every
//! layout is one [`HBox`] of 36 px controls (gap 8) in the padded 56 px band:
//!
//! - Idle: `[Share screen ▾][Share audio][fill status]`. The ▾ and
//!   [`PopoverKind::ShareMenu`] (add here / move here / share a different
//!   screen) only appear while another capture is live elsewhere; "Share
//!   audio" is disabled (audio rides along with a screen).
//! - Picking / Staged: `[84 pill][fill status][audio][Stream settings][sep][Discard][Go live]`
//!   (Picking disables audio, Stream settings and Go live).
//! - Live: `[84 pill][fill status][audio control][Stream settings][N rooms ▾][sep][Stop sharing]`
//!   (`N rooms ▾` = [`PopoverKind::Rooms`]: Also send to + Move to). While
//!   the pill says Starting only because the capture has no first frame yet
//!   (an idle monitor), the status reads "Waiting for the screen to change /
//!   Move the mouse over it to start" (+ tooltip) instead of screen · fps.
//! - Paused (room reconnecting): `[84 pill][fill status]`.
//!
//! "audio" is either a `Share audio ▾` button or, once a source is chosen, the
//! audio control `[36 mute][source name · meter · ▾]` (mockup `.audio-ctl`);
//! both open [`PopoverKind::AudioSource`], a per-app source list with live
//! level meters (taps run only while that popover is open).
//!
//! Leave is not here: it lives in the room header (user request).
//!
//! # Narrow windows
//! At the 960 px minimum window the bar is ~656 px wide, too narrow for every
//! label (German most of all). Each layout picks the lowest degradation
//! level ([`fit_level`]) that leaves the status fill [`STATUS_MIN_W`]:
//! first the status text shrinks (it truncates with "…"), then buttons
//! collapse to icon-only (the label moves into the tooltip) in this order:
//! - Live: Stream settings → `N rooms ▾` becomes `N ▾` → audio compact
//!   (icon-only `Share audio ▾`, or the source name capped shorter) → Stop
//!   sharing.
//! - Picking / Staged: Stream settings → audio compact → Discard → Go live.
//! - Idle: Share audio (disabled anyway); Share screen keeps its label.
//!
//! `Why` this order: the least used / most redundant control goes first
//! (Stream settings is also on the own share card), and the one action the
//! bar exists for (Stop sharing, Go live) keeps its words longest.
//!
//! The popover bodies for Also send to / Move to are shared with the
//! stream-settings modal (`pub(super)` helpers at the bottom), so the two
//! lists can't drift.

use egui::{Align, CornerRadius, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use simple_screensharing_client::audio::AudioSource;
use simple_screensharing_client::capture::destination::{DestinationState, EncoderLimited};
use simple_screensharing_client::protocol::StreamKind;
use simple_screensharing_client::rooms::BandwidthMode;
use simple_screensharing_client::session::{
    CaptureSourceId, Destination, DestinationId, LocalCapture, PendingRole, RoomHandle, ShareHandle, ShareStatus,
};

use super::{ConnState, Cx, Modal, PopoverKind, ShareState, UiAction, tr, tr_with};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, Btn, FILL, HBox, Pill, PillLead, PopItem, Slot, fixed, paint_icon, paint_text, text_width};

/// Popover width (mockup `.popup`: 340).
pub(super) const POPOVER_W: f32 = 340.0;

/// Narrowest the status fill gets before the bar starts collapsing buttons
/// (still room for "Screen…" / "H.264 · 6…"). `Why` this small: the status
/// repeats what the own share card shows, while a German "Teilen beenden"
/// at 960 px only keeps its label if the status gives up ~90 px first.
const STATUS_MIN_W: f32 = 72.0;

/// The bar's degradation level (see "Narrow windows" above): the lowest
/// `level` in `0..=max` whose fixed slots (`fixed(level)` = their summed
/// width; `n` fixed slots + the status fill = `n` gaps) leave the status fill
/// at least [`STATUS_MIN_W`]; `max` if none does.
fn fit_level(avail: f32, n: usize, max: usize, fixed: impl Fn(usize) -> f32) -> usize {
    (0..=max).find(|l| avail - fixed(*l) - theme::S2 * n as f32 >= STATUS_MIN_W).unwrap_or(max)
}

/// `b` icon-only when `on` (its label moves into the tooltip `tip`).
fn icon_only<'a>(b: Btn<'a>, on: bool, tip: &'a str) -> Btn<'a> {
    if on { Btn { label: "", ..b }.tip(tip) } else { b }
}

/// Action-bar UI state.
#[derive(Default)]
pub struct ActionBarState {
    /// Audio source ids the running level taps were started for (the
    /// [`PopoverKind::AudioSource`] popover is open). `None` = no taps.
    taps: Option<Vec<u32>>,
}

/// The bar for the selected room.
pub fn show(ui: &mut Ui, cx: &mut Cx, st: &mut ActionBarState) {
    let Some(room) = cx.selected_room() else { return };
    let state = cx.share_state(&room);
    // Popovers before the bar: a click outside one that lands on another bar
    // button closes it (ClosePopover) *before* that button's OpenPopover is
    // queued, so the new one isn't closed again (actions apply in order).
    if room.conn == ConnState::Joined
        && let Some(h) = room.handle
    {
        audio_popover(ui, cx, st);
        share_menu(ui, cx, h);
        if let ShareState::Live { capture, .. } = state {
            rooms_popover(ui, cx, capture, h);
        }
    }
    match (state, room.handle) {
        (ShareState::Paused, _) | (_, None) => paused(ui, room.conn == ConnState::Connecting),
        (ShareState::Idle, Some(h)) => idle(ui, cx, h),
        (ShareState::Picking { capture }, Some(h)) => staged(ui, cx, h, capture, true),
        (ShareState::Staged { capture }, Some(h)) => staged(ui, cx, h, capture, false),
        (ShareState::Live { capture, destination }, Some(h)) => live(ui, cx, h, capture, destination),
    }
    sync_level_taps(cx, st);
}

// ---------------------------------------------------------------------------
// Layouts
// ---------------------------------------------------------------------------

/// `connecting`: a first connect is in flight (nothing was ever shared here,
/// so "paused until the room is back" would be wrong).
fn paused(ui: &mut Ui, connecting: bool) {
    let row = HBox::new("action_bar", theme::CTL_H_LG).gap(theme::S2).slots(&[fixed(84.0), FILL]).show(ui);
    if connecting {
        let text = tr("ui.room.connecting_pill", "Connecting");
        paint_pill(ui, row.cell(0), Pill::progress(&text).large());
        status_text(ui, row.cell(1), &tr("ui.bar.connecting_status", "You can share once the room is connected"), None);
        return;
    }
    let text = tr("ui.bar.paused", "Paused");
    paint_pill(ui, row.cell(0), Pill::muted(&text).lead(PillLead::Icon("pause")).large());
    status_text(ui, row.cell(1), &tr("ui.bar.paused_status", "Sharing is paused until the room is back"), None);
}

fn idle(ui: &mut Ui, cx: &mut Cx, room: RoomHandle) {
    // Any destination elsewhere (even Starting) offers the add/move menu; the
    // "live in" line only counts rooms where the share is actually on air.
    let others: Vec<RoomHandle> = cx.live_rooms().into_iter().filter(|r| *r != room).collect();
    let on_air: Vec<RoomHandle> = cx.on_air_rooms().into_iter().filter(|r| *r != room).collect();
    let share_l = tr("ui.bar.share_screen", "Share screen");
    let audio_l = tr("ui.bar.share_audio", "Share audio");
    let audio_tip = tr("ui.bar.share_audio_first", "Share a screen first; audio rides along with it");
    let audio_tip_full = format!("{audio_l}\n{audio_tip}");
    let mut share = Btn::primary(&share_l).icon("screen_share").large();
    if !others.is_empty() {
        share = share.trailing("expand_more");
    }
    let audio_at = |l: usize| {
        let b = Btn::secondary(&audio_l).icon("volume_up").large().enabled(false).tip(&audio_tip);
        icon_only(b, l >= 1, &audio_tip_full)
    };
    let level = if on_air.is_empty() { 0 } else { fit_level(ui.available_width(), 2, 1, |l| share.width(ui) + audio_at(l).width(ui)) };
    let audio = audio_at(level);
    let slots = [fixed(share.width(ui)), fixed(audio.width(ui)), FILL];
    let row = HBox::new("action_bar", theme::CTL_H_LG).gap(theme::S2).slots(&slots).show(ui);
    let share_clicked = row.show(ui, 0, Align::Min, |ui| share.show_sized(ui, row.cell(0).width())).clicked();
    row.show(ui, 1, Align::Min, |ui| audio.show_sized(ui, row.cell(1).width()));
    if !on_air.is_empty() {
        let names: Vec<String> = on_air.iter().map(|r| cx.room_name(*r)).collect();
        let text = tr_with("ui.bar.live_elsewhere", "Your screen is live in %{rooms}", &[("rooms", &names.join(", "))]);
        let c = row.cell(2);
        paint_text(ui, Rect::from_x_y_ranges(c.left() + theme::S1..=c.right(), c.y_range()), &text, t2_font(false), theme::TEXT_DIM, Align::Min);
    }
    if share_clicked {
        if others.is_empty() {
            cx.act(UiAction::StageShare(room));
        } else {
            cx.toggle_popover(PopoverKind::ShareMenu { room }, row.cell(0));
        }
    }
}

fn staged(ui: &mut Ui, cx: &mut Cx, room: RoomHandle, capture: CaptureSourceId, picking: bool) {
    let Some(c) = cx.capture(capture) else { return };
    let staged_audio = cx.shell.staged.get(&room).and_then(|s| s.audio.clone());
    let settings_l = tr("ui.bar.stream_settings", "Stream settings");
    let discard_l = tr("ui.bar.discard", "Discard");
    let go_l = tr("ui.bar.go_live", "Go live");
    let staged_name = staged_audio.as_ref().map(|a| a.name.clone());
    // Degradation order: see "Narrow windows" in the module doc.
    let settings_at = |l: usize| icon_only(Btn::secondary(&settings_l).icon("tune").large().enabled(!picking), l >= 1, &settings_l);
    let audio_at = |l: usize| AudioSlot::new(ui, staged_name.clone(), false, None, !picking, l >= 2);
    let discard_at = |l: usize| if l >= 3 { Btn::secondary("").icon("close").large().tip(&discard_l) } else { Btn::secondary(&discard_l).large() };
    let go_at = |l: usize| icon_only(Btn::primary(&go_l).icon("cast").large().enabled(!picking), l >= 4, &go_l);
    let level = fit_level(ui.available_width(), 6, 4, |l| {
        84.0 + audio_at(l).width + settings_at(l).width(ui) + 1.0 + discard_at(l).width(ui) + go_at(l).width(ui)
    });
    let (settings, audio, discard, go) = (settings_at(level), audio_at(level), discard_at(level), go_at(level));
    let slots = [
        fixed(84.0),
        FILL,
        fixed(audio.width),
        fixed(settings.width(ui)),
        fixed(1.0),
        fixed(discard.width(ui)),
        fixed(go.width(ui)),
    ];
    let row = HBox::new("action_bar", theme::CTL_H_LG).gap(theme::S2).slots(&slots).show(ui);
    let (pill_t, t1, t2) = if picking {
        (
            tr("ui.bar.picking", "Picking"),
            tr("ui.bar.picking_title", "Choose a screen in the system picker"),
            tr("ui.bar.picking_sub", "Waiting for the screen picker"),
        )
    } else {
        (
            tr("ui.bar.ready", "Ready"),
            tr_with("ui.bar.not_live_yet", "%{screen} · not live yet", &[("screen", &screen_name(c))]),
            tr_with("ui.bar.goes_live_in", "Goes live in %{room}", &[("room", &cx.room_name(room))]),
        )
    };
    let pill = if picking { Pill::progress(&pill_t) } else { Pill::muted(&pill_t) };
    paint_pill(ui, row.cell(0), pill.large());
    status_text(ui, row.cell(1), &t1, Some((&t2, false)));
    let audio_out = audio.show(ui, row.cell(2));
    let settings_clicked = row.show(ui, 3, Align::Min, |ui| settings.show_sized(ui, row.cell(3).width())).clicked();
    paint_vsep(ui, row.cell(4));
    let discard_clicked = row.show(ui, 5, Align::Min, |ui| discard.show_sized(ui, row.cell(5).width())).clicked();
    let go_clicked = row.show(ui, 6, Align::Min, |ui| go.show_sized(ui, row.cell(6).width())).clicked();
    if audio_out.open {
        cx.toggle_popover(PopoverKind::AudioSource { capture }, audio_out.anchor);
    }
    if settings_clicked {
        cx.act(UiAction::OpenModal(Modal::StreamSettings { capture, room }));
    }
    if discard_clicked {
        cx.act(UiAction::Discard(capture));
    }
    if go_clicked {
        cx.act(UiAction::GoLive { capture, room });
    }
}

fn live(ui: &mut Ui, cx: &mut Cx, room: RoomHandle, capture: CaptureSourceId, destination: DestinationId) {
    let state = cx.state;
    let Some(c) = cx.capture(capture) else { return };
    let Some(d) = c.destinations.get(&destination) else { return };
    let n_rooms = c.destinations.len();
    let audio_cap = cx.paired_audio(capture);
    let (audio_name, muted, lit) = match audio_cap {
        Some((id, a)) => {
            let muted = state.is_audio_share_muted(id);
            let lit = if muted { 0 } else { state.audio_share_level(id).map(level_lit).unwrap_or(0) };
            (Some(a.label.clone()), muted, lit)
        }
        None => (None, false, 0),
    };
    let settings_l = tr("ui.bar.stream_settings", "Stream settings");
    let rooms_l = rooms_count(n_rooms);
    let rooms_tip = tr("ui.bar.rooms_tip", "Rooms this stream goes to");
    let stop_l = tr("ui.bar.stop_sharing", "Stop sharing");
    let rooms_n = n_rooms.to_string();
    let rooms_tip_full = format!("{rooms_l}\n{rooms_tip}");
    // Degradation order: see "Narrow windows" in the module doc.
    let settings_at = |l: usize| icon_only(Btn::secondary(&settings_l).icon("tune").large(), l >= 1, &settings_l);
    let rooms_at = |l: usize| {
        let (label, tip) = if l >= 2 { (&rooms_n, &rooms_tip_full) } else { (&rooms_l, &rooms_tip) };
        Btn::secondary(label).icon("cast").trailing("expand_more").large().tip(tip)
    };
    let audio_at = |l: usize| AudioSlot::new(ui, audio_name.clone(), muted, Some(lit), true, l >= 3);
    let stop_at = |l: usize| icon_only(Btn::danger_solid(&stop_l).icon("stop_screen_share").large(), l >= 4, &stop_l);
    let level = fit_level(ui.available_width(), 6, 4, |l| {
        84.0 + audio_at(l).width + settings_at(l).width(ui) + rooms_at(l).width(ui) + 1.0 + stop_at(l).width(ui)
    });
    let (settings, rooms, audio, stop) = (settings_at(level), rooms_at(level), audio_at(level), stop_at(level));
    let slots = [
        fixed(84.0),
        FILL,
        fixed(audio.width),
        fixed(settings.width(ui)),
        fixed(rooms.width(ui)),
        fixed(1.0),
        fixed(stop.width(ui)),
    ];
    let row = HBox::new("action_bar", theme::CTL_H_LG).gap(theme::S2).slots(&slots).show(ui);
    let (kind, text) = dest_pill(c, d);
    paint_pill(ui, row.cell(0), kind.pill(&text).large());
    let params = d.live_params();
    let kbps = dest_kbps(cx, destination, d);
    if awaiting_first_frame(cx, capture, c, d) {
        // An idle monitor sends no first frame until something on it repaints
        // (portal `copy_with_damage`); say so instead of a silent "Starting".
        let t1 = tr("ui.bar.waiting_title", "Waiting for the screen to change");
        let t2 = tr("ui.bar.waiting_sub", "Move the mouse over it to start");
        status_text(ui, row.cell(1), &t1, Some((&t2, false)));
        ui.interact(row.cell(1), ui.id().with("waiting_tip"), Sense::hover()).on_hover_text(waiting_hint());
        // The flag flips on the capture thread; poll so Live shows promptly.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    } else {
        let screen = screen_name(c);
        let fps = params.framerate.to_string();
        let limit = encoder_limited(d);
        // Encoder-bound (#46): the fps reads "168/240" in the warning colour,
        // and the tooltip says what that means.
        let t1 = match limit {
            Some(l) => tr_with(
                "ui.bar.live_title_limited",
                "%{screen} · %{n}/%{fps} fps",
                &[("screen", &screen), ("n", &l.encoding_fps.to_string()), ("fps", &fps)],
            ),
            None => tr_with("ui.bar.live_title", "%{screen} · %{fps} fps", &[("screen", &screen), ("fps", &fps)]),
        };
        let t2 = format!("{} · {}", simple_screensharing_client::presets::codec_label(params.codec), fmt_rate(kbps));
        let c1 = if limit.is_some() { theme::WARNING } else { theme::TEXT };
        status_text_colored(ui, row.cell(1), &t1, c1, Some((&t2, true)));
        let tip = match limit {
            Some(l) => format!("{t1}\n{t2}\n\n{}", encoder_limit_tip(l)),
            None => format!("{t1}\n{t2}"),
        };
        ui.interact(row.cell(1), ui.id().with("live_tip"), Sense::hover()).on_hover_text(tip);
    }
    // The encoder-limit verdict changes on the pump's clock; 1 Hz is plenty.
    ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
    let audio_out = audio.show(ui, row.cell(2));
    let settings_clicked = row.show(ui, 3, Align::Min, |ui| settings.show_sized(ui, row.cell(3).width())).clicked();
    let rooms_clicked = row.show(ui, 4, Align::Min, |ui| rooms.show_sized(ui, row.cell(4).width())).clicked();
    paint_vsep(ui, row.cell(5));
    let stop_clicked = row.show(ui, 6, Align::Min, |ui| stop.show_sized(ui, row.cell(6).width())).clicked();
    if audio_out.mute {
        cx.act(UiAction::SetSharedAudioMuted { capture, muted: !muted });
    }
    if audio_out.open {
        cx.toggle_popover(PopoverKind::AudioSource { capture }, audio_out.anchor);
    }
    if settings_clicked {
        cx.act(UiAction::OpenModal(Modal::StreamSettings { capture, room }));
    }
    if rooms_clicked {
        cx.toggle_popover(PopoverKind::Rooms { capture, room }, row.cell(4));
    }
    if stop_clicked {
        cx.act(UiAction::StopDestination(destination));
    }
    // Live meters in the bar need frames; ~10 Hz is what the lib updates at.
    if audio_cap.is_some() {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
}

// ---------------------------------------------------------------------------
// Audio control
// ---------------------------------------------------------------------------

/// The bar's audio slot: a `Share audio ▾` button when no source is chosen,
/// else the `.audio-ctl` compound `[36 mute | name · meter · ▾]`.
struct AudioSlot {
    name: Option<String>,
    muted: bool,
    /// Meter bars lit; `None` = no meter (a staged choice isn't capturing yet).
    lit: Option<u8>,
    enabled: bool,
    /// Narrow bar: icon-only `Share audio ▾`, shorter source name.
    compact: bool,
    /// The source name is cut short (its full form goes in the tooltip).
    truncated: bool,
    width: f32,
    label_w: f32,
}

struct AudioOut {
    mute: bool,
    open: bool,
    anchor: Rect,
}

const AUDIO_MUTE_W: f32 = 36.0;
const AUDIO_PAD: f32 = 10.0;
const AUDIO_NAME_MAX: f32 = 150.0;
/// [`AUDIO_NAME_MAX`] in a narrow bar.
const AUDIO_NAME_MAX_COMPACT: f32 = 64.0;

impl AudioSlot {
    fn new(ui: &Ui, name: Option<String>, muted: bool, lit: Option<u8>, enabled: bool, compact: bool) -> Self {
        let mut truncated = false;
        let (width, label_w) = match &name {
            None => {
                let l = tr("ui.bar.share_audio", "Share audio");
                let l = if compact { "" } else { l.as_str() };
                (Btn::secondary(l).icon("volume_up").trailing("expand_more").large().width(ui), 0.0)
            }
            Some(n) => {
                let full = text_width(ui, n, font(Weight::Bold, 12.5), 0.0).ceil();
                let lw = full.min(if compact { AUDIO_NAME_MAX_COMPACT } else { AUDIO_NAME_MAX });
                truncated = lw < full;
                let meter_w = if lit.is_some() { theme::S2 + 23.0 } else { 0.0 };
                (AUDIO_MUTE_W + 1.0 + AUDIO_PAD + lw + meter_w + theme::S2 + theme::ICON_BTN + AUDIO_PAD, lw)
            }
        };
        Self { name, muted, lit, enabled, compact, truncated, width, label_w }
    }

    fn show(&self, ui: &mut Ui, cell: Rect) -> AudioOut {
        let Some(name) = &self.name else {
            let l = tr("ui.bar.share_audio", "Share audio");
            let b = Btn::secondary(&l).icon("volume_up").trailing("expand_more").large().enabled(self.enabled);
            let b = icon_only(b, self.compact, &l);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(cell));
            let open = b.show_sized(&mut child, cell.width()).clicked();
            return AudioOut { mute: false, open, anchor: cell };
        };
        let rect = Rect::from_center_size(cell.center(), vec2(cell.width(), theme::CTL_H_LG));
        let r = CornerRadius::same(theme::ROUNDING_MD);
        let p = ui.painter();
        p.rect(rect, r, theme::BTN, Stroke::new(1.0_f32, theme::BTN_BORDER), StrokeKind::Inside);
        let mute_r = Rect::from_min_size(rect.min, vec2(AUDIO_MUTE_W, rect.height()));
        let src_r = Rect::from_min_max(pos2(mute_r.right() + 1.0, rect.top()), rect.max);
        let id = ui.id().with("audio_ctl");
        let mute_sense = if self.lit.is_some() && self.enabled { Sense::click() } else { Sense::hover() };
        let mute = ui.interact(mute_r, id.with("mute"), mute_sense);
        let src = ui.interact(src_r, id.with("src"), if self.enabled { Sense::click() } else { Sense::hover() });
        let clip = ui.painter().with_clip_rect(rect.shrink(1.0).intersect(ui.clip_rect()));
        if mute.hovered() && self.lit.is_some() {
            clip.rect_filled(mute_r, CornerRadius { nw: theme::ROUNDING_MD, sw: theme::ROUNDING_MD, ne: 0, se: 0 }, theme::WASH_HOVER_1);
        }
        if src.hovered() && self.enabled {
            clip.rect_filled(src_r, CornerRadius { nw: 0, sw: 0, ne: theme::ROUNDING_MD, se: theme::ROUNDING_MD }, theme::WASH_HOVER_1);
        }
        ui.painter().vline(mute_r.right() + 0.5, rect.y_range().shrink(1.0), Stroke::new(1.0_f32, theme::BTN_BORDER));
        let (icon, col) = if self.muted { ("volume_off", theme::DANGER) } else { ("volume_up", theme::TEXT) };
        let fade = |c| widgets::fade(c, self.enabled);
        paint_icon(ui, mute_r, icon, theme::ICON_BTN, fade(col));
        let mut x = src_r.left() + AUDIO_PAD;
        let lr = Rect::from_x_y_ranges(x..=x + self.label_w, rect.y_range());
        paint_text(ui, lr, name, font(Weight::Bold, 12.5), fade(theme::TEXT), Align::Min);
        x += self.label_w;
        if let Some(lit) = self.lit {
            x += theme::S2;
            let mr = Rect::from_min_size(pos2(x, rect.center().y - 7.0), vec2(23.0, 14.0));
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(mr));
            widgets::meter(&mut child, lit);
            x += 23.0;
        }
        x += theme::S2;
        paint_icon(ui, Rect::from_x_y_ranges(x..=x + theme::ICON_BTN, rect.y_range()), "expand_more", theme::ICON_BTN, fade(theme::TEXT));
        let mute_tip = if self.muted {
            tr("ui.bar.unmute_audio", "Unmute shared audio")
        } else {
            tr("ui.bar.mute_audio", "Mute shared audio")
        };
        let mute = mute.on_hover_text(mute_tip);
        let change = tr("ui.bar.change_audio", "Change audio source");
        let src = src.on_hover_text(if self.truncated { format!("{name}\n{change}") } else { change });
        AudioOut {
            mute: mute.clicked(),
            open: src.clicked(),
            anchor: rect,
        }
    }
}

/// The audio-source popover ([`PopoverKind::AudioSource`]): every app
/// producing audio, with a live meter each; the current one is highlighted.
fn audio_popover(ui: &mut Ui, cx: &mut Cx, _st: &mut ActionBarState) {
    let Some(pop) = cx.nav.popover.clone().filter(|p| matches!(p.kind, PopoverKind::AudioSource { .. })) else { return };
    let PopoverKind::AudioSource { capture } = pop.kind else { return };
    let state = cx.state;
    let staged_room = cx.shell.staged.iter().find(|(_, s)| s.capture == capture).map(|(r, _)| *r);
    let current: Option<String> = match staged_room {
        Some(r) => cx.shell.staged.get(&r).and_then(|s| s.audio.as_ref().map(|a| a.name.clone())),
        None => cx.paired_audio(capture).map(|(_, a)| a.label.clone()),
    };
    let sources = state.audio_sources.clone();
    let mut picked: Option<UiAction> = None;
    let heading = tr("ui.audio.heading", "Audio source");
    let empty = tr("ui.audio.none", "No app is playing audio");
    let refresh = tr("ui.audio.refresh", "Refresh");
    let stop = tr("ui.audio.stop", "Stop sharing audio");
    let levels: Vec<u8> = sources.iter().map(|s| cx.source_level(s.id).map(level_lit).unwrap_or(0)).collect();
    let ctx = ui.ctx().clone();
    let open = super::popover(&ctx, &pop, POPOVER_W, |ui| {
        pop_heading(ui, &heading);
        if sources.is_empty() {
            let msg = state.audio_sources_error.clone().unwrap_or(empty.clone());
            pop_note(ui, &msg);
            if widgets::pop_item(ui, &PopItem::new("refresh", &refresh)).clicked() {
                picked = Some(UiAction::RefreshAudioSources);
            }
        }
        for (s, lit) in sources.iter().zip(&levels) {
            let sel = current.as_deref() == Some(s.name.as_str());
            if widgets::pop_item(ui, &PopItem::new("music_note", &s.name).selected(sel).meter(*lit)).clicked() {
                picked = Some(UiAction::ShareAudio { capture, source: s.clone() });
            }
        }
        if current.is_some() {
            pop_sep(ui);
            if widgets::pop_item(ui, &PopItem::new("volume_off", &stop).danger()).clicked() {
                picked = Some(UiAction::StopSharingAudio { capture });
            }
        }
    });
    ctx.request_repaint_after(std::time::Duration::from_millis(100));
    if let Some(a) = picked {
        let keep_open = matches!(a, UiAction::RefreshAudioSources);
        cx.act(a);
        if !keep_open {
            cx.act(UiAction::ClosePopover);
        }
    } else if !open {
        cx.act(UiAction::ClosePopover);
    }
}

/// Start level taps when the audio popover opens (and refresh the source
/// list), restart them when the list changes, stop them when it closes.
fn sync_level_taps(cx: &mut Cx, st: &mut ActionBarState) {
    let open = cx.nav.popover.as_ref().is_some_and(|p| matches!(p.kind, PopoverKind::AudioSource { .. }));
    let ids: Vec<u32> = cx.state.audio_sources.iter().map(|s| s.id).collect();
    match (&st.taps, open) {
        (None, true) => {
            cx.act(UiAction::RefreshAudioSources);
            cx.act(UiAction::StartLevelTaps(ids.clone()));
            st.taps = Some(ids);
        }
        (Some(running), true) if *running != ids => {
            cx.act(UiAction::StartLevelTaps(ids.clone()));
            st.taps = Some(ids);
        }
        (Some(_), false) => {
            cx.act(UiAction::StopLevelTaps);
            st.taps = None;
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Share menu and rooms popovers
// ---------------------------------------------------------------------------

/// Idle "Share screen ▾" while live elsewhere: per live capture, add it here
/// / move it here; then "Share a different screen" (the OS picker).
fn share_menu(ui: &mut Ui, cx: &mut Cx, room: RoomHandle) {
    let Some(pop) = cx.popover_of(&PopoverKind::ShareMenu { room }).cloned() else { return };
    let captures = cx.live_captures();
    let room_name = cx.room_name(room);
    let heading = tr_with("ui.share_menu.heading", "Share in %{room}", &[("room", &room_name)]);
    let items: Vec<(CaptureSourceId, String, String, String)> = captures
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
    let add_sub = tr("ui.share_menu.add_sub", "Same capture, one more room");
    let other = tr("ui.share_menu.other", "Share a different screen");
    let other_sub = tr("ui.share_menu.other_sub", "Opens the system picker");
    let mut picked = None;
    let ctx = ui.ctx().clone();
    let open = super::popover(&ctx, &pop, POPOVER_W, |ui| {
        pop_heading(ui, &heading);
        for (capture, add, mv, mv_sub) in &items {
            if widgets::pop_item(ui, &PopItem::new("add_to_queue", add).sub(&add_sub)).clicked() {
                picked = Some(UiAction::AddDestination { capture: *capture, room });
            }
            if widgets::pop_item(ui, &PopItem::new("swap_horiz", mv).sub(mv_sub)).clicked() {
                picked = Some(UiAction::MoveCaptureHere { capture: *capture, room });
            }
        }
        if !items.is_empty() {
            pop_sep(ui);
        }
        if widgets::pop_item(ui, &PopItem::new("screen_share", &other).sub(&other_sub)).clicked() {
            picked = Some(UiAction::StageShare(room));
        }
    });
    if let Some(a) = picked {
        cx.act(a);
        cx.act(UiAction::ClosePopover);
    } else if !open {
        cx.act(UiAction::ClosePopover);
    }
}

/// Live "N rooms ▾": Also send to (checkbox per joined room) + Move to.
fn rooms_popover(ui: &mut Ui, cx: &mut Cx, capture: CaptureSourceId, room: RoomHandle) {
    let Some(pop) = cx.popover_of(&PopoverKind::Rooms { capture, room }).cloned() else { return };
    let ctx = ui.ctx().clone();
    let also = tr("ui.rooms.also_send_to", "Also send to");
    let move_to = tr("ui.rooms.move_to", "Move to");
    let targets = RoomTargets::of(cx, capture, room);
    let mut picked = None;
    let open = super::popover(&ctx, &pop, POPOVER_W, |ui| {
        pop_heading(ui, &also);
        targets.also_items(ui, &mut picked);
        pop_sep(ui);
        pop_heading(ui, &move_to);
        targets.move_items(ui, &mut picked);
    });
    targets.finish(cx, picked, open);
}

/// The other joined rooms a capture could go to, for the Also send to / Move
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

impl RoomTargets {
    pub(super) fn of(cx: &Cx, capture: CaptureSourceId, room: RoomHandle) -> Self {
        let c = cx.capture(capture);
        let dest_in = |r: RoomHandle| c.and_then(|c| c.destinations.iter().find(|(_, d)| d.room == r).map(|(id, _)| *id));
        let rooms = cx
            .rooms()
            .into_iter()
            .filter(|e| e.conn == ConnState::Joined)
            .filter_map(|e| e.handle.map(|h| (h, e.name)))
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

/// `true` while `d` is still Starting because its capture has not produced a
/// first frame yet: an idle monitor (the portal only sends on repaint) until
/// something on it changes. See `AppState::capture_awaiting_first_frame`.
pub(super) fn awaiting_first_frame(cx: &Cx, capture: CaptureSourceId, c: &LocalCapture, d: &Destination) -> bool {
    matches!(c.destination_state(d), DestinationState::Starting) && cx.state.capture_awaiting_first_frame(capture)
}

/// `Some` while `d`'s encoder has been the bottleneck for a while: it
/// encodes fewer frames than it is offered (see
/// `capture::destination::EncoderLimit`, #46).
pub(super) fn encoder_limited(d: &Destination) -> Option<EncoderLimited> {
    d.runtime.as_ref().and_then(|r| r.lock().ok()?.encoder_limited())
}

/// Bar tooltip for an encoder-bound destination: "Encoding N of M fps" and
/// what to do about it.
fn encoder_limit_tip(l: EncoderLimited) -> String {
    let head = tr_with(
        "ui.bar.encoding_n_of_m",
        "Encoding %{n} of %{fps} fps",
        &[("n", &l.encoding_fps.to_string()), ("fps", &l.target_fps.to_string())],
    );
    let why = tr("ui.bar.encoder_limit_why", "Your encoder can't keep up with this framerate. Stream settings shows what helps.");
    format!("{head}\n{why}")
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

/// A pill left-aligned and vertically centred in `cell`.
fn paint_pill(ui: &Ui, cell: Rect, pill: Pill) {
    let h = if pill.large { theme::PILL_H_LG } else { theme::PILL_H };
    let w = pill.width(ui).min(cell.width());
    pill.paint(ui, Rect::from_min_size(pos2(cell.left(), cell.center().y - h / 2.0), vec2(w, h)));
}

fn t2_font(mono: bool) -> egui::FontId {
    if mono { font(Weight::Mono, theme::FS_TINY) } else { font(Weight::Regular, theme::FS_TINY) }
}

/// The bar's `vbox·status-text`: bold 12.5/16 line + dim 11/14 line (mono
/// when it is a rate), centred as a pair in `cell`.
fn status_text(ui: &Ui, cell: Rect, t1: &str, t2: Option<(&str, bool)>) {
    status_text_colored(ui, cell, t1, theme::TEXT, t2);
}

/// [`status_text`] with the first line in `c1`.
fn status_text_colored(ui: &Ui, cell: Rect, t1: &str, c1: egui::Color32, t2: Option<(&str, bool)>) {
    let f1 = font(Weight::Bold, 12.5);
    match t2 {
        None => {
            paint_text(ui, cell, t1, f1, c1, Align::Min);
        }
        Some((t2, mono)) => {
            let top = cell.center().y - 15.0;
            paint_text(ui, Rect::from_x_y_ranges(cell.x_range(), top..=top + 16.0), t1, f1, c1, Align::Min);
            paint_text(ui, Rect::from_x_y_ranges(cell.x_range(), top + 16.0..=top + 30.0), t2, t2_font(mono), theme::TEXT_DIM, Align::Min);
        }
    }
}

/// The 1×28 `.vsep` centred in a 1 px slot.
fn paint_vsep(ui: &Ui, cell: Rect) {
    let r = Rect::from_center_size(cell.center(), vec2(1.0, 28.0));
    ui.painter().rect_filled(r, 0.0, theme::DIVIDER);
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

/// An audio source by display name from the current listing.
pub(super) fn find_source(cx: &Cx, name: &str) -> Option<AudioSource> {
    cx.state.audio_sources.iter().find(|s| s.name == name).cloned()
}
