//! Primitive widgets: buttons, icon buttons, pills, status dots, avatars, meters,
//! segmented control, styled checkbox / radio, slider, value boxes, select, text field,
//! collapser, section heading, banner, separators.
//!
//! Sizing convention: a primitive either *fills* `ui.available_width()` (segmented,
//! slider, select, field, value box — they are meant for a fill or fixed slot) or is
//! *content-sized* and exposes a `width(ui)` so a caller can reserve an exact slot for
//! it (buttons, pills). Heights are always a token (`CTL_H`, `CTL_H_LG`, `PILL_H`, …).

use egui::{Align, Color32, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use super::{fade, paint_dashed_rect, paint_galley, paint_glow, paint_gradient, paint_hatch, paint_icon, paint_text, text_galley};
use crate::theme::{self, Weight, font};

fn cr(r: u8) -> CornerRadius {
    CornerRadius::same(r)
}

fn pill_radius(h: f32) -> CornerRadius {
    CornerRadius::same((h / 2.0).round() as u8)
}

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

/// Button variants (`.btn-*` in the mockup).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BtnKind {
    /// Accent gradient + glow, dark text — the one primary action.
    Primary,
    /// `BTN` fill, `BTN_BORDER` border — everything else.
    Secondary,
    /// Transparent, red text + red border — destructive but not final.
    Danger,
    /// Solid red, dark text — the "Stop sharing" bar button.
    DangerSolid,
    /// Dashed accent outline, left-aligned — "Create or join a room", "+ Add…".
    Ghost,
}

/// A text button (optionally with leading / trailing icons). Build, then [`Btn::show`]
/// (content-sized), [`Btn::show_sized`] (exact width, e.g. a fill slot) or let
/// [`super::btn_row`] lay several out at equal widths.
#[derive(Clone, Copy, Debug)]
pub struct Btn<'a> {
    pub kind: BtnKind,
    pub label: &'a str,
    pub icon: Option<&'a str>,
    pub trailing_icon: Option<&'a str>,
    /// 36 px (`CTL_H_LG`, bottom bar / modal footers) instead of 32.
    pub large: bool,
    pub enabled: bool,
    /// Tooltip.
    pub tip: Option<&'a str>,
}

impl<'a> Btn<'a> {
    pub fn new(kind: BtnKind, label: &'a str) -> Self {
        Self { kind, label, icon: None, trailing_icon: None, large: false, enabled: true, tip: None }
    }
    pub fn primary(label: &'a str) -> Self {
        Self::new(BtnKind::Primary, label)
    }
    pub fn secondary(label: &'a str) -> Self {
        Self::new(BtnKind::Secondary, label)
    }
    pub fn danger(label: &'a str) -> Self {
        Self::new(BtnKind::Danger, label)
    }
    pub fn danger_solid(label: &'a str) -> Self {
        Self::new(BtnKind::DangerSolid, label)
    }
    pub fn ghost(label: &'a str) -> Self {
        Self::new(BtnKind::Ghost, label)
    }
    /// Leading Material Symbols icon.
    pub fn icon(mut self, name: &'a str) -> Self {
        self.icon = Some(name);
        self
    }
    /// Trailing icon (e.g. `"expand_more"` for a menu button).
    pub fn trailing(mut self, name: &'a str) -> Self {
        self.trailing_icon = Some(name);
        self
    }
    /// 36 px tall.
    pub fn large(mut self) -> Self {
        self.large = true;
        self
    }
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }
    pub fn tip(mut self, tip: &'a str) -> Self {
        self.tip = Some(tip);
        self
    }

    pub fn height(&self) -> f32 {
        if self.large { theme::CTL_H_LG } else { theme::CTL_H }
    }
    fn pad(&self) -> f32 {
        if self.large { 14.0 } else { theme::S3 }
    }
    fn font_size(&self) -> f32 {
        if self.large { 12.5 } else { theme::FS_SMALL }
    }
    const GAP: f32 = 6.0;

    /// Content width: padding + icon + gap + label + gap + trailing icon + padding.
    pub fn width(&self, ui: &Ui) -> f32 {
        let mut w = 2.0 * self.pad();
        let mut parts = 0;
        if self.icon.is_some() {
            w += theme::ICON_BTN;
            parts += 1;
        }
        if !self.label.is_empty() {
            w += super::text_width(ui, self.label, font(Weight::Bold, self.font_size()), 0.0).ceil();
            parts += 1;
        }
        if self.trailing_icon.is_some() {
            w += theme::ICON_BTN;
            parts += 1;
        }
        w + Self::GAP * (parts.max(1) - 1) as f32
    }

    /// Content-sized button.
    pub fn show(self, ui: &mut Ui) -> Response {
        let w = self.width(ui);
        self.show_sized(ui, w)
    }

    /// Button exactly `width` wide (content centred; Ghost is left-aligned).
    pub fn show_sized(self, ui: &mut Ui, width: f32) -> Response {
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(vec2(width, self.height()), sense);
        self.paint(ui, rect, &resp);
        let resp = if self.enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
        match self.tip {
            Some(t) => resp.on_hover_text(t),
            None => resp,
        }
    }

    fn paint(&self, ui: &Ui, rect: Rect, resp: &Response) {
        if !ui.is_rect_visible(rect) {
            return;
        }
        let on = self.enabled;
        let hover = on && resp.hovered();
        let p = ui.painter();
        let r = cr(theme::ROUNDING_MD);
        let fg = match self.kind {
            BtnKind::Primary => {
                if on {
                    paint_glow(p, rect, r, 3, 14, theme::ACCENT_GLOW);
                }
                let (top, bot) = if hover {
                    (theme::ACCENT_LIGHT.lerp_to_gamma(Color32::WHITE, 0.08), theme::ACCENT.lerp_to_gamma(Color32::WHITE, 0.08))
                } else {
                    (theme::ACCENT_LIGHT, theme::ACCENT)
                };
                paint_gradient(p, rect, r, 180.0, fade(top, on), fade(bot, on));
                theme::PRIMARY_TEXT
            }
            BtnKind::Secondary => {
                let fill = if hover { theme::WASH_HOVER_1 } else { theme::BTN };
                p.rect(rect, r, fade(fill, on), Stroke::new(1.0_f32, fade(theme::BTN_BORDER, on)), StrokeKind::Inside);
                theme::TEXT
            }
            BtnKind::Danger => {
                let fill = if hover { theme::DANGER_TINT } else { Color32::TRANSPARENT };
                p.rect(rect, r, fill, Stroke::new(1.0_f32, fade(theme::DANGER_BORDER, on)), StrokeKind::Inside);
                theme::DANGER
            }
            BtnKind::DangerSolid => {
                let fill = if hover { theme::DANGER.lerp_to_gamma(Color32::WHITE, 0.08) } else { theme::DANGER };
                p.rect(rect, r, fade(fill, on), Stroke::new(1.0_f32, fade(theme::DANGER, on)), StrokeKind::Inside);
                theme::PRIMARY_TEXT
            }
            BtnKind::Ghost => {
                let fill = if hover { theme::ACCENT_GHOST_BG_HOVER } else { theme::ACCENT_GHOST_BG };
                p.rect_filled(rect, r, fade(fill, on));
                paint_dashed_rect(p, rect, r, Stroke::new(1.0_f32, fade(theme::ACCENT_GHOST_BORDER, on)), 4.0, 3.0);
                theme::ACCENT
            }
        };
        let fg = fade(fg, on);
        // Content: [icon][label][trailing], centred (ghost: left-aligned at the padding).
        let content_w = self.width(ui) - 2.0 * self.pad();
        let x0 = if self.kind == BtnKind::Ghost {
            rect.left() + self.pad()
        } else {
            (rect.center().x - content_w / 2.0).max(rect.left() + self.pad())
        };
        let mut x = x0;
        let y = rect.y_range();
        if let Some(i) = self.icon {
            paint_icon(ui, Rect::from_x_y_ranges(x..=x + theme::ICON_BTN, y), i, theme::ICON_BTN, fg);
            x += theme::ICON_BTN + Self::GAP;
        }
        if !self.label.is_empty() {
            let trailing_w = if self.trailing_icon.is_some() { theme::ICON_BTN + Self::GAP } else { 0.0 };
            let max_w = (rect.right() - self.pad() - trailing_w - x).max(0.0);
            let g = text_galley(ui, self.label, font(Weight::Bold, self.font_size()), fg, max_w + 0.5, 0.0);
            let gr = paint_galley(ui, Rect::from_x_y_ranges(x..=x + g.size().x, y), g, Align::Min);
            x = gr.right() + Self::GAP;
        }
        if let Some(i) = self.trailing_icon {
            paint_icon(ui, Rect::from_x_y_ranges(x..=x + theme::ICON_BTN, y), i, theme::ICON_BTN, fg);
        }
    }
}

