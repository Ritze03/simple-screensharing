//! The app's own menu style, for the parts the popover rows of `widgets` (`pop_item`) do not cover:
//! [`select`] (a dropdown field that opens one of our menus) and [`switch_row`] (a menu row with a switch).
//!
//! *Why not `egui::ComboBox` (what `widgets::select` is):* egui draws its own list (plain selectable labels,
//! its own frame, no icons and no second line). The user's rule (task I3, D1) is that every dropdown looks like
//! the app's menus: the popover frame, `pop_item` rows with icon and sub-line, opening under the field and as
//! wide as it. Every dropdown field in the app, the Stream settings modal's included, goes through [`select`].

use egui::{Align, CornerRadius, Frame, Layout, Margin, Popup, PopupCloseBehavior, PopupKind, Rect, RectAlign, Sense, Stroke, Ui, UiBuilder, vec2};

use crate::theme::{self, Weight, font};
use crate::widgets::{self, FILL, HBox, fixed};

/// One choice of a [`select`]: label, an optional second line and an optional icon.
pub struct Opt<'a> {
    pub label: &'a str,
    pub sub: Option<&'a str>,
    pub icon: Option<&'a str>,
}

impl<'a> Opt<'a> {
    pub fn new(label: &'a str) -> Self {
        Self { label, sub: None, icon: None }
    }
}

/// The popover frame (`ui::popover`): `PANEL_RAISED`, border, radius 10, padding 6, shadow.
pub fn frame() -> Frame {
    Frame::new()
        .fill(theme::PANEL_RAISED)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(6))
        .shadow(egui::Shadow { offset: [0, 14], blur: 40, spread: 0, color: egui::Color32::from_black_alpha(153) })
}

/// A dropdown field (`select.field`): fills the available width, 32 px, `FIELD` fill. Click opens the app's
/// menu under it, as wide as the field, one `pop_item` row per option (icon and sub-line when the option has
/// them, the current one highlighted). Sets `*selected` to the picked index and marks the response changed.
/// `id_salt` must be unique among selects in the same parent `Ui`.
pub fn select(ui: &mut Ui, id_salt: impl std::hash::Hash, selected: &mut usize, opts: &[Opt]) -> egui::Response {
    let id = ui.make_persistent_id(id_salt);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), theme::CTL_H), Sense::hover());
    let mut resp = ui.interact(rect, id, Sense::click());
    let popup_id = Popup::default_response_id(&resp);
    let open = Popup::is_id_open(ui.ctx(), popup_id);
    let current = opts.get(*selected);
    let stroke = if open {
        theme::ACCENT
    } else if resp.hovered() {
        theme::BTN_BORDER
    } else {
        theme::BORDER
    };
    ui.painter().rect(rect, CornerRadius::same(theme::ROUNDING_SM), theme::FIELD, Stroke::new(1.0_f32, stroke), egui::StrokeKind::Inside);
    let slots = if current.is_some_and(|o| o.icon.is_some()) { vec![fixed(16.0), FILL, fixed(18.0)] } else { vec![FILL, fixed(18.0)] };
    let row = HBox::new("select_field", theme::CTL_H).pad(10.0, 8.0).slots(&slots).width(rect.width()).show(&mut ui.new_child(UiBuilder::new().max_rect(rect)));
    let n = slots.len();
    if let Some(o) = current {
        if let Some(icon) = o.icon {
            row.icon(ui, 0, icon, 16.0, theme::TEXT_DIM);
        }
        row.text(ui, n - 2, o.label, font(Weight::Regular, theme::FS_ROW), theme::TEXT, Align::Min);
    }
    row.icon(ui, n - 1, if open { "expand_less" } else { "expand_more" }, 18.0, theme::TEXT_DIM);
    resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);

    let mut picked = None;
    let w = rect.width();
    Popup::from_toggle_button_response(&resp)
        .id(popup_id)
        .kind(PopupKind::Menu)
        .layout(Layout::top_down_justified(Align::Min))
        .close_behavior(PopupCloseBehavior::CloseOnClick)
        .align(RectAlign::BOTTOM_START)
        .gap(4.0)
        .width(w)
        .frame(frame())
        .show(|ui| {
            ui.set_width(w - 12.0);
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical().max_height(320.0).auto_shrink([false, true]).show(ui, |ui| {
                for (i, o) in opts.iter().enumerate() {
                    if option_row(ui, o, i == *selected).clicked() {
                        picked = Some(i);
                    }
                }
            });
        });
    if let Some(i) = picked
        && i != *selected
    {
        *selected = i;
        resp.mark_changed();
    }
    resp
}

