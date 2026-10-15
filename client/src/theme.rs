//! Central egui theme: the design tokens (colors, sizes, spacing, radii, type scale),
//! the global `egui::Style`, the embedded fonts, and the icon font lookup.
//!
//! **Source of truth for every value here:** the approved "Proposed" room-centric mockup
//! (`:root` block of its CSS), whose colors are lifted from the former Electron
//! frontend's `tokens.css` (removed in the egui rework). Each constant names its CSS variable so the two
//! can be diffed. Reference colors by *role* (`ACCENT`, `TEXT_DIM`, …), never by raw hex
//! at a call site.
//!
//! The widgets that turn these tokens into rows, cards and controls live in
//! `crate::widgets` (see its module doc for the slot layout model); this module has no
//! widgets of its own.
//!
//! Entry point: [`apply`] — fonts + visuals + spacing. Call once at startup.
//!
//! The app is dark-only: the former Electron app never had a light theme (`color-scheme: dark`).

#![allow(dead_code)] // design tokens: not every token has a caller until the new screens land

use egui::{Color32, CornerRadius, FontFamily, FontId, Stroke, Vec2};

// ---------------------------------------------------------------------------
// Color tokens (mockup `:root` / tokens.css)
// ---------------------------------------------------------------------------

/// `--accent` — brand accent: section labels, primary buttons, selection, focus.
pub const ACCENT: Color32 = Color32::from_rgb(0x4C, 0x6E, 0xF5);
/// Top stop of `--accent-gradient` (primary buttons, active segment) and of
/// `--accent-mark-gradient`.
pub const ACCENT_LIGHT: Color32 = Color32::from_rgb(0x5C, 0x7B, 0xF7);
/// `#2b3ea8` — dark end of `--accent-mark-gradient` and the slider fill's start.
pub const ACCENT_DEEP: Color32 = Color32::from_rgb(0x2B, 0x3E, 0xA8);
/// `--pill-accent-bg-lighter` — text of in-progress pills, "watching" eye icons.
pub const ACCENT_LIGHTER: Color32 = Color32::from_rgb(0x8E, 0xA3, 0xF9);

/// `--panel-sunken` — recessed surfaces: right People panel, action bar, dest rows.
pub const PANEL_SUNKEN: Color32 = Color32::from_rgb(0x16, 0x17, 0x1A);
/// `--panel-base` — the central panel and modal background.
pub const PANEL_BASE: Color32 = Color32::from_rgb(0x1B, 0x1C, 0x1F);
/// `--panel-raised` — cards, share cards, popups.
pub const PANEL_RAISED: Color32 = Color32::from_rgb(0x24, 0x25, 0x2A);
/// `--sidebar-rail` — the left sidebar background (deepest surface).
pub const SIDEBAR_RAIL: Color32 = Color32::from_rgb(0x10, 0x11, 0x14);
/// `--tile-bg` — video tile / thumbnail backdrop.
pub const TILE_BG: Color32 = Color32::from_rgb(0x10, 0x11, 0x14);
/// `--elevation-1b` — segmented-control track, slider track, row hover.
pub const ELEV_1B: Color32 = Color32::from_rgb(0x1B, 0x1C, 0x1F);
/// `--elevation-16` — sidebar wells.
pub const ELEV_16: Color32 = Color32::from_rgb(0x16, 0x17, 0x1A);
/// `#191a1e` — light stripe of `--screen-hatch`.
pub const ELEV_19: Color32 = Color32::from_rgb(0x19, 0x1A, 0x1E);
/// `--elevation-14` — the sidebar's user band; dark stripe of `--screen-hatch`.
pub const ELEV_14: Color32 = Color32::from_rgb(0x14, 0x15, 0x18);
/// `--wash-hover-1` — hover wash for buttons, checkboxes, segments, members.
pub const WASH_HOVER_1: Color32 = Color32::from_rgb(0x24, 0x25, 0x2A);
/// `--wash-hover-2` — hover wash for popup items.
pub const WASH_HOVER_2: Color32 = Color32::from_rgb(0x26, 0x27, 0x2C);

/// `--border` — card / panel edge on a raised surface, inputs.
pub const BORDER: Color32 = Color32::from_rgb(0x34, 0x35, 0x3B);
/// `--divider` — rules inside a panel: band borders, modal head/foot, separators.
pub const DIVIDER: Color32 = Color32::from_rgb(0x2A, 0x2B, 0x30);
/// `--inset-border` — edge of a sunken inset (segmented track, slider track, dest row).
pub const INSET_BORDER: Color32 = Color32::from_rgb(0x26, 0x27, 0x2C);
/// `--well-border` — sidebar well edge and the section-heading rule.
pub const WELL_BORDER: Color32 = Color32::from_rgb(0x1F, 0x20, 0x24);