/// Icon-only button styles (`.icon-btn`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconBtnKind {
    /// 32×32, transparent, dim icon (close ×, settings gear).
    Plain,
    /// 32×32 with `BTN` fill and border (room "⋯" menu).
    Bordered,
    /// 26×26, 16 px icon (dest-row ×).
    Small,
}

/// An icon-only button. Always give it a tooltip (`tip`) — it has no visible label.
pub fn icon_btn(ui: &mut Ui, name: &str, kind: IconBtnKind, tip: &str) -> Response {
    let (size, icon) = match kind {
        IconBtnKind::Small => (26.0, 16.0),
        _ => (theme::CTL_H, theme::ICON),
    };
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::click());
    if ui.is_rect_visible(rect) {
        let hover = resp.hovered();
        let p = ui.painter();
        let r = cr(theme::ROUNDING_SM);
        match kind {
            IconBtnKind::Bordered => {
                let fill = if hover { theme::WASH_HOVER_1 } else { theme::BTN };
                p.rect(rect, r, fill, Stroke::new(1.0_f32, theme::BTN_BORDER), StrokeKind::Inside);
            }
            _ if hover => {
                p.rect_filled(rect, r, theme::WASH_HOVER_1);
            }
            _ => {}
        }
        let col = if hover || kind == IconBtnKind::Bordered { theme::TEXT } else { theme::TEXT_DIM };
        paint_icon(ui, rect, name, icon, col);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip)
}

// ---------------------------------------------------------------------------
// Pills (badges), status dots, avatars, meters
// ---------------------------------------------------------------------------

/// Pill color families (`.badge-*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Green (`.badge-live`).
    Live,
    /// Accent, in progress (`.badge-progress`).
    Progress,
    /// Amber hatched (`.badge-degraded`).
    Degraded,
    /// Grey (`.badge-muted`).
    Muted,
}

/// What a pill shows before its text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PillLead<'a> {
    None,
    /// Glowing 6 px dot (live).
    Dot,
    /// Marching 14×4 dashes (in progress).
    Bar,
    /// A 12 px icon.
    Icon(&'a str),
}

/// A status pill, 18 px (or 22 px when `large`), uppercase 9.5 px bold text.
#[derive(Clone, Copy, Debug)]
pub struct Pill<'a> {
    pub tone: Tone,
    pub text: &'a str,
    pub lead: PillLead<'a>,
    pub large: bool,
}

