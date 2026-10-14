//! The mockup's row patterns. Each row *type* is exactly one function here, so two
//! rows of a type can never differ: same height, same slots, same x-positions.
//!
//! | helper            | height | slots (gap 8)                                              |
//! |-------------------|--------|------------------------------------------------------------|
//! | [`ctrl_row`]      | 32     | `[120 label][fill control]`                                |
//! | [`ctrl_row_value`]| 32     | `[120 label][fill control][72 value]`                      |
//! | [`room_row`]      | 32     | pad 8 · `[18 lead][fill name][18 live?][28 num][22 unit]`  |
//! | [`sharer_row`]    | 24     | pad 8 · `[18 indent][18 icon][fill name][28 num][22 unit]` |
//! | [`person_row`]    | 32     | pad 4 · `[22 avatar][fill name][44 pill][18 eye]`          |
//! | [`dest_row`]      | 40     | pad 12/4 · `[18 icon][fill name][84 pill][76 kbps][32 ×]`  |
//! | [`pop_item`]      | 32/44  | pad 8 · `[18 icon][fill label/sub][23 meter?]`             |
//! | [`btn_row`]       | 32/36  | `[fill btn][fill btn]…`                                    |

use egui::{Align, Color32, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::{
    Btn, DotState, FILL, HBox, Pill, Slot, fixed, num_box, paint_avatar, paint_galley, paint_icon, paint_status_dot, paint_text, slider,
    text_galley,
};
use crate::theme::{self, Weight, font};

/// Color role of a row icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconTone {
    /// `TEXT_DIM` (default `.ico`).
    Dim,
    /// `TEXT_FAINT` (`.unit-ico`).
    Faint,
    /// `SUCCESS` (`.ico.live` — your own live share).
    Live,
    /// `ACCENT_LIGHTER` (`.ico.watch` — a share you're watching).
    Watch,
    /// `DANGER`.
    Danger,
}

impl IconTone {
    pub fn color(self) -> Color32 {
        match self {
            IconTone::Dim => theme::TEXT_DIM,
            IconTone::Faint => theme::TEXT_FAINT,
            IconTone::Live => theme::SUCCESS,
            IconTone::Watch => theme::ACCENT_LIGHTER,
            IconTone::Danger => theme::DANGER,
        }
    }
}

// ---------------------------------------------------------------------------
// Control rows
// ---------------------------------------------------------------------------

const CTRL_SLOTS: [Slot; 2] = [fixed(theme::LABEL_W), FILL];
const CTRL_SLOTS_V: [Slot; 3] = [fixed(theme::LABEL_W), FILL, fixed(theme::VALUE_W)];

fn ctrl_label(ui: &mut Ui, rect: Rect, label: &str, tip: Option<&str>) {
    if label.is_empty() {
        return;
    }
    let f = font(Weight::Regular, theme::FS_LABEL);
    let tip_w = if tip.is_some() { 6.0 + 15.0 } else { 0.0 };
    // A label (with its ⓘ) may run into the 8 px gap before the control, minus 2 px of
    // air — "Keyframe interval ⓘ" then fits the 120 slot without truncating.
    let rect = Rect::from_min_max(rect.min, pos2(rect.right() + theme::S2 - 2.0, rect.bottom()));
    // A label too long for itself plus the ⓘ (German "Keyframe-Abstand") drops
    // the ⓘ and carries the tooltip itself, instead of truncating the label.
    let full_w = super::text_width(ui, label, f.clone(), 0.0);
    let (tip_w, tip_on_label) = if tip.is_some() && full_w + tip_w > rect.width() { (0.0, true) } else { (tip_w, false) };
    let g = text_galley(ui, label, f, theme::TEXT, (rect.width() - tip_w).max(0.0), 0.0);
    let truncated = g.size().x + 0.5 < full_w;
    let gr = paint_galley(ui, rect, g, Align::Min);
    if tip_on_label || truncated {
        let hover = match tip {
            Some(t) if truncated => format!("{label}\n{t}"),
            Some(t) => t.to_owned(),
            None => label.to_owned(),
        };
        ui.interact(gr, ui.id().with(("label_tip", gr.min.x as i32, gr.min.y as i32)), Sense::hover()).on_hover_text(hover);
    } else if let Some(t) = tip {
        let r = Rect::from_center_size(pos2(gr.right() + 6.0 + 7.5, rect.center().y), vec2(15.0, 15.0));
        paint_icon(ui, r, "info", 15.0, theme::TEXT_FAINT);
        ui.interact(r, ui.id().with(("tip", r.min.x as i32, r.min.y as i32)), Sense::hover()).on_hover_text(t);
    }
}