/// `--text` — primary text.
pub const TEXT: Color32 = Color32::from_rgb(0xE8, 0xE9, 0xEC);
/// `--text-dim` — secondary text, idle icons, readouts.
pub const TEXT_DIM: Color32 = Color32::from_rgb(0xA8, 0xAA, 0xB2);
/// `--text-faint` — tertiary text: counts, units, hints, tooltip ⓘ.
pub const TEXT_FAINT: Color32 = Color32::from_rgb(0x6E, 0x70, 0x78);
/// `--primary-text` — text on accent / danger-solid fills.
pub const PRIMARY_TEXT: Color32 = Color32::from_rgb(0x0B, 0x0C, 0x10);

/// `--field` — input, select and value-box background.
pub const FIELD: Color32 = Color32::from_rgb(0x28, 0x29, 0x2D);
/// `--btn` — secondary button fill.
pub const BTN: Color32 = Color32::from_rgb(0x2C, 0x2D, 0x32);
/// `--btn-border` — secondary button border, checkbox outline, meter bars (off).
pub const BTN_BORDER: Color32 = Color32::from_rgb(0x44, 0x45, 0x4C);

/// `--danger` — destructive actions, errors.
pub const DANGER: Color32 = Color32::from_rgb(0xE0, 0x5A, 0x5A);
/// `--success` — live / connected.
pub const SUCCESS: Color32 = Color32::from_rgb(0x50, 0xC8, 0x78);
/// `--warning` — reconnecting / degraded / over budget.
pub const WARNING: Color32 = Color32::from_rgb(0xE8, 0xA3, 0x3D);

const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied_const(r, g, b, (a * 255.0 + 0.5) as u8)
}

/// `--pill-live-bg` / `--pill-live-border`.
pub const PILL_LIVE_BG: Color32 = rgba(80, 200, 120, 0.12);
pub const PILL_LIVE_BORDER: Color32 = rgba(80, 200, 120, 0.3);
/// `--pill-progress-bg` / `--pill-progress-border`.
pub const PILL_PROGRESS_BG: Color32 = rgba(76, 110, 245, 0.14);
pub const PILL_PROGRESS_BORDER: Color32 = rgba(76, 110, 245, 0.4);
/// `--hatch-amber-border` and the two stripe pairs (`--hatch-amber-gradient` for
/// pills, `--hatch-amber-gradient-soft` for banners).
pub const HATCH_AMBER_BORDER: Color32 = rgba(232, 163, 61, 0.4);
pub const HATCH_AMBER_STRONG: Color32 = rgba(232, 163, 61, 0.16);
pub const HATCH_AMBER_WEAK: Color32 = rgba(232, 163, 61, 0.05);
pub const HATCH_AMBER_SOFT_A: Color32 = rgba(232, 163, 61, 0.10);
pub const HATCH_AMBER_SOFT_B: Color32 = rgba(232, 163, 61, 0.04);
/// `--accent-row-selected` / `--accent-ring-inset` — selected sidebar row.
pub const ACCENT_ROW_SELECTED: Color32 = rgba(76, 110, 245, 0.16);
pub const ACCENT_RING: Color32 = rgba(76, 110, 245, 0.28);
/// `--accent-ghost-*` — dashed "add something" buttons.
pub const ACCENT_GHOST_BG: Color32 = rgba(76, 110, 245, 0.07);
pub const ACCENT_GHOST_BG_HOVER: Color32 = rgba(76, 110, 245, 0.14);
pub const ACCENT_GHOST_BORDER: Color32 = rgba(76, 110, 245, 0.4);
/// `--accent-chip-*` — the "this room" dest row, info banner.
pub const ACCENT_CHIP_BG: Color32 = rgba(76, 110, 245, 0.1);
pub const ACCENT_CHIP_BORDER: Color32 = rgba(76, 110, 245, 0.45);
/// `--danger-tint` / `--danger-border`.
pub const DANGER_TINT: Color32 = rgba(224, 90, 90, 0.12);
pub const DANGER_BORDER: Color32 = rgba(224, 90, 90, 0.5);
/// `--scrim` — modal backdrop.
pub const SCRIM: Color32 = rgba(6, 7, 9, 0.66);
/// `--accent-glow` color (`0 3px 14px`), under primary buttons.
pub const ACCENT_GLOW: Color32 = rgba(76, 110, 245, 0.35);
/// `--success-glow` color (`0 0 6px`), around live dots.
pub const SUCCESS_GLOW: Color32 = rgba(80, 200, 120, 0.8);
/// `.chip-dark` fill (`rgba(0,0,0,.66)`) — chips over video.
pub const CHIP_DARK: Color32 = rgba(0, 0, 0, 0.66);

