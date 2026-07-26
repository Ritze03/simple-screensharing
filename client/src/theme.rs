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
///
/// *Why this value (2026-07-26 redesign pass):* the original `0x5B8BF0` read
/// as flat and low-saturation for the Discord/Slack-style look this app is
/// moving toward (see `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`'s
/// UX/Design-lens critique). `0x4C6EF5` sits in the same saturated
/// blue-violet neighborhood as reference points like Discord's blurple
/// without copying that exact brand color — same character, this app's own
/// hue.
pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x6E, 0xF5);
/// Base-tier panel / window background — the middle of three elevation
/// tiers (`PANEL_SUNKEN` < `PANEL_BASE` < `PANEL_RAISED`), used for the
/// general app/window background level. See "Elevation tiers" below.
pub const PANEL_BASE: Color32 = Color32::from_rgb(0x1E, 0x1F, 0x22);
/// Recessed-tier surface — sits visually *below* `PANEL_BASE`: sidebar
/// recesses, scroll wells, and the letterboxed background behind a
/// chrome-free video tile (see `media_frame`). See "Elevation tiers" below.
pub const PANEL_SUNKEN: Color32 = Color32::from_rgb(0x17, 0x18, 0x1B);
/// Raised-tier surface — sits visually *above* `PANEL_BASE`: cards and
/// other content that should read as floating above the base background.
/// `card()` fills with this tier. See "Elevation tiers" below.
pub const PANEL_RAISED: Color32 = Color32::from_rgb(0x27, 0x28, 0x2D);
/// Deprecated alias for `PANEL_BASE`, kept only so any external reference
/// written against the pre-elevation single-tier palette keeps compiling.
/// Prefer `PANEL_BASE` (or `PANEL_SUNKEN`/`PANEL_RAISED`) in new code — see
/// "Elevation tiers" below for which tier to reach for.
#[deprecated(note = "use PANEL_BASE, PANEL_SUNKEN, or PANEL_RAISED instead")]
pub const PANEL: Color32 = PANEL_BASE;
/// Hairline borders and dividers. Still useful alongside the elevation
/// tiers above (e.g. a hairline between two same-tier surfaces), but no
/// longer the *only* depth cue — most surface-to-surface contrast should
/// come from picking a different `PANEL_*` tier first.
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
/// Caution/attention amber — distinct from both `DANGER` (red) and
/// `SUCCESS` (green): overshoot/approaching-a-limit states that are worth
/// noticing but aren't failures. First consumer: the bandwidth-overshoot
/// warning banner (`superdoc/brainstorm/ui-redesign-multiroom-streaming.md`),
/// added here ahead of that banner landing so the token exists when it's
/// needed. Follows the same "one named `pub const`, reference by role"
/// pattern `DANGER`/`SUCCESS` already use — no separate derived
/// background/border constants, since neither of those has any either
/// (call sites route through `tint()` as needed).
pub const WARNING: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);

/// Standard gap between stacked cards / side-by-side columns, and each
/// card's own inner margin — one number, used everywhere, per
/// STYLING-GUIDE.md's "Consistent spacing".
pub const SPACING: f32 = 7.0;
/// Standard corner rounding for cards, buttons, and media tiles — the
/// larger of two rounding tiers.
///
/// *Why 14, not 8:* the original `8` read as too subtle for the
/// Discord/Slack-style "chunky" feel this app is moving toward (see
/// `superdoc/brainstorm/ui-redesign-multiroom-streaming.md`'s UX/Design-lens
/// critique, which called for 12-16px). Landed in the middle of that range.
pub const ROUNDING: u8 = 14;
/// Smaller rounding tier for compact, dense elements — sidebar list rows,
/// pills — where the full `ROUNDING` would look oversized relative to the
/// element's own height. Matches the previous single-tier `ROUNDING` value,
/// so anything already tuned against the old `8` (e.g. compact row mockups)
/// stays visually consistent.
pub const ROUNDING_SM: u8 = 8;
/// Fixed width reserved for a control row's trailing value spinner/preview
/// (see STYLING-GUIDE.md's "Reserved value-width"), sized to the widest
/// value this app's rows ever show (e.g. `"40000 kbps"`, `"240 fps"`).
pub const VALUE_W: f32 = 64.0;
/// Height of a nav-rail pill (`nav_button`) — deliberately much taller than
/// a form button's default `interact_size.y`, see that function's docs.
pub const NAV_BUTTON_HEIGHT: f32 = 38.0;
/// Diameter of a `status_dot`. Small enough to read as an indicator rather
/// than a bullet, large enough to still read at a glance in a dense list.
pub const DOT_SIZE: f32 = 9.0;
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
// Global style
// ---------------------------------------------------------------------------