/// Control row `[120 label][fill control]`, 32 px. `label` may be `""` (empty label
/// slot, e.g. the "Mute shared audio" checkbox under "Audio"); `tip` adds the faint ⓘ
/// with a tooltip (no helper text under options). `control` runs in the fill cell —
/// fill-type controls (segmented, slider, select, field) take its full width.
pub fn ctrl_row<R>(ui: &mut Ui, label: &str, tip: Option<&str>, control: impl FnOnce(&mut Ui) -> R) -> R {
    let row = HBox::new("ctrl_row", theme::CTL_H).slots(&CTRL_SLOTS).show(ui);
    ctrl_label(ui, row.cell(0), label, tip);
    row.show(ui, 1, Align::Min, control)
}

/// Control row with a right-bound value slot: `[120 label][fill control][72 value]`.
/// The value slot is always reserved (pass `|_| ()` to leave it empty), so the control
/// edge lines up with every other value row.
pub fn ctrl_row_value<R, V>(
    ui: &mut Ui,
    label: &str,
    tip: Option<&str>,
    control: impl FnOnce(&mut Ui) -> R,
    value: impl FnOnce(&mut Ui) -> V,
) -> (R, V) {
    let row = HBox::new("ctrl_row", theme::CTL_H).slots(&CTRL_SLOTS_V).show(ui);
    ctrl_label(ui, row.cell(0), label, tip);
    let r = row.show(ui, 1, Align::Min, control);
    let v = row.show(ui, 2, Align::Max, value);
    (r, v)
}

/// Slider row: `[120 label][fill slider][72 editable value box]`. Returns the combined
/// response (`.changed()` when either control edited `*value`).
#[allow(clippy::too_many_arguments)]
pub fn slider_row<N: egui::emath::Numeric>(
    ui: &mut Ui,
    label: &str,
    tip: Option<&str>,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
    step: f64,
    decimals: usize,
    suffix: &str,
) -> Response {
    let row = HBox::new("ctrl_row", theme::CTL_H).slots(&CTRL_SLOTS_V).show(ui);
    ctrl_label(ui, row.cell(0), label, tip);
    let s = row.show(ui, 1, Align::Min, |ui| slider(ui, value, range.clone(), step));
    let v = row.show(ui, 2, Align::Max, |ui| num_box(ui, value, range, step, decimals, suffix));
    s | v
}

// ---------------------------------------------------------------------------
// Sidebar rows
// ---------------------------------------------------------------------------

/// Leading slot of a [`room_row`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum RoomLead<'a> {
    Dot(DotState),
    Icon(&'a str),
}

/// The "you are live here" slot of a [`room_row`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LiveSlot {
    /// No slot at all (the "Watched streams" row): `[18][fill][28][22]`.
    Absent,
    /// Slot reserved, empty.
    Off,
    /// Green cast icon.
    On,
}

/// The unit slot after a count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit<'a> {
    None,
    /// 15 px faint icon (`.unit-ico`).
    Icon(&'a str),
    /// Mono 10.5 faint text (`.unit`), e.g. "fps".
    Text(&'a str),
}

/// Data for [`room_row`].
#[derive(Clone, Copy, Debug)]
pub struct RoomRow<'a> {
    pub lead: RoomLead<'a>,
    pub name: &'a str,
    pub live: LiveSlot,
    /// Count shown right-aligned in the 28 slot (`None` = empty).
    pub num: Option<u32>,
    pub unit: Unit<'a>,
    pub selected: bool,
}

fn row_bg(ui: &Ui, rect: Rect, resp: &Response, selected: bool, radius: u8, hover_fill: Color32) {
    let r = CornerRadius::same(radius);
    if selected {
        ui.painter().rect(rect, r, theme::ACCENT_ROW_SELECTED, Stroke::new(1.0_f32, theme::ACCENT_RING), StrokeKind::Inside);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, r, hover_fill);
    }
}

fn paint_num(ui: &Ui, rect: Rect, num: Option<u32>) {
    if let Some(n) = num {
        paint_text(ui, rect, &n.to_string(), font(Weight::Mono, theme::FS_TINY), theme::TEXT_DIM, Align::Max);
    }
}

fn paint_unit(ui: &Ui, rect: Rect, unit: Unit) {
    match unit {
        Unit::None => {}
        Unit::Icon(i) => paint_icon(ui, Rect::from_min_size(rect.min, vec2(15.0, rect.height())), i, 15.0, theme::TEXT_FAINT),
        Unit::Text(t) => {
            paint_text(ui, rect, t, font(Weight::Mono, theme::FS_UNIT), theme::TEXT_FAINT, Align::Min);
        }
    }
}