/// Debug-overlay colors (`--ov-*` in the mockup): vbox, hbox, fixed slot, fill slot.
pub const OV_VBOX: Color32 = rgba(110, 140, 255, 0.85);
pub const OV_HBOX: Color32 = rgba(232, 163, 61, 0.85);
pub const OV_SLOT: Color32 = rgba(80, 200, 120, 0.9);
pub const OV_FILL: Color32 = rgba(200, 200, 210, 0.45);

/// Avatar saturation / lightness (HSL). `Why` L = 34 %: white initials need >= 3:1
/// at every hue, and the worst hue is yellow (60 deg, contrast 3.6:1 here; at
/// L = 45 % it would be 2.3:1). One S/L for all hues keeps the whole circle
/// equally calm. Cross-client contract, see `superdoc/ui/STYLING-GUIDE.md`.
const AVATAR_SAT: f64 = 0.60;
const AVATAR_LIGHT: f64 = 0.34;

/// Hue (0..360 degrees) of a person's avatar: FNV-1a 32-bit over the
/// UTF-8 bytes of the trimmed, lowercased display name, modulo 360. A pure
/// function of the name so every viewer, client and restart agrees (the
/// browser client implements the same, `docs/js/ui/dom.js` `avatarHue`).
pub fn avatar_hue(name: &str) -> u32 {
    let mut h: u32 = 2_166_136_261;
    for b in name.trim().to_lowercase().bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(16_777_619);
    }
    h % 360
}

/// Avatar background (flat) for `name`: `HSL(avatar_hue, 60 %, 34 %)`.
pub fn avatar_color(name: &str) -> Color32 {
    let h = f64::from(avatar_hue(name));
    let a = AVATAR_SAT * AVATAR_LIGHT.min(1.0 - AVATAR_LIGHT);
    let f = |n: f64| {
        let k = (n + h / 30.0) % 12.0;
        let v = AVATAR_LIGHT - a * (-1.0_f64).max((k - 3.0).min(9.0 - k).min(1.0));
        (255.0 * v).round() as u8
    };
    Color32::from_rgb(f(0.0), f(8.0), f(4.0))
}

// ---------------------------------------------------------------------------
// Size / spacing / radius tokens (mockup `--s*`, `--*-h`, `--rounding*`)
// ---------------------------------------------------------------------------

/// `--s1..--s4` — the only gaps: item spacing, paddings. (`--s0` = 0.)
pub const S1: f32 = 4.0;
pub const S2: f32 = 8.0;
pub const S3: f32 = 12.0;
pub const S4: f32 = 16.0;

/// `--band-h` — every horizontal band: brand, room header, user band, action bar,
/// modal head.
pub const BAND_H: f32 = 56.0;
/// `--ctl-h` — every button, field, select, segmented control, value box, control row.
pub const CTL_H: f32 = 32.0;
/// `--ctl-h-lg` — buttons in the bottom bar and in modal footers.
pub const CTL_H_LG: f32 = 36.0;
/// `--pill-h` — status pills (badges). Large pills (`.badge.lg`) are [`PILL_H_LG`].
pub const PILL_H: f32 = 18.0;
/// `.badge.lg` height (bottom bar status, modal head "Applying").
pub const PILL_H_LG: f32 = 22.0;
/// `--row-h` — room row, person row, popup item.
pub const ROW_H: f32 = 32.0;
/// `--subrow-h` — sharer line under a room.
pub const SUBROW_H: f32 = 24.0;
/// `.destrow` height.
pub const DEST_ROW_H: f32 = 40.0;
/// `.pi.two` — popup item with a sub line.
pub const POP_ITEM_TWO_H: f32 = 44.0;
/// `.modal-foot` height.
pub const MODAL_FOOT_H: f32 = 64.0;
/// Section heading height (`.section-label`).
pub const HEADING_H: f32 = 16.0;
/// Label slot of a control row (`LABEL_W` in the mockup).
pub const LABEL_W: f32 = 120.0;
/// Value slot of a control row (`VALUE_W` in the mockup) — fits "40000" / "4000 ms".
pub const VALUE_W: f32 = 72.0;
/// Fixed width of the left sidebar (`.p-side`).
pub const SIDEBAR_W: f32 = 272.0;
/// Fixed width of the right People panel (`.p-right`).
pub const PEOPLE_W: f32 = 232.0;
/// Card/tile grid column pitch: `cols = clamp(floor((avail + 12) / 272), 1, 4)`.
pub const GRID_PITCH: f32 = 272.0;
/// Card/tile grid gap (`g3`).
pub const GRID_GAP: f32 = S3;
/// Max card/tile grid columns.
pub const GRID_MAX_COLS: usize = 4;
/// Default icon size (`.ms`), and the icon slot width used by rows.
pub const ICON: f32 = 18.0;
/// Icon size inside buttons (`.btn .ms`).
pub const ICON_BTN: f32 = 17.0;
/// Status dot diameter (`.sdot`).
pub const DOT_SIZE: f32 = 9.0;
/// Checkbox / radio mark size (`.cbx .box`).
pub const MARK: f32 = 18.0;
/// Avatar diameters (`.avatar`, `.avatar.sm`).
pub const AVATAR: f32 = 32.0;
pub const AVATAR_SM: f32 = 22.0;
/// Modal widths (`.modal`, `.wide`, `.xwide`); all are capped at window − 48.
pub const MODAL_W: f32 = 520.0;
pub const MODAL_W_WIDE: f32 = 640.0;
pub const MODAL_W_XWIDE: f32 = 1120.0;