/// Push this theme's tokens into egui's *own* baseline visuals, once per
/// process (`ui.rs` calls this on its first frame).
///
/// `Why` this exists on top of the per-widget helpers: the helpers only cover
/// the widgets this app builds through them. Everything egui draws for itself
/// — combo-box popups, scroll bars, tooltips, text-edit fields, modal
/// backdrops — otherwise keeps egui's stock dark palette, which is a
/// *different* dark grey than `PANEL_*` and reads as an unstyled patch sitting
/// on top of a themed app. Setting the baseline once is what makes the whole
/// window belong to one palette instead of two.
pub fn install(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = PANEL_BASE;
    visuals.window_fill = PANEL_RAISED;
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.window_corner_radius = CornerRadius::same(ROUNDING);
    visuals.extreme_bg_color = FIELD;
    visuals.faint_bg_color = PANEL_SUNKEN;
    visuals.override_text_color = Some(TEXT);
    visuals.hyperlink_color = ACCENT;
    visuals.selection.bg_fill = tint(ACCENT, 120);
    visuals.selection.stroke = Stroke::new(1.0, TEXT);

    for widget in [
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = CornerRadius::same(ROUNDING_SM);
        widget.bg_stroke = Stroke::new(1.0, BORDER);
    }
    visuals.widgets.noninteractive.bg_fill = PANEL_BASE;
    visuals.widgets.inactive.bg_fill = BTN;
    visuals.widgets.inactive.weak_bg_fill = BTN;
    visuals.widgets.hovered.bg_fill = tint(ACCENT, 60);
    visuals.widgets.hovered.weak_bg_fill = tint(ACCENT, 60);
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, BTN_BORDER);
    visuals.widgets.active.bg_fill = tint(ACCENT, 110);
    visuals.widgets.active.weak_bg_fill = tint(ACCENT, 110);
    visuals.widgets.open.bg_fill = FIELD;
    visuals.widgets.open.weak_bg_fill = FIELD;

    ctx.set_visuals(visuals);
    ctx.style_mut(|style| {
        style.spacing.item_spacing = Vec2::splat(SPACING);
    });
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
///
/// Fills with `PANEL_RAISED` (not the old flat `PANEL`) so a card reads as
/// sitting a tier *above* the base app background — see "Elevation tiers"
/// on the `PANEL_*` tokens above. This is the card/form/settings treatment;
/// for a video tile or other live-media surface, use `media_frame` instead
/// — never wrap media in `card()`, the plan explicitly calls for chrome-free
/// media (no border, no title, edge-to-edge).
pub fn card(ui: &mut Ui, title: &str, add_contents: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(PANEL_RAISED)
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

/// Chrome-free frame for a video tile or other live-media surface: no
/// border/stroke, no title, no inner margin — media renders edge-to-edge
/// inside the tile, unlike `card()`'s bordered-panel-with-title treatment.
///
/// *Why this exists:* the UI redesign plan
/// (`superdoc/brainstorm/ui-redesign-multiroom-streaming.md`) calls for
/// video tiles to "drop card chrome entirely" — a bordered, titled `card()`
/// wrapper around a video texture would read as a settings widget, not a
/// piece of media. `media_frame` is the obvious, correct primitive for that
/// surface so future tiled-viewing-pane code doesn't reach for `card()` by
/// habit and accidentally chrome-wrap a video tile.
///
/// Fills with `PANEL_SUNKEN` as a letterbox/backdrop color, visible behind
/// the media wherever its aspect ratio doesn't fill the tile (e.g.
/// pillarboxing a portrait capture), and keeps `ROUNDING` for the tile's
/// own corner shape — a Discord/Slack-style tiled grid still wants rounded
/// tile corners, just no border/title framing them. Note: `Frame` does not
/// itself clip children to the rounded rect, so a caller drawing a texture
/// that must not show square corners under the rounding should also round
/// the image draw (e.g. `egui::Image::corner_radius(…)`), not rely on this
/// frame alone.
pub fn media_frame(ui: &mut Ui, add_contents: impl FnOnce(&mut Ui)) {
    egui::Frame::new()
        .fill(PANEL_SUNKEN)
        .corner_radius(CornerRadius::same(ROUNDING))
        .inner_margin(0)
        .show(ui, |ui| {
            add_contents(ui);
        });
}

/// An inline amber banner for a condition the user should know about but
/// which isn't an error: a tinted, `WARNING`-stroked block with the message
/// inside it.
///
/// *Why its own primitive rather than a `colored_label`:* the plan doc
/// specifies the bandwidth-overshoot notice as a *banner* — something with
/// enough visual weight to be noticed above a section, not a line of amber
/// text lost among controls — and it's the first of a family (a stalled
/// destination, a degraded encoder fallback) that must all look the same.
/// `tint(WARNING, …)` for the fill keeps it legible on `PANEL_RAISED` without
/// introducing another colour token.
pub fn warning_banner(ui: &mut Ui, message: &str) {
    egui::Frame::new()
        .fill(tint(WARNING, 28))
        .stroke(Stroke::new(1.0, WARNING))
        .corner_radius(CornerRadius::same(ROUNDING_SM))
        .inner_margin(SPACING)
        .show(ui, |ui| {
            ui.label(RichText::new(message).color(WARNING).strong());
        });
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

/// A chunky, full-width **nav-rail pill** — the sidebar's Stream Control /
/// View / Settings switcher. Accent-filled when `active`, a faint hover-only
/// surface when not.
///
/// *Why a fourth button variant, distinct from `primary_button`:* the three
/// variants above are *form* buttons — they sit inside a card at egui's
/// default `interact_size` height and are sized to their label. A nav rail
/// wants the opposite: a fixed, deliberately oversized target that spans the
/// sidebar's full width and reads as a persistent piece of chrome rather than
/// an action you press once. Styling that inline at each call site (fill by
/// active-state, `NAV_BUTTON_HEIGHT`, full-width sizing) would put three
/// hand-tuned copies of the same look in `ui.rs`, which is exactly what
/// STYLING-GUIDE.md's "expose each as a `theme::*_button` helper" rule exists
/// to prevent.
///
/// The caller still owns the *width*, since only the caller knows the rail's
/// measure — use `ui.add_sized([ui.available_width(),
/// theme::NAV_BUTTON_HEIGHT], theme::nav_button(label, active))`.
pub fn nav_button(text: impl Into<String>, active: bool) -> impl Widget {
    let (fill, text_color) = if active {
        (ACCENT, PRIMARY_TEXT)
    } else {
        // Transparent rather than `BTN`: an inactive rail item should read as
        // part of the sidebar surface, only lifting on hover (egui's own
        // hover visuals supply that), so the one accent-filled pill is
        // unambiguously "where you are".
        (Color32::TRANSPARENT, TEXT_DIM)
    };
    egui::Button::new(RichText::new(text.into()).strong().color(text_color))
        .fill(fill)
        // Explicitly strokeless in both states, rather than inheriting the
        // hairline `install()` gives every other widget: an outlined inactive
        // pill competes with the accent-filled active one, and outlining the
        // active pill too would double up on a fill that is already the
        // loudest thing in the rail.
        .stroke(Stroke::NONE)
        .corner_radius(CornerRadius::same(ROUNDING))
        .min_size(Vec2::new(0.0, NAV_BUTTON_HEIGHT))
}

/// A compact, chrome-free icon-only button for dense list rows (a room row's
/// watch / leave / forget affordances). Transparent fill, no border, dim text
/// that egui's own hover visuals lift.
///
/// *Why not `secondary_button`:* a bordered, padded button per stream row
/// would dominate a sidebar row that is mostly label — the round-1 UX note
/// asked for "icon-only buttons (denser than text buttons)" precisely because
/// several of these stack inside one expanded room.
pub fn icon_button(glyph: impl Into<String>) -> impl Widget {
    egui::Button::new(RichText::new(glyph.into()).color(TEXT_DIM))
        .fill(Color32::TRANSPARENT)
        .corner_radius(CornerRadius::same(ROUNDING_SM))
        .frame(false)
}

// ---------------------------------------------------------------------------
// Indicators
// ---------------------------------------------------------------------------

/// A small round status indicator — the sidebar's per-room live/offline/
/// reconnecting dot. `filled` draws a solid disc (a live, connected state);
/// otherwise a hollow ring (saved but not connected), mirroring the
/// `●`/`○` pair the roster already uses.
///
/// Painted rather than rendered as a `●`/`○` glyph because the reconnecting
/// state animates its *alpha* (a pulse): a colored `RichText` glyph can carry
/// a per-frame color too, but a painted disc also gives an exact, font-
/// independent size (`DOT_SIZE`) so every row's label starts at the same x
/// regardless of which state its dot is in.
pub fn status_dot(ui: &mut Ui, color: Color32, filled: bool) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(DOT_SIZE), egui::Sense::hover());
    let center = rect.center();
    let radius = DOT_SIZE / 2.0;
    if filled {
        ui.painter().circle_filled(center, radius, color);
    } else {
        ui.painter().circle_stroke(center, radius - 0.5, Stroke::new(1.5, color));
    }
}