/// Sidebar room row (32 px, clickable): status dot or icon, bold name, optional live
/// cast icon, share count + unit. Returns the row's click response.
pub fn room_row(ui: &mut Ui, row: &RoomRow) -> Response {
    const WITH_LIVE: [Slot; 5] = [fixed(18.0), FILL, fixed(18.0), fixed(28.0), fixed(22.0)];
    const NO_LIVE: [Slot; 4] = [fixed(18.0), FILL, fixed(28.0), fixed(22.0)];
    let slots: &[Slot] = if row.live == LiveSlot::Absent { &NO_LIVE } else { &WITH_LIVE };
    let r = HBox::new("room_row", theme::ROW_H).pad(theme::S2, theme::S2).slots(slots).sense(Sense::click()).show(ui);
    row_bg(ui, r.rect, &r.response, row.selected, theme::ROUNDING_SM, theme::ELEV_1B);
    match row.lead {
        RoomLead::Dot(s) => paint_status_dot(ui, r.cell(0).center(), s),
        RoomLead::Icon(i) => r.icon(ui, 0, i, 16.0, theme::TEXT_DIM),
    }
    r.text(ui, 1, row.name, font(Weight::Bold, theme::FS_ROW), theme::TEXT, Align::Min);
    let mut i = 2;
    if row.live != LiveSlot::Absent {
        if row.live == LiveSlot::On {
            r.icon(ui, 2, "cast", 16.0, theme::SUCCESS);
        }
        i = 3;
    }
    paint_num(ui, r.cell(i), row.num);
    paint_unit(ui, r.cell(i + 1), row.unit);
    r.response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Data for [`sharer_row`].
#[derive(Clone, Copy, Debug)]
pub struct SharerRow<'a> {
    pub icon: &'a str,
    pub tone: IconTone,
    pub name: &'a str,
    pub num: Option<u32>,
    pub unit: &'a str,
}

/// Sharer line under a room (24 px, clickable): indent, icon, name, fps + unit.
pub fn sharer_row(ui: &mut Ui, row: &SharerRow) -> Response {
    const SLOTS: [Slot; 5] = [fixed(18.0), fixed(18.0), FILL, fixed(28.0), fixed(22.0)];
    let r = HBox::new("sharer_row", theme::SUBROW_H).pad(theme::S2, theme::S2).slots(&SLOTS).sense(Sense::click()).show(ui);
    row_bg(ui, r.rect, &r.response, false, theme::ROUNDING_XS, theme::ELEV_1B);
    let name_col = if r.response.hovered() { theme::TEXT } else { theme::TEXT_DIM };
    r.icon(ui, 1, row.icon, 14.0, row.tone.color());
    r.text(ui, 2, row.name, font(Weight::Regular, theme::FS_SMALL), name_col, Align::Min);
    paint_num(ui, r.cell(3), row.num);
    paint_unit(ui, r.cell(4), if row.unit.is_empty() { Unit::None } else { Unit::Text(row.unit) });
    r.response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Data for [`person_row`].
#[derive(Clone, Copy, Debug)]
pub struct PersonRow<'a> {
    pub name: &'a str,
    /// Avatar gradient index (`theme::AVATAR_GRADIENTS`).
    pub color: usize,
    /// The faint "you" tag after the name (the caller's translation), or `None`.
    pub you: Option<&'a str>,
    /// Sharing: green "LIVE" pill in the 44 slot.
    pub live: bool,
    /// The live pill's text (the caller's translation of "Live").
    pub live_label: &'a str,
    /// You are watching their share: eye icon in the 18 slot.
    pub watching: bool,
    /// Not sharing: name dims.
    pub idle: bool,
    /// Last-known roster while reconnecting: whole row at 55 % opacity.
    pub stale: bool,
}

