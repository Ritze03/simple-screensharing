//! Containers: card, well, band, card/tile grid, modal shell + footer. Frames only
//! paint and pad; the layout inside is always a vbox (`ui.vertical` semantics) whose
//! width is fixed to the container's inner width and clipped to it, so a too-wide row
//! is cut at the container edge instead of widening it (the Forza guide's pane rule).

use egui::{Align, Color32, CornerRadius, Frame, Id, Layout, Margin, Rect, Stroke, Ui, UiBuilder, pos2, vec2};

use super::{
    Btn, FILL, HBox, IconBtnKind, Slot, card_title, columns, debug_vbox, fixed, grid_cols_stable, icon_btn, paint_icon, paint_text,
};
use crate::theme::{self, Weight, font};

/// Run `body` as a fixed-width, clipped vbox with `gap` between items.
fn pane<R>(ui: &mut Ui, gap: f32, body: impl FnOnce(&mut Ui) -> R) -> R {
    let w = ui.available_width();
    ui.set_width(w);
    // Clip horizontally only; the vertical extent is the parent's (inside a ScrollArea
    // `max_rect` is the viewport, not the content).
    let clip = Rect::from_x_y_ranges(ui.max_rect().x_range().expand(1.0), ui.clip_rect().y_range());
    ui.shrink_clip_rect(clip);
    ui.spacing_mut().item_spacing.y = gap;
    let top = ui.cursor().min;
    let r = body(ui);
    debug_vbox(ui, Rect::from_min_max(top, pos2(top.x + w, ui.min_rect().bottom())));
    r
}

/// A settings card (`Frame · vbox (card)`): `PANEL_RAISED`, 1 px border, radius 12,
/// padding 12, 8 px between rows; first row is the accent uppercase title (+ optional
/// mono count, e.g. "2 rooms"). Fills the available width exactly.
pub fn card<R>(ui: &mut Ui, title: &str, count: Option<&str>, body: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(theme::PANEL_RAISED)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(CornerRadius::same(theme::ROUNDING_WELL))
        .inner_margin(Margin::same(theme::S3 as i8))
        .show(ui, |ui| {
            pane(ui, theme::S2, |ui| {
                card_title(ui, title, count);
                body(ui)
            })
        })
        .inner
}

/// A sidebar well (`Frame · vbox (well)`): `ELEV_16`, `WELL_BORDER`, radius 12,
/// padding 4, rows stacked with no gap.
pub fn well<R>(ui: &mut Ui, body: impl FnOnce(&mut Ui) -> R) -> R {
    Frame::new()
        .fill(theme::ELEV_16)
        .stroke(Stroke::new(1.0_f32, theme::WELL_BORDER))
        .corner_radius(CornerRadius::same(theme::ROUNDING_WELL))
        .inner_margin(Margin::same(theme::S1 as i8))
        .show(ui, |ui| pane(ui, 0.0, body))
        .inner
}

/// Which edge of a [`band`] gets the 1 px divider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BandEdge {
    None,
    /// Divider along the bottom (top bands: brand, room header, modal head).
    Bottom,
    /// Divider along the top (bottom bands: user band, action bar).
    Top,
}

/// A horizontal band (`--band-h` 56 px, full width): paints `fill` and the divider,
/// then runs `body` in the padded band (`pad_x` left/right, content vertically
/// centred, left-to-right). Build the band's content with an [`HBox`] of height ≤ 56.
pub fn band<R>(ui: &mut Ui, fill: Color32, edge: BandEdge, pad_x: f32, body: impl FnOnce(&mut Ui) -> R) -> R {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), theme::BAND_H), egui::Sense::hover());
    ui.painter().rect_filled(rect, 0.0, fill);
    let stroke = Stroke::new(1.0_f32, theme::DIVIDER);
    match edge {
        BandEdge::None => {}
        BandEdge::Bottom => {
            ui.painter().hline(rect.x_range(), rect.bottom() - 0.5, stroke);
        }
        BandEdge::Top => {
            ui.painter().hline(rect.x_range(), rect.top() + 0.5, stroke);
        }
    }
    let inner = rect.shrink2(vec2(pad_x, 0.0));
    let mut child = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
    child.shrink_clip_rect(rect);
    body(&mut child)
}

/// Equal-width card/tile grid: a vbox of hbox rows of `cols` equal fill columns, gap 12
/// both ways; the last row is padded with empty columns so every column keeps the same
/// width. `cols` comes from [`super::grid_cols`] with hysteresis (no oscillation).
/// `item(ui, i)` draws item `i` into its column (a fixed-width, clipped vbox).
pub fn grid_rows(ui: &mut Ui, id_salt: impl std::hash::Hash, n_items: usize, item: impl FnMut(&mut Ui, usize)) {
    let id = ui.make_persistent_id(id_salt);
    let cols = grid_cols_stable(ui, id, ui.available_width());
    grid_rows_with(ui, cols, n_items, item);
}