/// `--rounding` — modals.
pub const ROUNDING: u8 = 14;
/// `--rounding-well` — sidebar wells, cards (`.gcard`, `.scard` use 12).
pub const ROUNDING_WELL: u8 = 12;
/// `--rounding-md` — buttons, segmented track, banners, insets.
pub const ROUNDING_MD: u8 = 9;
/// `--rounding-sm` — rows, fields, icon buttons.
pub const ROUNDING_SM: u8 = 8;
/// Small inner radius: segments, value boxes, sub rows, checkbox hover wash, tiles.
pub const ROUNDING_XS: u8 = 6;

// ---------------------------------------------------------------------------
// Type scale (mockup font sizes, px == egui points at 1x)
// ---------------------------------------------------------------------------

/// Font weights available. Geist ships Regular + Bold; GeistMono only Regular (the
/// mockup's mono text is never bold).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Weight {
    Regular,
    Bold,
    Mono,
}

/// Body text (`body`, 14) — rarely used directly; rows use [`FS_ROW`].
pub const FS_BODY: f32 = 14.0;
/// Room row / person row name, field text (13).
pub const FS_ROW: f32 = 13.0;
/// Control-row labels, checkbox labels, dest rows, popup items, banners' big text (12.5).
pub const FS_LABEL: f32 = 12.5;
/// Buttons (12, bold), sharer rows, meta, value boxes (mono), banner text.
pub const FS_SMALL: f32 = 12.0;
/// Sub lines (11): user status, popup sub, counts (`.num`, mono).
pub const FS_TINY: f32 = 11.0;
/// Units (`.unit`, mono 10.5).
pub const FS_UNIT: f32 = 10.5;
/// Section label (`.section-label`, 10 bold, uppercase, `0.11em` tracking).
pub const FS_SECTION: f32 = 10.0;
/// Pills (`.badge`, 9.5 bold, uppercase, `0.05em` tracking).
pub const FS_PILL: f32 = 9.5;
/// Room header title (`h2`, 18 bold).
pub const FS_H2: f32 = 18.0;
/// Modal title / brand (`h3`, 15 bold).
pub const FS_H3: f32 = 15.0;

/// Name of the bold Geist family (`FontFamily::Name`).
pub const FAMILY_BOLD: &str = "geist-bold";
/// Name of the Material Symbols family (`FontFamily::Name`).
pub const FAMILY_ICONS: &str = "material-symbols";

/// A `FontId` for `weight` at `size` points.
pub fn font(weight: Weight, size: f32) -> FontId {
    match weight {
        Weight::Regular => FontId::new(size, FontFamily::Proportional),
        Weight::Bold => FontId::new(size, FontFamily::Name(FAMILY_BOLD.into())),
        Weight::Mono => FontId::new(size, FontFamily::Monospace),
    }
}

/// The icon font at `size` points.
pub fn icon_font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(FAMILY_ICONS.into()))
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

static GEIST_REGULAR: &[u8] = include_bytes!("../assets/fonts/Geist-Regular.ttf");
static GEIST_BOLD: &[u8] = include_bytes!("../assets/fonts/Geist-Bold.ttf");
static GEIST_MONO: &[u8] = include_bytes!("../assets/fonts/GeistMono-Regular.ttf");
static MATERIAL_SYMBOLS: &[u8] = include_bytes!("../assets/fonts/MaterialSymbolsOutlined.ttf");