/// One option of a [`select`]: a `widgets::pop_item` row (32 px, 44 with a sub-line, the current one highlighted)
/// whose icon is optional (`pop_item` insists on one).
fn option_row(ui: &mut Ui, o: &Opt, selected: bool) -> egui::Response {
    let h = if o.sub.is_some() { theme::POP_ITEM_TWO_H } else { theme::ROW_H };
    let slots = if o.icon.is_some() { vec![fixed(18.0), FILL] } else { vec![FILL] };
    let row = HBox::new("select_option", h).pad(theme::S2, theme::S2).slots(&slots).sense(Sense::click()).show(ui);
    if selected {
        ui.painter().rect_filled(row.rect, CornerRadius::same(7), theme::ACCENT_ROW_SELECTED);
    } else if row.response.hovered() {
        ui.painter().rect_filled(row.rect, CornerRadius::same(7), theme::WASH_HOVER_2);
    }
    if let Some(icon) = o.icon {
        row.icon(ui, 0, icon, 16.0, theme::TEXT_DIM);
    }
    let last = slots.len() - 1;
    let cell = row.cell(last);
    match o.sub {
        None => row.text(ui, last, o.label, font(Weight::Regular, theme::FS_LABEL), theme::TEXT, Align::Min),
        Some(sub) => {
            let mid = cell.center().y;
            widgets::paint_text(ui, Rect::from_x_y_ranges(cell.x_range(), mid - 17.0..=mid), o.label, font(Weight::Regular, theme::FS_LABEL), theme::TEXT, Align::Min);
            widgets::paint_text(ui, Rect::from_x_y_ranges(cell.x_range(), mid..=mid + 15.0), sub, font(Weight::Regular, theme::FS_TINY), theme::TEXT_FAINT, Align::Min);
        }
    }
    row.response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A menu row `[18 icon][label][switch]` (`.mi` with `.sw`), 32 px, the whole row toggles `*on`. Returns true
/// on the frame it was toggled.
pub fn switch_row(ui: &mut Ui, icon: &str, label: &str, on: &mut bool) -> bool {
    let row = HBox::new("menu_switch", theme::ROW_H).pad(theme::S2, theme::S2).slots(&[fixed(18.0), FILL, fixed(30.0)]).sense(Sense::click()).show(ui);
    if row.response.hovered() {
        ui.painter().rect_filled(row.rect, CornerRadius::same(7), theme::WASH_HOVER_2);
    }
    row.icon(ui, 0, icon, 16.0, theme::TEXT_DIM);
    row.text(ui, 1, label, font(Weight::Regular, theme::FS_LABEL), theme::TEXT, Align::Min);
    let sw = egui::Rect::from_center_size(row.cell(2).center(), vec2(30.0, 18.0));
    ui.painter().rect_filled(sw, CornerRadius::same(9), if *on { theme::ACCENT } else { theme::BTN_BORDER });
    let knob_x = if *on { sw.right() - 9.0 } else { sw.left() + 9.0 };
    ui.painter().circle_filled(egui::pos2(knob_x, sw.center().y), 7.0, theme::TEXT);
    let clicked = row.response.on_hover_cursor(egui::CursorIcon::PointingHand).clicked();
    if clicked {
        *on = !*on;
    }
    clicked
}

/// Whether the app has a dropdown open (a [`select`] list): the stage's fullscreen chrome keeps still while
/// one is, like for any menu.
pub fn any_select_open(ctx: &egui::Context) -> bool {
    Popup::is_any_open(ctx)
}