impl<'a> Pill<'a> {
    /// Green, with a dot ("● LIVE").
    pub fn live(text: &'a str) -> Self {
        Self { tone: Tone::Live, text, lead: PillLead::Dot, large: false }
    }
    /// Accent, with the marching bar ("Starting", "Moving", "Applying").
    pub fn progress(text: &'a str) -> Self {
        Self { tone: Tone::Progress, text, lead: PillLead::Bar, large: false }
    }
    /// Amber hatched, with the sync icon ("Reconnecting").
    pub fn degraded(text: &'a str) -> Self {
        Self { tone: Tone::Degraded, text, lead: PillLead::Icon("sync"), large: false }
    }
    /// Grey, no lead ("Not live", "Ready").
    pub fn muted(text: &'a str) -> Self {
        Self { tone: Tone::Muted, text, lead: PillLead::None, large: false }
    }
    pub fn lead(mut self, lead: PillLead<'a>) -> Self {
        self.lead = lead;
        self
    }
    /// 22 px variant (`.badge.lg`).
    pub fn large(mut self) -> Self {
        self.large = true;
        self
    }

    fn h(&self) -> f32 {
        if self.large { theme::PILL_H_LG } else { theme::PILL_H }
    }
    fn fs(&self) -> f32 {
        if self.large { 10.0 } else { theme::FS_PILL }
    }
    fn lead_w(&self) -> f32 {
        match self.lead {
            PillLead::None => 0.0,
            PillLead::Dot => 6.0,
            PillLead::Bar => 14.0,
            PillLead::Icon(_) => 12.0,
        }
    }
    fn label(&self) -> String {
        self.text.to_uppercase()
    }

    /// Content width.
    pub fn width(&self, ui: &Ui) -> f32 {
        let (pl, pr) = if self.large { (8.0, 10.0) } else { (7.0, 8.0) };
        let lead = self.lead_w();
        let gap = if lead > 0.0 { 5.0 } else { 0.0 };
        let text = super::text_width(ui, &self.label(), font(Weight::Bold, self.fs()), self.fs() * 0.05);
        (pl + lead + gap + text + pr).ceil()
    }

    /// Content-sized pill.
    pub fn show(self, ui: &mut Ui) -> Response {
        let (rect, resp) = ui.allocate_exact_size(vec2(self.width(ui), self.h()), Sense::hover());
        self.paint(ui, rect);
        resp
    }

    /// Paint into `rect` (its height should be the pill height).
    pub fn paint(&self, ui: &Ui, rect: Rect) {
        let p = ui.painter();
        let r = pill_radius(rect.height());
        let (fg, bg, border) = match self.tone {
            Tone::Live => (theme::SUCCESS, theme::PILL_LIVE_BG, theme::PILL_LIVE_BORDER),
            Tone::Progress => (theme::ACCENT_LIGHTER, theme::PILL_PROGRESS_BG, theme::PILL_PROGRESS_BORDER),
            Tone::Degraded => (theme::WARNING, Color32::TRANSPARENT, theme::HATCH_AMBER_BORDER),
            Tone::Muted => (theme::TEXT_DIM, theme::PANEL_SUNKEN, theme::BORDER),
        };
        if self.tone == Tone::Degraded {
            paint_hatch(p, rect, r, 4.0, theme::HATCH_AMBER_STRONG, theme::HATCH_AMBER_WEAK);
        } else {
            p.rect_filled(rect, r, bg);
        }
        p.rect_stroke(rect, r, Stroke::new(1.0_f32, border), StrokeKind::Inside);
        let pl = if self.large { 8.0 } else { 7.0 };
        let mut x = rect.left() + pl;
        let cy = rect.center().y;
        match self.lead {
            PillLead::None => {}
            PillLead::Dot => {
                let c = pos2(x + 3.0, cy);
                p.circle_filled(c, 5.0, theme::SUCCESS_GLOW.gamma_multiply(0.25));
                p.circle_filled(c, 3.0, fg);
            }
            PillLead::Bar => {
                // Marching dashes: 4 on, 3 off, moving 14 px every 0.65 s.
                let t = ui.input(|i| i.time) as f32;
                let off = (t / 0.65).fract() * 7.0;
                let bar = Rect::from_min_size(pos2(x, cy - 2.0), vec2(14.0, 4.0));
                let pp = p.with_clip_rect(bar.intersect(p.clip_rect()));
                let mut dx = -7.0 + off;
                while dx < 14.0 {
                    pp.rect_filled(Rect::from_min_size(pos2(x + dx, cy - 2.0), vec2(4.0, 4.0)), cr(1), fg);
                    dx += 7.0;
                }
                ui.ctx().request_repaint();
            }
            PillLead::Icon(name) => {
                paint_icon(ui, Rect::from_center_size(pos2(x + 6.0, cy), vec2(12.0, 12.0)), name, 12.0, fg);
            }
        }
        if self.lead_w() > 0.0 {
            x += self.lead_w() + 5.0;
        }
        let g = text_galley(ui, &self.label(), font(Weight::Bold, self.fs()), fg, f32::INFINITY, self.fs() * 0.05);
        paint_galley(ui, Rect::from_x_y_ranges(x..=rect.right(), rect.y_range()), g, Align::Min);
    }
}

/// Room connection state shown by [`status_dot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DotState {
    /// Solid green with glow.
    Joined,
    /// Amber, pulsing.
    Reconnecting,
    /// Hollow faint ring.
    Disconnected,
}

/// A 9 px status dot, allocated as a 9×9 rect (centre it in an 18 slot).
pub fn status_dot(ui: &mut Ui, state: DotState) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(theme::DOT_SIZE, theme::DOT_SIZE), Sense::hover());
    paint_status_dot(ui, rect.center(), state);
    resp
}