/// People-panel row (32 px): small avatar, name (+ "you"), live pill, watching eye.
pub fn person_row(ui: &mut Ui, row: &PersonRow) -> Response {
    const SLOTS: [Slot; 4] = [fixed(theme::AVATAR_SM), FILL, fixed(44.0), fixed(18.0)];
    ui.scope(|ui| {
        if row.stale {
            ui.multiply_opacity(0.55);
        }
        let r = HBox::new("person_row", theme::ROW_H).pad(theme::S1, theme::S1).slots(&SLOTS).show(ui);
        row_bg(ui, r.rect, &r.response, false, theme::ROUNDING_SM, theme::WASH_HOVER_1);
        let initial: String = row.name.chars().next().map(|c| c.to_string()).unwrap_or_default();
        let av = Rect::from_center_size(r.cell(0).center(), vec2(theme::AVATAR_SM, theme::AVATAR_SM));
        paint_avatar(ui, av, &initial, row.color, false);
        let name_col = if row.idle { theme::TEXT_DIM } else { theme::TEXT };
        let cell = r.cell(1);
        let you_w = row.you.map_or(0.0, |y| 6.0 + super::text_width(ui, y, font(Weight::Regular, theme::FS_TINY), 0.0));
        let g = text_galley(ui, row.name, font(Weight::Regular, theme::FS_ROW), name_col, (cell.width() - you_w).max(0.0), 0.0);
        let gr = paint_galley(ui, cell, g, Align::Min);
        if let Some(y) = row.you {
            let yr = Rect::from_x_y_ranges(gr.right() + 6.0..=cell.right(), cell.y_range());
            paint_text(ui, yr, y, font(Weight::Regular, theme::FS_TINY), theme::TEXT_FAINT, Align::Min);
        }
        if row.live {
            let pill = Pill::live(row.live_label).lead(super::PillLead::None);
            let w = pill.width(ui).min(r.cell(2).width());
            let c = r.cell(2);
            pill.paint(ui, Rect::from_min_size(pos2(c.right() - w, c.center().y - theme::PILL_H / 2.0), vec2(w, theme::PILL_H)));
        }
        if row.watching {
            r.icon(ui, 3, "visibility", 16.0, theme::ACCENT_LIGHTER);
        }
        r.response
    })
    .inner
}

// ---------------------------------------------------------------------------
// Destination row
// ---------------------------------------------------------------------------

/// Data for [`dest_row`].
#[derive(Clone, Copy, Debug)]
pub struct DestRow<'a> {
    /// This room (accent chip look, radio icon, `note`).
    pub current: bool,
    pub name: &'a str,
    /// Faint note after the name of the current row (the caller's "this room").
    pub note: &'a str,
    /// Tooltip of the × (the caller's "Stop sending to <room>").
    pub remove_tip: &'a str,
    pub pill: Pill<'a>,
    /// Pre-formatted, e.g. "6000 kbps" or "—".
    pub kbps: &'a str,
    /// Show the × "stop sending" button.
    pub removable: bool,
}

/// What happened in a [`dest_row`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DestAction {
    None,
    /// The × was clicked.
    Remove,
}

/// Destination row (40 px) in the "Sending to" card.
pub fn dest_row(ui: &mut Ui, row: &DestRow) -> DestAction {
    const SLOTS: [Slot; 5] = [fixed(18.0), FILL, fixed(84.0), fixed(76.0), fixed(32.0)];
    let r = HBox::new("dest_row", theme::DEST_ROW_H).pad(theme::S3, theme::S1).slots(&SLOTS).show(ui);
    let (bg, border, icon, icon_col) = if row.current {
        (theme::ACCENT_CHIP_BG, theme::ACCENT_CHIP_BORDER, "radio_button_checked", theme::ACCENT_LIGHTER)
    } else {
        (theme::PANEL_SUNKEN, theme::INSET_BORDER, "meeting_room", theme::TEXT_DIM)
    };
    ui.painter().rect(r.rect, CornerRadius::same(theme::ROUNDING_SM), bg, Stroke::new(1.0_f32, border), StrokeKind::Inside);
    r.icon(ui, 0, icon, 16.0, icon_col);
    let cell = r.cell(1);
    let note_w = if row.current { 6.0 + super::text_width(ui, row.note, font(Weight::Regular, theme::FS_TINY), 0.0) } else { 0.0 };
    let g = text_galley(ui, row.name, font(Weight::Bold, theme::FS_LABEL), theme::TEXT, (cell.width() - note_w).max(0.0), 0.0);
    let gr = paint_galley(ui, cell, g, Align::Min);
    if row.current {
        let nr = Rect::from_x_y_ranges(gr.right() + 6.0..=cell.right(), cell.y_range());
        paint_text(ui, nr, row.note, font(Weight::Regular, theme::FS_TINY), theme::TEXT_FAINT, Align::Min);
    }
    let pc = r.cell(2);
    let pw = row.pill.width(ui).min(pc.width());
    row.pill.paint(ui, Rect::from_min_size(pos2(pc.left(), pc.center().y - theme::PILL_H / 2.0), vec2(pw, theme::PILL_H)));
    r.text(ui, 3, row.kbps, font(Weight::Mono, 11.5), theme::TEXT_DIM, Align::Max);
    let mut action = DestAction::None;
    if row.removable {
        let clicked = r.show(ui, 4, Align::Center, |ui| {
            super::icon_btn(ui, "close", super::IconBtnKind::Small, row.remove_tip).clicked()
        });
        if clicked {
            action = DestAction::Remove;
        }
    }
    action
}