/// [`grid_rows`] with an explicit column count (e.g. the "On screen" tiles: 1 or 2).
pub fn grid_rows_with(ui: &mut Ui, cols: usize, n_items: usize, mut item: impl FnMut(&mut Ui, usize)) {
    let cols = cols.max(1);
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = theme::GRID_GAP;
        for start in (0..n_items).step_by(cols) {
            columns(ui, cols, theme::GRID_GAP, |c| {
                for (j, col) in c.iter_mut().enumerate() {
                    if start + j < n_items {
                        item(col, start + j);
                    }
                }
            });
        }
    });
}

// ---------------------------------------------------------------------------
// Modal
// ---------------------------------------------------------------------------

/// Static description of a modal.
#[derive(Clone, Copy, Debug)]
pub struct ModalSpec<'a> {
    /// Unique id (egui `Id` source).
    pub id: &'a str,
    pub title: &'a str,
    pub sub: Option<&'a str>,
    /// `theme::MODAL_W` / `MODAL_W_WIDE` / `MODAL_W_XWIDE`; capped at window − 48.
    pub width: f32,
    /// Optional pill in the head band (e.g. "Applying"), in a slot of max(96, its width).
    pub head_pill: Option<super::Pill<'a>>,
}

/// What the user did to a modal's chrome.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModalAction {
    /// × clicked, Escape pressed, or backdrop clicked ([`modal_shell`] only).
    pub close: bool,
}

/// The modal's chrome drawn into `ui` (no backdrop / window): head band
/// `[fill vbox(title, sub)][pill? max(96, its width)][32 ×]` 56 px, a vertical `ScrollArea` body
/// (padding 16/20, 12 px between items), and a 64 px footer band (padding 20) in which
/// `footer` runs — use [`modal_footer`] there. `max_h` caps the total height.
pub fn modal_frame(ui: &mut Ui, spec: &ModalSpec, max_h: f32, body: impl FnOnce(&mut Ui), footer: impl FnOnce(&mut Ui)) -> ModalAction {
    let w = spec.width.min(ui.available_width());
    let mut action = ModalAction::default();
    Frame::new()
        .fill(theme::PANEL_BASE)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(CornerRadius::same(theme::ROUNDING))
        .show(ui, |ui| {
            ui.set_width(w - 2.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            // Head band.
            // `Why` measured: a fixed 96 slot clips a pill wider than that
            // (the bar's 84 clipped "CONNECTING", 2026-10-15).
            let pill_w = spec.head_pill.map_or(96.0, |p| p.width(ui).max(96.0));
            let slots_pill = [FILL, fixed(pill_w), fixed(theme::CTL_H)];
            let slots_plain = [FILL, fixed(theme::CTL_H)];
            let slots: &[Slot] = if spec.head_pill.is_some() { &slots_pill } else { &slots_plain };
            let head = HBox::new("modal_head", theme::BAND_H).pad(20.0, theme::S3).gap(theme::S3).slots(slots).show(ui);
            ui.painter().hline(head.rect.x_range(), head.rect.bottom() - 0.5, Stroke::new(1.0_f32, theme::DIVIDER));
            let c = head.cell(0);
            if let Some(sub) = spec.sub {
                let t = Rect::from_x_y_ranges(c.x_range(), c.center().y - 18.0..=c.center().y + 2.0);
                let s = Rect::from_x_y_ranges(c.x_range(), c.center().y + 2.0..=c.center().y + 18.0);
                paint_text(ui, t, spec.title, font(Weight::Bold, theme::FS_H3), theme::TEXT, Align::Min);
                paint_text(ui, s, sub, font(Weight::Regular, theme::FS_SMALL), theme::TEXT_DIM, Align::Min);
            } else {
                paint_text(ui, c, spec.title, font(Weight::Bold, theme::FS_H3), theme::TEXT, Align::Min);
            }
            if let Some(p) = spec.head_pill {
                let pc = head.cell(1);
                let pw = p.width(ui).min(pc.width());
                let ph = if p.large { theme::PILL_H_LG } else { theme::PILL_H };
                p.paint(ui, Rect::from_min_size(pos2(pc.right() - pw, pc.center().y - ph / 2.0), vec2(pw, ph)));
            }
            let close_i = slots.len() - 1;
            let close_tip = crate::ui::tr("ui.modal.close", "Close");
            if head.show(ui, close_i, Align::Center, |ui| icon_btn(ui, "close", IconBtnKind::Plain, &close_tip)).clicked() {
                action.close = true;
            }
            // Body.
            let body_max = (max_h - theme::BAND_H - theme::MODAL_FOOT_H).max(80.0);
            // `min_scrolled_height`: never let a parent's small remaining height squeeze the
            // body (e.g. a modal frame drawn inline inside another ScrollArea).
            egui::ScrollArea::vertical().max_height(body_max).min_scrolled_height(body_max).auto_shrink([false, true]).show(ui, |ui| {
                Frame::new().inner_margin(Margin::symmetric(20, theme::S4 as i8)).show(ui, |ui| {
                    pane(ui, theme::S3, body);
                });
            });
            // Footer band.
            let (frect, _) = ui.allocate_exact_size(vec2(ui.available_width(), theme::MODAL_FOOT_H), egui::Sense::hover());
            ui.painter().hline(frect.x_range(), frect.top() + 0.5, Stroke::new(1.0_f32, theme::DIVIDER));
            let inner = Rect::from_center_size(frect.center(), vec2(frect.width() - 40.0, theme::CTL_H_LG));
            let mut child = ui.new_child(UiBuilder::new().max_rect(inner).layout(Layout::left_to_right(Align::Center)));
            footer(&mut child);
        });
    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
        action.close = true;
    }
    action
}