/// Paint a status dot centred at `c`.
pub fn paint_status_dot(ui: &Ui, c: egui::Pos2, state: DotState) {
    let p = ui.painter();
    let r = theme::DOT_SIZE / 2.0;
    match state {
        DotState::Joined => {
            p.circle_filled(c, r + 2.5, theme::SUCCESS_GLOW.gamma_multiply(0.18));
            p.circle_filled(c, r, theme::SUCCESS);
        }
        DotState::Reconnecting => {
            // CSS `pulse`: opacity 1 → .35 → 1 over 1.2 s.
            let t = ui.input(|i| i.time) as f32;
            let k = 0.675 + 0.325 * (t / 1.2 * std::f32::consts::TAU).cos();
            p.circle_filled(c, r, theme::WARNING.gamma_multiply(k));
            ui.ctx().request_repaint();
        }
        DotState::Disconnected => {
            p.circle_stroke(c, r - 0.75, Stroke::new(1.5_f32, theme::TEXT_FAINT));
        }
    }
}

/// A round avatar with `initial`, `size` = `AVATAR` or `AVATAR_SM`, flat
/// `color` (always `theme::avatar_color(name)`), and an optional green presence dot.
pub fn avatar(ui: &mut Ui, initial: &str, size: f32, color: Color32, presence: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(size, size), Sense::hover());
    paint_avatar(ui, rect, initial, color, presence);
    resp
}

/// Paint an avatar into a square `rect`.
pub fn paint_avatar(ui: &Ui, rect: Rect, initial: &str, color: Color32, presence: bool) {
    let p = ui.painter();
    p.rect_filled(rect, pill_radius(rect.height()), color);
    let fs = if rect.height() <= theme::AVATAR_SM { 10.5 } else { 13.0 };
    paint_text(ui, rect, &initial.to_uppercase(), font(Weight::Bold, fs), Color32::WHITE, Align::Center);
    if presence {
        // Mockup `.avatar .pres`: a 10 px green dot with a 2 px ring in the user
        // band's `ELEV_14` (content-box, so 14 px outer) at right/bottom -2, i.e.
        // centred 5 px in from the corner.
        let c = pos2(rect.right() - 5.0, rect.bottom() - 5.0);
        p.circle_filled(c, 7.0, theme::ELEV_14);
        p.circle_filled(c, 5.0, theme::SUCCESS);
    }
}

/// The two-rectangle brand mark (mockup `.mark`, 30x22), vertically centred at
/// the left of `cell`: an accent outline behind a green outline filled with `bg`
/// (the surface it sits on) that knocks the back one out. The CSS boxes are
/// content-box (`19x13` / `15x11` plus a 2.5 px border each side), so the
/// outer boxes are 24x18 at the top-left and 20x16 at the bottom-right.
pub fn paint_brand_mark(ui: &Ui, cell: Rect, bg: Color32) {
    let o = pos2(cell.left(), (cell.center().y - 11.0).round());
    let p = ui.painter();
    let back = Rect::from_min_size(o, vec2(24.0, 18.0));
    let front = Rect::from_min_size(o + vec2(10.0, 6.0), vec2(20.0, 16.0));
    p.rect_stroke(back, cr(5), Stroke::new(2.5_f32, theme::ACCENT_LIGHT), StrokeKind::Inside);
    p.rect(front, cr(5), bg, Stroke::new(2.5_f32, theme::SUCCESS), StrokeKind::Inside);
}

/// A 5-bar audio level meter (23×14): `lit` bars (0–5) green, the 5th amber when `hot`.
pub fn meter(ui: &mut Ui, lit: u8) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(23.0, 14.0), Sense::hover());
    let heights = [5.0, 8.0, 11.0, 14.0, 14.0];
    for (i, h) in heights.iter().enumerate() {
        let x = rect.left() + i as f32 * 5.0;
        let bar = Rect::from_min_max(pos2(x, rect.bottom() - h), pos2(x + 3.0, rect.bottom()));
        let col = if (i as u8) < lit { if i == 4 { theme::WARNING } else { theme::SUCCESS } } else { theme::BTN_BORDER };
        ui.painter().rect_filled(bar, cr(1), col);
    }
    resp
}

// ---------------------------------------------------------------------------
// Segmented control, checkbox, radio
// ---------------------------------------------------------------------------

/// One-of-N segmented control (`.seg`): fills the available width, 32 px, equal-width
/// segments; the selected one is the accent gradient with dark bold text. Returns `true`
/// on the frame the selection changes. Long labels truncate with "…".
pub fn segmented<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, options: &[(T, &str)]) -> bool {
    let w = ui.available_width();
    let (outer, resp) = ui.allocate_exact_size(vec2(w, theme::CTL_H), Sense::hover());
    let p = ui.painter();
    p.rect(outer, cr(theme::ROUNDING_MD), theme::ELEV_1B, Stroke::new(1.0_f32, theme::INSET_BORDER), StrokeKind::Inside);
    let inner = outer.shrink(3.0);
    let n = options.len().max(1);
    let spans = super::slot_spans(inner.width(), 3.0, &vec![super::Slot::Fill; n]);
    let mut changed = false;
    for (i, ((value, label), (x, sw))) in options.iter().zip(spans).enumerate() {
        let seg = Rect::from_min_size(pos2(inner.left() + x, inner.top()), vec2(sw, inner.height()));
        let r = ui.interact(seg, resp.id.with(i), Sense::click());
        let selected = *current == *value;
        let enabled = ui.is_enabled();
        let fg = if selected {
            paint_gradient(ui.painter(), seg, cr(theme::ROUNDING_XS), 180.0, theme::ACCENT_LIGHT, theme::ACCENT);
            theme::PRIMARY_TEXT
        } else if r.hovered() && enabled {
            ui.painter().rect_filled(seg, cr(theme::ROUNDING_XS), theme::WASH_HOVER_1);
            theme::TEXT
        } else {
            theme::TEXT_DIM
        };
        let f = if selected { font(Weight::Bold, theme::FS_SMALL) } else { font(Weight::Regular, theme::FS_SMALL) };
        let g = text_galley(ui, label, f, fg, sw - 8.0, 0.0);
        paint_galley(ui, seg, g, Align::Center);
        if enabled {
            let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
            if r.clicked() && !selected {
                *current = *value;
                changed = true;
            }
        }
    }
    changed
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    Check,
    Radio,
}