// ---------------------------------------------------------------------------
// Popup item, button row
// ---------------------------------------------------------------------------

/// Data for [`pop_item`].
#[derive(Clone, Copy, Debug)]
pub struct PopItem<'a> {
    pub icon: &'a str,
    pub label: &'a str,
    /// Second line (faint 11 px); makes the item 44 px tall.
    pub sub: Option<&'a str>,
    pub danger: bool,
    pub selected: bool,
    pub enabled: bool,
    /// Trailing audio meter with this many lit bars (0–5).
    pub meter: Option<u8>,
}

impl<'a> PopItem<'a> {
    pub fn new(icon: &'a str, label: &'a str) -> Self {
        Self { icon, label, sub: None, danger: false, selected: false, enabled: true, meter: None }
    }
    pub fn sub(mut self, sub: &'a str) -> Self {
        self.sub = Some(sub);
        self
    }
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn meter(mut self, lit: u8) -> Self {
        self.meter = Some(lit);
        self
    }
}

/// Popup menu item (32 px, or 44 px with a sub line), clickable unless disabled.
pub fn pop_item(ui: &mut Ui, item: &PopItem) -> Response {
    const WITH_METER: [Slot; 3] = [fixed(18.0), FILL, fixed(23.0)];
    const PLAIN: [Slot; 2] = [fixed(18.0), FILL];
    let h = if item.sub.is_some() { theme::POP_ITEM_TWO_H } else { theme::ROW_H };
    let slots: &[Slot] = if item.meter.is_some() { &WITH_METER } else { &PLAIN };
    let sense = if item.enabled { Sense::click() } else { Sense::hover() };
    ui.scope(|ui| {
        if !item.enabled {
            ui.multiply_opacity(super::DISABLED_OPACITY);
        }
        let r = HBox::new("pop_item", h).pad(theme::S2, theme::S2).slots(slots).sense(sense).show(ui);
        let r7 = CornerRadius::same(7);
        if item.selected {
            ui.painter().rect_filled(r.rect, r7, theme::ACCENT_ROW_SELECTED);
        } else if item.enabled && r.response.hovered() {
            ui.painter().rect_filled(r.rect, r7, theme::WASH_HOVER_2);
        }
        let (fg, ic) = if item.danger { (theme::DANGER, theme::DANGER) } else { (theme::TEXT, theme::TEXT_DIM) };
        r.icon(ui, 0, item.icon, 16.0, ic);
        let cell = r.cell(1);
        match item.sub {
            None => r.text(ui, 1, item.label, font(Weight::Regular, theme::FS_LABEL), fg, Align::Min),
            Some(sub) => {
                let top = Rect::from_x_y_ranges(cell.x_range(), cell.center().y - 17.0..=cell.center().y);
                let bot = Rect::from_x_y_ranges(cell.x_range(), cell.center().y..=cell.center().y + 15.0);
                paint_text(ui, top, item.label, font(Weight::Regular, theme::FS_LABEL), fg, Align::Min);
                paint_text(ui, bot, sub, font(Weight::Regular, theme::FS_TINY), theme::TEXT_FAINT, Align::Min);
            }
        }
        if let Some(lit) = item.meter {
            r.show(ui, 2, Align::Center, |ui| super::meter(ui, lit));
        }
        if item.enabled { r.response.on_hover_cursor(egui::CursorIcon::PointingHand) } else { r.response }
    })
    .inner
}

/// Equal-width button row (`hbox·buttons`): every button gets `(avail − 8·(n−1)) / n`.
/// All buttons should share one size (32 or 36). Returns the index clicked, if any.
pub fn btn_row(ui: &mut Ui, buttons: &[Btn]) -> Option<usize> {
    let h = buttons.iter().map(|b| b.height()).fold(theme::CTL_H, f32::max);
    let slots = vec![FILL; buttons.len()];
    let r = HBox::new("btn_row", h).slots(&slots).show(ui);
    let mut clicked = None;
    for (i, b) in buttons.iter().enumerate() {
        let w = r.cell(i).width();
        if r.show(ui, i, Align::Min, |ui| b.show_sized(ui, w)).clicked() {
            clicked = Some(i);
        }
    }
    clicked
}
