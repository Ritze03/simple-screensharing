//! Central egui theme: named color tokens, spacing/rounding constants, and
//! widget helpers (cards, buttons, checkboxes/radios, two-column control
//! rows), so `ui.rs` never hard-codes a raw `Color32` or hand-rolls a card
//! frame at a call site. See `superdoc/ui/STYLING-GUIDE.md` for the rules
//! this module exists to satisfy — this is that guide's `theme.rs` target.
//!
//! Every color is referenced by *role* (`ACCENT`, `DANGER`, …), not by where
//! it happens to be used, so a future re-theme or accent-color change is a
//! one-file edit instead of a grep-and-replace across the UI.

use egui::{Color32, CornerRadius, RichText, Stroke, Ui, Vec2, Widget};

// ---------------------------------------------------------------------------
// Color tokens
// ---------------------------------------------------------------------------

/// Brand accent — section titles, primary buttons, slider defaults,
/// selection tint, and (deliberately) non-destructive informational
/// highlights like the "popped out" stream tag: it's the same "this is the
/// thing to notice" role in every case, just applied to different content.
pub const ACCENT: Color32 = Color32::from_rgb(0x5B, 0x8B, 0xF0);
/// Main panel / window background.
pub const PANEL: Color32 = Color32::from_rgb(0x1E, 0x1F, 0x22);
/// Hairline borders and dividers.
pub const BORDER: Color32 = Color32::from_rgb(0x3A, 0x3B, 0x40);
/// Primary text.
pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xE9, 0xEC);
/// Secondary text.
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xA8, 0xAA, 0xB2);
/// Tertiary text — hints, placeholders, "offline"/disabled labels.
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x6E, 0x70, 0x78);
/// Input and code-block backgrounds.
pub const FIELD: Color32 = Color32::from_rgb(0x28, 0x29, 0x2D);
/// Secondary button fill.
pub const BTN: Color32 = Color32::from_rgb(0x2C, 0x2D, 0x32);
/// Secondary button border.
pub const BTN_BORDER: Color32 = Color32::from_rgb(0x44, 0x45, 0x4C);
/// Text drawn on top of an accent-filled primary button.
pub const PRIMARY_TEXT: Color32 = Color32::from_rgb(0x0B, 0x0C, 0x10);
/// Destructive-action red — danger button text/border, error messages.
pub const DANGER: Color32 = Color32::from_rgb(0xE0, 0x5A, 0x5A);
/// Positive/active-state green — streaming indicator dot, "Saved.",
/// "Viewing"/"Listening" tags.
///
/// *Why a new token:* STYLING-GUIDE.md's base palette and its "Severity
/// colors" section only cover *bad*-state colors (deriving from
/// `DANGER`/`ACCENT`); it has no equivalent role for a *good*-state
/// indicator, which this app needs in several places. Adding one named
/// constant here (rather than a scattered raw green literal) keeps the
/// "reference by role" rule intact — see this file's note added to
/// STYLING-GUIDE.md itself.
pub const SUCCESS: Color32 = Color32::from_rgb(0x50, 0xC8, 0x78);

/// Standard gap between stacked cards / side-by-side columns, and each
/// card's own inner margin — one number, used everywhere, per
/// STYLING-GUIDE.md's "Consistent spacing".
pub const SPACING: f32 = 7.0;
/// Standard corner rounding for cards and buttons.
pub const ROUNDING: u8 = 8;
/// Fixed width reserved for a control row's trailing value spinner/preview
/// (see STYLING-GUIDE.md's "Reserved value-width"), sized to the widest
/// value this app's rows ever show (e.g. `"40000 kbps"`, `"240 fps"`).
pub const VALUE_W: f32 = 64.0;
/// Max content width for a single-column, capped-width settings-style panel
/// (see STYLING-GUIDE.md's "Layout options for a settings-style view").
pub const PANEL_MAX_WIDTH: f32 = 520.0;

/// Blend `base` toward transparent for a translucent fill/stroke (hover
/// washes, selection tints, …) through `egui::Rgba`'s gamma-correct
/// conversion, rather than calling `Color32::from_rgba_unmultiplied`
/// directly at the call site.
///
/// *Why:* between egui 0.29 and 0.33, `from_rgba_unmultiplied`'s blend
/// semantics changed from gamma-correct to a naive per-channel multiply, so
/// a translucent tint tuned by eye against one behavior can go nearly
/// invisible under the other. Routing every translucent color through this
/// one helper makes a future egui upgrade a one-place fix instead of a
/// UI-wide "why did the tints get fainter" hunt. See
/// STYLING-GUIDE.md's "egui version footguns".
pub fn tint(base: Color32, alpha: u8) -> Color32 {
    let a = alpha as f32 / 255.0;
    let linear = egui::Rgba::from(base);
    Color32::from(egui::Rgba::from_rgba_premultiplied(linear.r() * a, linear.g() * a, linear.b() * a, a))
}

// ---------------------------------------------------------------------------
// Cards / sections
// ---------------------------------------------------------------------------