/// Shared checkbox/radio renderer (the Forza guide's `mark_ui`, mockup `.cbx`): an 18 px
/// mark + 8 px gap + 12.5 px label, 32 px tall, whole row clickable with a hover wash
/// that extends 8 px left/right. Content-sized (never wider than the space left).
fn mark_ui(ui: &mut Ui, on: bool, label: &str, enabled: bool, shape: Mark) -> Response {
    let f = font(Weight::Regular, theme::FS_LABEL);
    let text_w = if label.is_empty() { 0.0 } else { super::text_width(ui, label, f.clone(), 0.0) + theme::S2 };
    // Never wider than the space the row has; in a wrapping layout the limit is the
    // whole line, so an option that doesn't fit moves to the next line instead of
    // truncating (the Forza guide's `mark_ui` rule).
    let limit = if ui.layout().main_wrap { ui.max_rect().width() } else { ui.available_size_before_wrap().x };
    let w = (theme::MARK + text_w).min(limit.max(theme::MARK));
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(vec2(w, theme::CTL_H), sense);
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    if enabled && resp.hovered() {
        p.rect_filled(rect.expand2(vec2(theme::S2, 0.0)), cr(theme::ROUNDING_XS), theme::WASH_HOVER_1);
    }
    let bx = Rect::from_min_size(pos2(rect.left(), rect.center().y - theme::MARK / 2.0), vec2(theme::MARK, theme::MARK));
    let accent = fade(theme::ACCENT, enabled);
    match shape {
        Mark::Check => {
            if on {
                p.rect_filled(bx, cr(5), accent);
                // CSS draws the 15 px "check" glyph at weight 700; Material's outline
                // check at 400 is thin, so paint it twice offset by ½ px for weight.
                for dx in [0.0, 0.5] {
                    paint_icon(ui, bx.translate(vec2(dx, 0.0)), "check", 15.0, fade(Color32::WHITE, enabled));
                }
            } else {
                p.rect_stroke(bx, cr(5), Stroke::new(1.0_f32, fade(theme::BTN_BORDER, enabled)), StrokeKind::Inside);
            }
        }
        Mark::Radio => {
            let c = bx.center();
            if on {
                p.circle_filled(c, theme::MARK / 2.0, accent);
                p.circle_filled(c, 3.5, fade(Color32::WHITE, enabled));
            } else {
                p.circle_stroke(c, theme::MARK / 2.0 - 0.5, Stroke::new(1.0_f32, fade(theme::BTN_BORDER, enabled)));
            }
        }
    }
    if !label.is_empty() {
        let tr = Rect::from_x_y_ranges(bx.right() + theme::S2..=rect.right(), rect.y_range());
        paint_text(ui, tr, label, f, fade(theme::TEXT, enabled), Align::Min);
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// The styled checkbox — always use this, never `egui::Checkbox`. Toggles `*checked`.
pub fn checkbox(ui: &mut Ui, checked: &mut bool, label: &str) -> Response {
    checkbox_enabled(ui, checked, label, true)
}

/// [`checkbox`] that can be disabled (dimmed, ignores clicks).
pub fn checkbox_enabled(ui: &mut Ui, checked: &mut bool, label: &str, enabled: bool) -> Response {
    let mut r = mark_ui(ui, *checked, label, enabled, Mark::Check);
    if enabled && r.clicked() {
        *checked = !*checked;
        r.mark_changed();
    }
    r
}

/// The styled radio matching [`checkbox`]. Sets `*current = value` on click.
pub fn radio<T: PartialEq>(ui: &mut Ui, current: &mut T, value: T, label: &str) -> Response {
    let selected = *current == value;
    let mut r = mark_ui(ui, selected, label, true, Mark::Radio);
    if r.clicked() && !selected {
        *current = value;
        r.mark_changed();
    }
    r
}

// ---------------------------------------------------------------------------
// Slider, value boxes, select, text field
// ---------------------------------------------------------------------------

/// A slider filling the available width (32 px tall hit area): 5 px track with the
/// accent-gradient fill up to the value and a 14 px light thumb. Snaps to `step`.
pub fn slider<N: egui::emath::Numeric>(ui: &mut Ui, value: &mut N, range: std::ops::RangeInclusive<N>, step: f64) -> Response {
    let w = ui.available_width();
    let (rect, mut resp) = ui.allocate_exact_size(vec2(w, theme::CTL_H), Sense::click_and_drag());
    let (lo, hi) = (range.start().to_f64(), range.end().to_f64());
    const THUMB: f32 = 14.0;
    // CSS range input: the 5 px track spans the full width; the thumb's centre
    // travels inside it, half a thumb in from each end.
    let track = Rect::from_min_max(pos2(rect.left(), rect.center().y - 2.5), pos2(rect.right(), rect.center().y + 2.5));
    let travel = track.x_range().shrink(THUMB / 2.0);
    if ui.is_enabled()
        && let Some(pos) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
    {
        let t = ((pos.x - travel.min) / travel.span().max(1.0)).clamp(0.0, 1.0) as f64;
        let mut v = lo + t * (hi - lo);
        if step > 0.0 {
            v = lo + ((v - lo) / step).round() * step;
        }
        let v = N::from_f64(v.clamp(lo, hi));
        if v.to_f64() != value.to_f64() {
            *value = v;
            resp.mark_changed();
        }
    }
    let t = if hi > lo { ((value.to_f64() - lo) / (hi - lo)).clamp(0.0, 1.0) as f32 } else { 0.0 };
    let p = ui.painter();
    p.rect(track, cr(3), theme::ELEV_1B, Stroke::new(1.0_f32, theme::INSET_BORDER), StrokeKind::Outside);
    let thumb_x = travel.min + travel.span() * t;
    let fill = Rect::from_min_max(track.min, pos2(thumb_x, track.bottom()));
    if t > 0.0 {
        paint_gradient(p, fill, cr(3), 90.0, theme::ACCENT_DEEP, theme::ACCENT);
    }
    let c = pos2(thumb_x, track.center().y);
    p.circle_filled(c + vec2(0.0, 1.0), THUMB / 2.0 + 1.5, Color32::from_black_alpha(90));
    let thumb = if resp.hovered() || resp.dragged() { Color32::WHITE } else { theme::TEXT };
    p.circle_filled(c, THUMB / 2.0, thumb);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn value_frame(ui: &Ui, rect: Rect, focused: bool) {
    let stroke = if focused { theme::ACCENT } else { theme::BORDER };
    ui.painter().rect(rect, cr(theme::ROUNDING_XS), theme::FIELD, Stroke::new(1.0_f32, stroke), StrokeKind::Inside);
}

/// A read-only value box (`.val`): fills the available width (put it in a `VALUE_W`
/// slot), 32 px, `FIELD` fill, mono 12 px right-aligned text.
pub fn value_box(ui: &mut Ui, text: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), theme::CTL_H), Sense::hover());
    value_frame(ui, rect, false);
    paint_text(ui, rect.shrink2(vec2(theme::S2, 0.0)), text, font(Weight::Mono, theme::FS_SMALL), theme::TEXT, Align::Max);
    resp
}