/// A real modal: scrim backdrop (`SCRIM`) + centred [`modal_frame`] with the modal
/// shadow. Width is `spec.width` capped at window − 48, height at window − 48.
pub fn modal_shell(ctx: &egui::Context, spec: &ModalSpec, body: impl FnOnce(&mut Ui), footer: impl FnOnce(&mut Ui)) -> ModalAction {
    let screen = ctx.content_rect();
    let max_w = (screen.width() - 48.0).max(200.0);
    let max_h = (screen.height() - 48.0).max(200.0);
    let spec = ModalSpec { width: spec.width.min(max_w), ..*spec };
    let frame = Frame::new().corner_radius(CornerRadius::same(theme::ROUNDING)).shadow(egui::Shadow {
        offset: [0, 26],
        blur: 70,
        spread: 0,
        color: Color32::from_black_alpha(178),
    });
    let resp = egui::Modal::new(Id::new(spec.id)).frame(frame).backdrop_color(theme::SCRIM).show(ctx, |ui| {
        ui.set_max_width(spec.width);
        modal_frame(ui, &spec, max_h, body, footer)
    });
    let mut a = resp.inner;
    if resp.should_close() {
        a.close = true;
    }
    a
}

/// What was clicked in a [`modal_footer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FooterClick {
    None,
    Leading(usize),
    Trailing(usize),
}

/// Modal footer row: `[leading btns…][fill spacer][note?][trailing btns…]`, 36 px.
/// Buttons are at least 96 wide (`.modal-foot .btn`); `note` is the "✓ Saved" text.
pub fn modal_footer(ui: &mut Ui, leading: &[Btn], note: Option<&str>, trailing: &[Btn]) -> FooterClick {
    let bw = |b: &Btn, ui: &Ui| b.large().width(ui).max(96.0);
    let note_w = note.map(|n| 16.0 + theme::S1 + super::text_width(ui, n, font(Weight::Regular, theme::FS_SMALL), 0.0).ceil());
    let mut slots: Vec<Slot> = leading.iter().map(|b| fixed(bw(b, ui))).collect();
    slots.push(FILL);
    if let Some(w) = note_w {
        slots.push(fixed(w));
    }
    slots.extend(trailing.iter().map(|b| fixed(bw(b, ui))));
    let r = HBox::new("modal_footer", theme::CTL_H_LG).slots(&slots).show(ui);
    let mut click = FooterClick::None;
    for (i, b) in leading.iter().enumerate() {
        let w = r.cell(i).width();
        if r.show(ui, i, Align::Min, |ui| b.large().show_sized(ui, w)).clicked() {
            click = FooterClick::Leading(i);
        }
    }
    let mut k = leading.len() + 1;
    if let Some(n) = note {
        let c = r.cell(k);
        paint_icon(ui, Rect::from_min_size(c.min, vec2(16.0, c.height())), "check_circle", 16.0, theme::SUCCESS);
        let tr = Rect::from_x_y_ranges(c.left() + 16.0 + theme::S1..=c.right(), c.y_range());
        paint_text(ui, tr, n, font(Weight::Regular, theme::FS_SMALL), theme::TEXT_DIM, Align::Min);
        k += 1;
    }
    for (i, b) in trailing.iter().enumerate() {
        let w = r.cell(k + i).width();
        if r.show(ui, k + i, Align::Min, |ui| b.large().show_sized(ui, w)).clicked() {
            click = FooterClick::Trailing(i);
        }
    }
    click
}