/// An uppercase, small, bold, `ACCENT`-colored section title — usable inside
/// a `card` or on its own (e.g. above a live-view panel) so both share
/// exactly the same title styling.
pub fn section_label(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text.to_uppercase()).small().strong().color(ACCENT));
}

/// A bordered, rounded rectangle with an uppercase `ACCENT` title, grouping
/// related controls — the basic unit for settings/status displays. Owns its
/// own trailing gap (`SPACING`), so callers stacking cards should zero the
/// container's own item spacing rather than also `add_space`-ing between
/// them (see STYLING-GUIDE.md's "Watch out for doubled gaps").
pub fn card(ui: &mut Ui, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(PANEL)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(CornerRadius::same(ROUNDING))
        .inner_margin(SPACING)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = SPACING;
            section_label(ui, title);
            add_contents(ui);
        });
    ui.add_space(SPACING);
}

// ---------------------------------------------------------------------------
// Control rows
// ---------------------------------------------------------------------------

/// Left-column label for a two-column control row built with `ui.columns`.
///
/// `ui.columns` top-aligns each column by default, so a plain `ui.label`
/// sits at the row's top edge while a taller control beside it renders
/// vertically centered. This allocates the label a full standard control
/// height and centers it, so the label + control read as one aligned band.
pub fn row_label(ui: &mut Ui, text: &str) {
    let height = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), height), egui::Layout::left_to_right(egui::Align::Center), |ui| {
        ui.label(RichText::new(text).color(TEXT));
    });
}

/// A labeled slider row: label in the left half; in the right half, a
/// slider filling available space followed by a reserved, fixed-width
/// `DragValue` (see `VALUE_W`) so the row's right edge never jitters as the
/// value's digit count changes, and so the exact value can still be typed
/// directly rather than only dragged.
///
/// Returns whether `*value` changed this frame (either control edited it),
/// mirroring `Response::changed()` — callers that only need to react to a
/// real edit (e.g. pushing a new volume to a decoder) shouldn't have to
/// re-derive that from a raw egui `Response` themselves.
pub fn slider_row(ui: &mut Ui, label: &str, value: &mut u32, range: std::ops::RangeInclusive<u32>, speed: f64, suffix: &str) -> bool {
    let mut changed = false;
    ui.columns(2, |columns| {
        row_label(&mut columns[0], label);
        columns[1].horizontal(|ui| {
            let slider_width = (ui.available_width() - VALUE_W - ui.spacing().item_spacing.x).max(1.0);
            let height = ui.spacing().interact_size.y;
            let slider_response = ui.add_sized([slider_width, height], egui::Slider::new(value, range).show_value(false));
            let drag_response = ui.add_sized([VALUE_W, height], egui::DragValue::new(value).speed(speed).suffix(suffix));
            changed = slider_response.changed() || drag_response.changed();
        });
    });
    changed
}

/// A labeled row whose right half is a `ComboBox` (or any other
/// content-driven control) rather than a slider — same two-column shape as
/// `slider_row`, for non-numeric settings like the presentation-mode or
/// framerate dropdowns.
pub fn setting_row(ui: &mut Ui, label: &str, add_control: impl FnOnce(&mut Ui)) {
    ui.columns(2, |columns| {
        row_label(&mut columns[0], label);
        columns[1].horizontal(|ui| add_control(ui));
    });
}

// Note: STYLING-GUIDE.md also specifies a shared styled checkbox/radio
// widget family (`styled_checkbox`/`checkbox_row`/`styled_radio`). This app
// has no checkbox or radio control anywhere yet (nothing in `ui.rs` needed
// one even after this conformance pass), so that family isn't implemented
// here — add it, following the guide's documented API shape, when the first
// real checkbox/radio need shows up, rather than carrying unused widget code
// speculatively.

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

fn styled_button(text: &str, fill: Color32, text_color: Color32, border: Option<Color32>, strong: bool) -> egui::Button<'static> {
    let mut rich = RichText::new(text.to_owned()).color(text_color);
    if strong {
        rich = rich.strong();
    }
    let mut button = egui::Button::new(rich).fill(fill).corner_radius(CornerRadius::same(ROUNDING));
    if let Some(border) = border {
        button = button.stroke(Stroke::new(1.0, border));
    }
    button
}

/// Solid `ACCENT` fill, `PRIMARY_TEXT` text, bold — the one primary action
/// on a screen (e.g. "Join", "Start sharing").
pub fn primary_button(text: impl Into<String>) -> impl Widget {
    styled_button(&text.into(), ACCENT, PRIMARY_TEXT, None, true)
}

/// Neutral `BTN` fill with a `BTN_BORDER` border — the default for
/// everything else (Cancel-that-isn't-destructive, Reload, Save).
pub fn secondary_button(text: impl Into<String>) -> impl Widget {
    styled_button(&text.into(), BTN, TEXT, Some(BTN_BORDER), false)
}

/// Transparent fill, `DANGER`-colored text and a faint `DANGER` border —
/// destructive/abort actions (Remove, Disconnect, Stop Sharing).
pub fn danger_button(text: impl Into<String>) -> impl Widget {
    styled_button(&text.into(), Color32::TRANSPARENT, DANGER, Some(tint(DANGER, 140)), false)
}