/// An empty value box with a widget centred in it (the audio meter in the Source card).
pub fn value_box_with<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), theme::CTL_H), Sense::hover());
    value_frame(ui, rect, false);
    // Content-sized widgets (meter, icon) centred in the box on both axes. Why
    // `centered_and_justified`: a `left_to_right(Center).with_main_align(Center)` child
    // only centres on the cross axis; egui ignores the main align of a non-justified
    // horizontal layout, so the meter sat at the box's left edge (task #65). Justified,
    // the widget's frame is the whole box and `allocate_exact_size` centres its rect in it.
    let mut child = ui.new_child(
        egui::UiBuilder::new().max_rect(rect).layout(egui::Layout::centered_and_justified(egui::Direction::LeftToRight)),
    );
    child.shrink_clip_rect(rect);
    add(&mut child)
}

/// An editable numeric value box: looks like [`value_box`]; drag horizontally to change
/// by `step`, click to type (Enter / focus loss commits, Escape cancels). The slider
/// row's value preview — "the exact value can still be typed" (Forza guide).
pub fn num_box<N: egui::emath::Numeric>(
    ui: &mut Ui,
    value: &mut N,
    range: std::ops::RangeInclusive<N>,
    step: f64,
    decimals: usize,
    suffix: &str,
) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(ui.available_width(), theme::CTL_H), Sense::click_and_drag());
    let edit_id = resp.id.with("edit");
    let mut editing: Option<String> = ui.ctx().data(|d| d.get_temp(edit_id));
    let (lo, hi) = (range.start().to_f64(), range.end().to_f64());
    let fmt = |v: f64| format!("{v:.decimals$}{suffix}");
    if editing.is_none() && resp.clicked() {
        editing = Some(format!("{:.decimals$}", value.to_f64()));
    }
    let f = font(Weight::Mono, theme::FS_SMALL);
    match editing {
        Some(mut buf) => {
            let te = egui::TextEdit::singleline(&mut buf)
                .id(edit_id.with("te"))
                .font(f)
                .horizontal_align(Align::Max)
                .vertical_align(Align::Center)
                .frame(false)
                .margin(egui::Margin::symmetric(theme::S2 as i8, 0));
            value_frame(ui, rect, true);
            let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
            let out = child.add_sized(rect.size(), te);
            if !out.has_focus() && !out.lost_focus() {
                out.request_focus();
            }
            let esc = ui.input(|i| i.key_pressed(egui::Key::Escape));
            if out.lost_focus() || esc {
                if !esc && let Ok(v) = buf.trim().trim_end_matches(suffix.trim()).trim().parse::<f64>() {
                    let v = N::from_f64(v.clamp(lo, hi));
                    if v.to_f64() != value.to_f64() {
                        *value = v;
                        resp.mark_changed();
                    }
                }
                ui.ctx().data_mut(|d| d.remove::<String>(edit_id));
            } else {
                ui.ctx().data_mut(|d| d.insert_temp(edit_id, buf));
            }
        }
        None => {
            if resp.dragged() {
                let dx = ui.input(|i| i.pointer.delta().x) as f64;
                let per_px = if step > 0.0 { step / 4.0 } else { (hi - lo) / 200.0 };
                let acc_id = resp.id.with("acc");
                let acc: f64 = ui.ctx().data(|d| d.get_temp(acc_id)).unwrap_or(value.to_f64()) + dx * per_px;
                ui.ctx().data_mut(|d| d.insert_temp(acc_id, acc.clamp(lo, hi)));
                let mut v = acc.clamp(lo, hi);
                if step > 0.0 {
                    v = lo + ((v - lo) / step).round() * step;
                }
                if v != value.to_f64() {
                    *value = N::from_f64(v);
                    resp.mark_changed();
                }
            } else {
                ui.ctx().data_mut(|d| d.remove::<f64>(resp.id.with("acc")));
            }
            value_frame(ui, rect, false);
            let txt = fmt(value.to_f64());
            paint_text(ui, rect.shrink2(vec2(theme::S2, 0.0)), &txt, f, theme::TEXT, Align::Max);
            resp = resp.on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
        }
    }
    resp
}