/// Font definitions for the new design: Geist as the proportional face, Geist Mono as
/// monospace, a named bold family, and a named icon family. egui's bundled fonts stay
/// as fallbacks (emoji, rare symbols); the icon font is the last fallback of every
/// text family, so an icon glyph inside ordinary text still renders instead of tofu.
pub fn font_definitions() -> egui::FontDefinitions {
    use egui::FontData;
    use std::sync::Arc;

    let mut defs = egui::FontDefinitions::default();
    let egui_prop = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let egui_mono = defs.families.get(&FontFamily::Monospace).cloned().unwrap_or_default();

    defs.font_data.insert("geist".into(), Arc::new(FontData::from_static(GEIST_REGULAR)));
    defs.font_data.insert("geist-bold".into(), Arc::new(FontData::from_static(GEIST_BOLD)));
    defs.font_data.insert("geist-mono".into(), Arc::new(FontData::from_static(GEIST_MONO)));
    // Material Symbols: em is 960 units but its ascent+descent span 1152, so egui's
    // line box is taller than the glyph. The tweak re-centres the glyph's ink on the
    // line box's centre, so painting an icon galley centred in a rect centres the icon
    // (verified on screen in the gallery; see `widgets::icon_in`).
    defs.font_data.insert(
        "material-symbols".into(),
        Arc::new(
            FontData::from_static(MATERIAL_SYMBOLS).tweak(egui::FontTweak { y_offset_factor: ICON_Y_OFFSET_FACTOR, ..Default::default() }),
        ),
    );

    let with = |first: &[&str], rest: &[String]| -> Vec<String> {
        let mut v: Vec<String> = first.iter().map(|s| s.to_string()).collect();
        v.extend(rest.iter().cloned());
        v.push("material-symbols".into());
        v
    };
    defs.families.insert(FontFamily::Proportional, with(&["geist"], &egui_prop));
    defs.families.insert(FontFamily::Monospace, with(&["geist-mono", "geist"], &egui_mono));
    defs.families.insert(FontFamily::Name(FAMILY_BOLD.into()), with(&["geist-bold", "geist"], &egui_prop));
    defs.families.insert(FontFamily::Name(FAMILY_ICONS.into()), vec!["material-symbols".into()]);
    defs
}

/// Vertical nudge for the icon font, as a fraction of its size (see `font_definitions`).
const ICON_Y_OFFSET_FACTOR: f32 = 0.0;

// ---------------------------------------------------------------------------
// Icons
// ---------------------------------------------------------------------------

/// Material Symbols Outlined icons available through [`icon`], by ligature name, sorted.
/// egui has no ligature shaping, so each name maps to its private-use codepoint. To add
/// one: `python3 -I client/assets/fonts/icon-codepoints.py <name>` and paste the line.
pub const ICONS: &[(&str, char)] = &[
    ("add", '\u{e145}'),
    ("add_to_queue", '\u{e05c}'),
    ("arrow_drop_down", '\u{e5c5}'),
    ("cast", '\u{e307}'),
    ("check", '\u{e5ca}'),
    ("check_circle", '\u{e86c}'),
    ("chevron_left", '\u{e408}'),
    ("chevron_right", '\u{e409}'),
    ("close", '\u{e14c}'),
    ("cloud_off", '\u{e2c1}'),
    ("content_copy", '\u{e14d}'),
    ("crop_square", '\u{e3c1}'),
    ("delete", '\u{e872}'),
    ("desktop_windows", '\u{e30c}'),
    ("edit", '\u{e150}'),
    ("error", '\u{e000}'),
    ("expand_less", '\u{e5ce}'),
    ("expand_more", '\u{e5cf}'),
    ("forum", '\u{e0bf}'),
    ("fullscreen", '\u{e5d0}'),
    ("fullscreen_exit", '\u{e5d1}'),
    ("grid_view", '\u{e9b0}'),
    ("info", '\u{e88e}'),
    ("link", '\u{e157}'),
    ("login", '\u{ea77}'),
    ("logout", '\u{e9ba}'),
    ("meeting_room", '\u{eb4f}'),
    ("mic", '\u{e029}'),
    ("more_horiz", '\u{e5d3}'),
    ("move_down", '\u{eb61}'),
    ("music_note", '\u{e3a1}'),
    ("open_in_new", '\u{e895}'),
    ("pause", '\u{e034}'),
    ("person", '\u{e7fd}'),
    ("play_arrow", '\u{e037}'),
    ("radio_button_checked", '\u{e837}'),
    ("radio_button_unchecked", '\u{e836}'),
    ("refresh", '\u{e5d5}'),
    ("restart_alt", '\u{f053}'),
    ("screen_share", '\u{e0e2}'),
    ("settings", '\u{e8b8}'),
    ("speaker", '\u{e32d}'),
    ("stop_circle", '\u{ef71}'),
    ("stop_screen_share", '\u{e0e3}'),
    ("swap_horiz", '\u{e8d4}'),
    ("sync", '\u{e627}'),
    ("sync_problem", '\u{e629}'),
    ("tune", '\u{e429}'),
    ("tv_off", '\u{e647}'),
    ("visibility", '\u{e417}'),
    ("visibility_off", '\u{e8f5}'),
    ("volume_off", '\u{e04f}'),
    ("volume_up", '\u{e050}'),
    ("warning", '\u{e002}'),
    ("web", '\u{e051}'),
];