/// Dim mono readout text filling the width (`.ro`), e.g. "Display 1 · 1920×1080".
pub fn readout(ui: &mut Ui, text: &str, align: Align) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), theme::CTL_H), Sense::hover());
    paint_text(ui, rect, text, font(Weight::Mono, theme::FS_SMALL), theme::TEXT_DIM, align);
    resp
}

/// A dropdown (`select.field`): fills the available width, 32 px, `FIELD` fill. Sets
/// `*selected` to the picked index. `id_salt` must be unique among selects in the
/// same parent `Ui`.
pub fn select(ui: &mut Ui, id_salt: impl std::hash::Hash, selected: &mut usize, options: &[&str]) -> Response {
    let w = ui.available_width();
    let text = options.get(*selected).copied().unwrap_or("");
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals.widgets;
        for wv in [&mut v.inactive, &mut v.hovered, &mut v.active, &mut v.open] {
            wv.weak_bg_fill = theme::FIELD;
            wv.bg_fill = theme::FIELD;
            wv.corner_radius = cr(theme::ROUNDING_SM);
        }
        v.inactive.bg_stroke = Stroke::new(1.0_f32, theme::BORDER);
        v.hovered.bg_stroke = Stroke::new(1.0_f32, theme::BTN_BORDER);
        v.open.bg_stroke = Stroke::new(1.0_f32, theme::ACCENT);
        ui.style_mut().spacing.button_padding = vec2(10.0, 0.0);
        ui.style_mut().spacing.interact_size.y = theme::CTL_H;
        let mut changed = false;
        let mut r = egui::ComboBox::from_id_salt(id_salt)
            .icon(|ui, rect, _visuals, open| {
                let r = Rect::from_center_size(rect.center(), vec2(18.0, 18.0));
                paint_icon(ui, r, if open { "expand_less" } else { "expand_more" }, 18.0, theme::TEXT_DIM);
            })
            .width(w)
            .height(320.0)
            .selected_text(egui::RichText::new(text).font(font(Weight::Regular, theme::FS_ROW)).color(theme::TEXT))
            .show_ui(ui, |ui| {
                for (i, o) in options.iter().enumerate() {
                    if ui.selectable_label(*selected == i, *o).clicked() && *selected != i {
                        *selected = i;
                        changed = true;
                    }
                }
            })
            .response;
        if changed {
            r.mark_changed();
        }
        r
    })
    .inner
}

/// A single-line text field (`.field`): fills the available width, 32 px.
pub fn text_field(ui: &mut Ui, text: &mut String, hint: &str) -> Response {
    let w = ui.available_width();
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals;
        v.extreme_bg_color = theme::FIELD;
        v.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, theme::BORDER);
        v.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, theme::BTN_BORDER);
        v.selection.stroke = Stroke::new(1.0_f32, theme::ACCENT);
        for wv in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
            wv.corner_radius = cr(theme::ROUNDING_SM);
        }
        ui.add_sized(
            vec2(w, theme::CTL_H),
            egui::TextEdit::singleline(text)
                .hint_text(egui::RichText::new(hint).color(theme::TEXT_FAINT))
                .font(font(Weight::Regular, theme::FS_ROW))
                .vertical_align(Align::Center)
                .margin(egui::Margin::symmetric(10, 0)),
        )
    })
    .inner
}

/// A "› Advanced tuning" collapsing header (`.collapser`, 24 px). Toggles `*open`.
pub fn collapser(ui: &mut Ui, open: &mut bool, label: &str) -> Response {
    let f = font(Weight::Regular, theme::FS_SMALL);
    let w = 16.0 + 6.0 + super::text_width(ui, label, f.clone(), 0.0);
    let (rect, mut resp) = ui.allocate_exact_size(vec2(w, 24.0), Sense::click());
    let col = if resp.hovered() { theme::TEXT } else { theme::TEXT_DIM };
    let ir = Rect::from_min_size(rect.min, vec2(16.0, 24.0));
    paint_icon(ui, ir, if *open { "expand_more" } else { "chevron_right" }, 16.0, col);
    paint_text(ui, Rect::from_x_y_ranges(ir.right() + 6.0..=rect.right(), rect.y_range()), label, f, col, Align::Min);
    if resp.clicked() {
        *open = !*open;
        resp.mark_changed();
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---------------------------------------------------------------------------
// Headings, banners, separators, tips
// ---------------------------------------------------------------------------

/// Paint an uppercase accent section label (10 px bold, 0.11 em tracking) and optional
/// mono count into `rect`; returns the x where the text ends.
fn paint_section_label(ui: &Ui, rect: Rect, title: &str, count: Option<&str>) -> f32 {
    let g = text_galley(ui, &title.to_uppercase(), font(Weight::Bold, theme::FS_SECTION), theme::ACCENT, rect.width(), 1.1);
    let mut x = paint_galley(ui, rect, g, Align::Min).right();
    if let Some(c) = count {
        x += theme::S2;
        // CSS `.section-label .count` inherits `text-transform: uppercase` ("1 ROOM").
        let g = text_galley(ui, &c.to_uppercase(), font(Weight::Mono, theme::FS_UNIT), theme::TEXT_FAINT, (rect.right() - x).max(0.0), 0.0);
        x = paint_galley(ui, Rect::from_x_y_ranges(x..=rect.right(), rect.y_range()), g, Align::Min).right();
    }
    x
}

/// Section heading (`hbox·heading`): `[LABEL][count][rule fill?]`, 16 px tall, full width.
pub fn heading(ui: &mut Ui, title: &str, count: Option<&str>, rule: bool) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), theme::HEADING_H), Sense::hover());
    let x = paint_section_label(ui, rect, title, count);
    if rule && x + theme::S2 < rect.right() {
        let y = rect.center().y.round() + 0.5;
        ui.painter().hline((x + theme::S2)..=rect.right(), y, Stroke::new(1.0_f32, theme::WELL_BORDER));
    }
    resp
}

/// Card title (same look as [`heading`], no rule). Used by [`super::card`].
pub fn card_title(ui: &mut Ui, title: &str, count: Option<&str>) -> Response {
    heading(ui, title, count, false)
}

/// Banner flavours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BannerKind {
    /// Accent chip colors — neutral information.
    Info,
    /// Flat amber tint — a warning.
    Warn,
    /// Amber 45° hatch (`.banner`) — degraded state: reconnecting, over budget.
    Hatched,
}

/// A banner: `[slot18 icon][fill text (wraps)][optional action button]`, min 32 px,
/// padding 8/12, radius 9. Returns `true` when the action button was clicked.
pub fn banner(ui: &mut Ui, kind: BannerKind, icon: &str, text: &str, action: Option<&str>) -> bool {
    let w = ui.available_width();
    let pad = vec2(theme::S3, theme::S2);
    let btn = action.map(Btn::secondary);
    let btn_w = btn.map(|b| b.width(ui) + theme::S2).unwrap_or(0.0);
    let text_w = (w - 2.0 * pad.x - theme::ICON - theme::S2 - btn_w).max(10.0);
    let (fg, tf) = match kind {
        BannerKind::Info => (theme::ACCENT_LIGHTER, theme::TEXT),
        BannerKind::Warn | BannerKind::Hatched => (theme::WARNING, theme::WARNING),
    };
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat { font_id: font(Weight::Regular, theme::FS_SMALL), color: tf, line_height: Some(18.0), ..Default::default() },
    );
    job.wrap.max_width = text_w;
    let g = ui.painter().layout_job(job);
    // CSS: `min-height: 32px` on the content box, plus 8 px padding and the 1 px border.
    let h = g.size().y.max(theme::CTL_H) + 2.0 * pad.y + 2.0;
    let (rect, _) = ui.allocate_exact_size(vec2(w, h), Sense::hover());
    let p = ui.painter();
    let r = cr(theme::ROUNDING_MD);
    match kind {
        BannerKind::Info => {
            p.rect(rect, r, theme::ACCENT_CHIP_BG, Stroke::new(1.0_f32, theme::ACCENT_CHIP_BORDER), StrokeKind::Inside);
        }
        BannerKind::Warn => {
            p.rect(rect, r, theme::HATCH_AMBER_STRONG, Stroke::new(1.0_f32, theme::HATCH_AMBER_BORDER), StrokeKind::Inside);
        }
        BannerKind::Hatched => {
            paint_hatch(p, rect, r, 6.0, theme::HATCH_AMBER_SOFT_A, theme::HATCH_AMBER_SOFT_B);
            p.rect_stroke(rect, r, Stroke::new(1.0_f32, theme::HATCH_AMBER_BORDER), StrokeKind::Inside);
        }
    }
    let inner = rect.shrink2(pad);
    let ir = Rect::from_min_size(pos2(inner.left(), inner.center().y - theme::ICON / 2.0), vec2(theme::ICON, theme::ICON));
    paint_icon(ui, ir, icon, theme::ICON, fg);
    let tx = ir.right() + theme::S2;
    ui.painter().galley(pos2(tx, inner.center().y - g.size().y / 2.0), g, tf);
    let mut clicked = false;
    if let Some(b) = btn {
        let bw = b.width(ui);
        let br = Rect::from_min_size(pos2(inner.right() - bw, inner.center().y - theme::CTL_H / 2.0), vec2(bw, theme::CTL_H));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(br));
        clicked = b.show_sized(&mut child, bw).clicked();
    }
    clicked
}

/// A vertical 1×28 separator (`.vsep`) in a horizontal bar.
pub fn vsep(ui: &mut Ui) {
    let (rect, _) = ui.allocate_exact_size(vec2(1.0, 28.0), Sense::hover());
    ui.painter().rect_filled(rect, 0.0, theme::DIVIDER);
}

/// The faint ⓘ after a row label; hovering shows `tip`. Allocates 15×15.
pub fn info_tip(ui: &mut Ui, tip: &str) -> Response {
    let (rect, resp) = ui.allocate_exact_size(vec2(15.0, 15.0), Sense::hover());
    paint_icon(ui, rect, "info", 15.0, theme::TEXT_FAINT);
    resp.on_hover_text(tip)
}

/// A dark translucent chip for use over video (`.chip-dark`), 22 px, content-sized:
/// optional dot / icon + 10.5 px bold text.
pub fn chip_dark(ui: &mut Ui, icon: Option<&str>, dot: bool, text: &str) -> Response {
    let f = font(Weight::Bold, 10.5);
    let tw = if text.is_empty() { 0.0 } else { super::text_width(ui, text, f.clone(), 0.3) };
    let lead = if dot {
        6.0
    } else if icon.is_some() {
        13.0
    } else {
        0.0
    };
    let gap = if lead > 0.0 && tw > 0.0 { theme::S1 } else { 0.0 };
    let (rect, resp) = ui.allocate_exact_size(vec2(16.0 + lead + gap + tw, 22.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, pill_radius(22.0), theme::CHIP_DARK);
    let mut x = rect.left() + 8.0;
    if dot {
        let c = pos2(x + 3.0, rect.center().y);
        p.circle_filled(c, 5.0, theme::SUCCESS_GLOW.gamma_multiply(0.25));
        p.circle_filled(c, 3.0, theme::SUCCESS);
    } else if let Some(i) = icon {
        paint_icon(ui, Rect::from_min_size(pos2(x, rect.top()), vec2(13.0, 22.0)), i, 13.0, theme::TEXT);
    }
    x += lead + gap;
    let g = text_galley(ui, text, f, theme::TEXT, f32::INFINITY, 0.3);
    paint_galley(ui, Rect::from_x_y_ranges(x..=rect.right(), rect.y_range()), g, Align::Min);
    resp
}