/// The glyph for a Material Symbols icon name (e.g. `icon("settings")`), as a `String`
/// to paint with [`icon_font`]. An unknown name is a programming error: it panics in
/// debug builds (caught by `every_icon_name_resolves`-style tests) and renders the
/// "error" icon in release builds rather than tofu.
pub fn icon(name: &str) -> String {
    icon_char(name).to_string()
}

/// Like [`icon`], as a `char`.
pub fn icon_char(name: &str) -> char {
    match ICONS.binary_search_by(|(n, _)| n.cmp(&name)) {
        Ok(i) => ICONS[i].1,
        Err(_) => {
            debug_assert!(false, "unknown icon {name:?}; add it to theme::ICONS");
            '\u{e000}'
        }
    }
}

// ---------------------------------------------------------------------------
// Global style (new design)
// ---------------------------------------------------------------------------

/// Install the new design on `ctx`: fonts, visuals, type scale and spacing. Call once
/// at startup (before the first frame that draws). Idempotent.
pub fn apply(ctx: &egui::Context) {
    ctx.set_fonts(font_definitions());

    let mut v = egui::Visuals::dark();
    v.override_text_color = Some(TEXT);
    v.panel_fill = PANEL_BASE;
    v.window_fill = PANEL_BASE;
    v.window_stroke = Stroke::new(1.0_f32, BORDER);
    v.window_corner_radius = CornerRadius::same(ROUNDING);
    v.window_shadow = egui::Shadow { offset: [0, 26], blur: 70, spread: 0, color: rgba(0, 0, 0, 0.7) };
    v.popup_shadow = egui::Shadow { offset: [0, 14], blur: 40, spread: 0, color: rgba(0, 0, 0, 0.6) };
    v.menu_corner_radius = CornerRadius::same(10);
    v.extreme_bg_color = FIELD;
    v.text_edit_bg_color = Some(FIELD);
    v.faint_bg_color = ELEV_16;
    v.code_bg_color = FIELD;
    v.hyperlink_color = ACCENT;
    v.warn_fg_color = WARNING;
    v.error_fg_color = DANGER;
    v.selection.bg_fill = rgba(76, 110, 245, 0.45);
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);

    let r = CornerRadius::same(ROUNDING_SM);
    let w = &mut v.widgets;
    for (wv, fill, stroke, fg) in [
        (&mut w.noninteractive, PANEL_BASE, BORDER, TEXT_DIM),
        (&mut w.inactive, BTN, BTN_BORDER, TEXT),
        (&mut w.hovered, WASH_HOVER_1, BTN_BORDER, TEXT),
        (&mut w.active, WASH_HOVER_2, ACCENT, TEXT),
        (&mut w.open, FIELD, BORDER, TEXT),
    ] {
        wv.bg_fill = fill;
        wv.weak_bg_fill = fill;
        if fill == WASH_HOVER_1 || fill == WASH_HOVER_2 {
            // Scroll handle while hovered / dragged (no egui widget we use
            // paints `bg_fill` in these states otherwise).
            wv.bg_fill = BTN_BORDER;
        }
        wv.bg_stroke = Stroke::new(1.0_f32, stroke);
        wv.fg_stroke = Stroke::new(1.0_f32, fg);
        wv.corner_radius = r;
        wv.expansion = 0.0;
    }
    ctx.set_visuals(v);

    ctx.style_mut(|s| {
        use egui::TextStyle;
        s.text_styles = [
            (TextStyle::Heading, font(Weight::Bold, FS_H2)),
            (TextStyle::Body, font(Weight::Regular, FS_ROW)),
            (TextStyle::Button, font(Weight::Bold, FS_SMALL)),
            (TextStyle::Small, font(Weight::Regular, FS_TINY)),
            (TextStyle::Monospace, font(Weight::Mono, FS_SMALL)),
        ]
        .into();
        s.spacing.item_spacing = Vec2::new(S2, S2);
        s.spacing.button_padding = Vec2::new(S3, 0.0);
        s.spacing.interact_size = Vec2::new(40.0, CTL_H);
        s.spacing.menu_margin = egui::Margin::same(6);
        s.spacing.window_margin = egui::Margin::ZERO;
        s.spacing.icon_width = MARK;
        s.spacing.icon_spacing = S2;
        // Scroll bars: a thin floating handle (no track) drawn 3 px inside the
        // scroll area's edge, in the dim `BTN` grey (`BTN_BORDER` when hovered /
        // dragged, via the widget visuals' `bg_fill` above). `Why:` egui's
        // default floating bar is `TEXT`-coloured and sits flush on the area's
        // right edge, so the room body's bar painted a bright line over the
        // People panel's divider; the mockup shows no bar at all at rest.
        s.spacing.scroll = egui::style::ScrollStyle {
            floating: true,
            bar_width: 8.0,
            floating_width: 4.0,
            floating_allocated_width: 0.0,
            handle_min_length: 24.0,
            bar_inner_margin: 0.0,
            bar_outer_margin: 3.0,
            foreground_color: false,
            dormant_background_opacity: 0.0,
            active_background_opacity: 0.0,
            interact_background_opacity: 0.0,
            dormant_handle_opacity: 0.0,
            active_handle_opacity: 1.0,
            interact_handle_opacity: 1.0,
        };
        // Labels in this design are never selectable text: selectable labels sense
        // clicks and would steal them from the clickable rows they sit in.
        s.interaction.selectable_labels = false;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_table_is_sorted_and_unique() {
        for w in ICONS.windows(2) {
            assert!(w[0].0 < w[1].0, "ICONS not sorted/unique at {:?} / {:?}", w[0].0, w[1].0);
        }
        assert_eq!(icon_char("settings"), '\u{e8b8}');
        assert_eq!(icon("close"), "\u{e14c}");
    }

    /// Every icon name used through an icon-taking helper anywhere under `src/` must be in
    /// [`ICONS`], so a typo or a new icon fails here instead of panicking in a debug build.
    ///
    /// Plain-text scan, no regex: for each call-site marker below, read the string
    /// literal(s) inside the call's parentheses (stopping at `;`, an unbalanced `)`, and
    /// for most markers `{`/`}`). To cover a new helper, add a row to `CALLS`.
    #[test]
    fn every_icon_name_used_in_src_is_in_icons() {
        #[derive(Clone, Copy)]
        enum Take {
            /// First literal in the call (icon is the first literal argument).
            First,
            /// Last top-level literal in the call (icon is the last argument, after `t!(..)` etc.).
            LastTop,
            /// Every literal until `;` (`let icon = match ..`, tuple of icons).
            All,
            /// Only if the literal follows the marker directly (`icon: "x"`).
            Immediate,
            /// Second argument, as `Some("x")` (`chip_dark(ui, Some("x"), ..)`; `None` is skipped).
            SecondSome,
        }
        use Take::*;
        const CALLS: &[(&str, Take)] = &[
            (".icon(", First),
            (".trailing(", First),
            ("theme::icon(", First),
            ("icon_char(", First),
            ("PopItem::new(", First),
            ("Lead::Icon(", First),
            ("icon_btn(", First),
            ("paint_icon(", First),
            ("chip_dark(", SecondSome),
            ("banner(", First),
            (".toast(", LastTop),
            ("icon:", Immediate),
            ("let icon =", All),
            ("let (icon,", All),
        ];

        /// Reads the string literal whose opening quote is `b[i]`; returns it and the index after it.
        fn literal(b: &[u8], i: usize) -> (String, usize) {
            let mut j = i + 1;
            while j < b.len() && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            (String::from_utf8_lossy(&b[i + 1..j.min(b.len())]).into_owned(), j + 1)
        }

        fn scan(src: &str, found: &mut Vec<String>) {
            // Drop comment lines (docs mention example calls).
            let code: String = src.lines().filter(|l| !l.trim_start().starts_with("//")).collect::<Vec<_>>().join("\n");
            let b = code.as_bytes();
            for &(marker, take) in CALLS {
                for (at, _) in code.match_indices(marker) {
                    let mut i = at + marker.len();
                    if matches!(take, Immediate) {
                        while i < b.len() && b[i] == b' ' {
                            i += 1;
                        }
                        if b.get(i) == Some(&b'"') {
                            found.push(literal(b, i).0);
                        }
                        continue;
                    }
                    if matches!(take, SecondSome) {
                        let Some(comma) = code[i..].find(',') else { continue };
                        let rest = code[i + comma + 1..].trim_start();
                        if let Some(r) = rest.strip_prefix("Some(\"") {
                            found.push(r.split('"').next().unwrap_or("").to_string());
                        }
                        continue;
                    }
                    let mut depth = 0i32;
                    let mut last_top = None;
                    while i < b.len() {
                        match b[i] {
                            b'"' => {
                                let (lit, next) = literal(b, i);
                                i = next;
                                match take {
                                    First => {
                                        found.push(lit);
                                        break;
                                    }
                                    All => found.push(lit),
                                    _ if depth == 0 => last_top = Some(lit),
                                    _ => {}
                                }
                                continue;
                            }
                            b'(' => depth += 1,
                            b')' if depth == 0 => break,
                            b')' => depth -= 1,
                            b';' => break,
                            b'{' | b'}' if !matches!(take, All) => break,
                            _ => {}
                        }
                        i += 1;
                    }
                    found.extend(last_top);
                }
            }
            // Icon tuples such as `("cast", IconTone::Live, ..)`.
            for (at, _) in code.match_indices("\", IconTone::") {
                if let Some(start) = code[..at].rfind('"') {
                    found.push(code[start + 1..at].to_string());
                }
            }
        }

        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }

        let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        walk(&src_dir, &mut files);
        let mut checked = 0;
        let mut missing = Vec::new();
        for f in files {
            // This file's own tests and docs spell out the markers.
            if f.file_name().is_some_and(|n| n == "theme.rs") {
                continue;
            }
            let mut found = Vec::new();
            scan(&std::fs::read_to_string(&f).unwrap(), &mut found);
            for name in found {
                checked += 1;
                if ICONS.binary_search_by(|(n, _)| n.cmp(&name.as_str())).is_err() {
                    missing.push(format!("{}: {name:?}", f.strip_prefix(&src_dir).unwrap().display()));
                }
            }
        }
        assert!(checked > 40, "icon scan found only {checked} names; did the call-site markers rot?");
        assert!(missing.is_empty(), "icon names used but missing from theme::ICONS (see client/assets/fonts/icon-codepoints.py): {missing:#?}");
    }

    #[test]
    fn every_icon_has_a_glyph_in_the_embedded_font() {
        let ctx = egui::Context::default();
        apply(&ctx);
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            ctx.fonts_mut(|f| {
                for (name, ch) in ICONS {
                    assert!(f.has_glyph(&icon_font(18.0), *ch), "icon {name} missing from font");
                }
                assert!(f.has_glyph(&font(Weight::Mono, 12.0), '0'));
                assert!(f.has_glyph(&font(Weight::Bold, 12.0), '…'));
            });
        });
    }

    /// Cross-client contract: `docs/dev/tests.js` asserts the same table.
    #[test]
    fn avatar_colour_vectors_match_the_browser() {
        let v: [(&str, u32, [u8; 3]); 9] = [
            ("Ritze", 177, [0x23, 0x8b, 0x86]),
            ("mo", 245, [0x2b, 0x23, 0x8b]),
            ("client-1", 144, [0x23, 0x8b, 0x4c]),
            ("client-2", 321, [0x8b, 0x23, 0x66]),
            ("Zed", 4, [0x8b, 0x2a, 0x23]),
            ("user-d740", 328, [0x8b, 0x23, 0x5a]),
            ("Größe", 314, [0x8b, 0x23, 0x72]),
            ("", 61, [0x89, 0x8b, 0x23]),
            ("  Mo ", 245, [0x2b, 0x23, 0x8b]),
        ];
        for (name, hue, rgb) in v {
            assert_eq!(avatar_hue(name), hue, "hue of {name:?}");
            assert_eq!(avatar_color(name), Color32::from_rgb(rgb[0], rgb[1], rgb[2]), "colour of {name:?}");
        }
    }

    /// White initials stay readable (>= 3:1, WCAG large/bold text) on every hue.
    #[test]
    fn avatar_colour_keeps_white_text_readable_on_every_hue() {
        let lin = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
        };
        for hue in 0..360u32 {
            // Any name that hashes to this hue (the colour only depends on the hue).
            let name = (0u32..).map(|i| format!("n{i}")).find(|n| avatar_hue(n) == hue).unwrap();
            let c = avatar_color(&name);
            let y = 0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b());
            assert!(1.05 / (y + 0.05) >= 3.0, "hue {hue}: contrast {}", 1.05 / (y + 0.05));
        }
    }
}
