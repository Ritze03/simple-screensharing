# Styling Guide

How the desktop client's egui UI should be built and kept visually consistent as it
grows. This project has no UI code yet — this guide is written as a target to build
toward, not a description of what exists. Treat it as the spec for the first `theme.rs`
module and the widget helpers that sit on top of it.

**Top-level rule:** reference colors and button styles by *role*, never hard-code a raw
`Color32::from_rgb(...)` / `Color32::from_rgba_unmultiplied(...)` at a call site. Every
chrome color should be a named constant (`ACCENT`, `PANEL`, `TEXT_DIM`, …) that lives in
one central `theme.rs` module, so a future re-theme or accent-color change touches one
file instead of a grep-and-replace across the UI.

**Font:** the UI should use **Geist Mono** as its default font, with values and readouts
staying monospace so numeric/text columns line up. See "Fonts" below for the full
recommended font stack.

## Color & tokens

Keep the palette small and named by role, not by where it's used. A reasonable starting
set:

| Token | Role |
| --- | --- |
| `ACCENT` | Brand accent — section titles, primary buttons, slider defaults, selection tint |
| `PANEL_BASE` / `PANEL_SUNKEN` / `PANEL_RAISED` | Elevation-tiered surface backgrounds — see "Elevation tiers" below |
| `BORDER` | Hairline borders and dividers |
| `TEXT` / `TEXT_DIM` / `TEXT_FAINT` | Primary / secondary / tertiary text |
| `FIELD` | Input and code-block backgrounds |
| `BTN` / `BTN_BORDER` | Secondary button fill / border |
| `PRIMARY_TEXT` | Text color drawn on top of an accent-filled primary button |
| `DANGER` | Destructive-action red — danger button text/border |
| `WARNING` | Caution/attention amber — overshoot or approaching-a-limit states, distinct from `DANGER` and `SUCCESS` |

```rust
// Correct — reference by role:
ui.label(egui::RichText::new(label).color(theme::ACCENT).small());

// Wrong — do not hard-code a literal at the call site:
ui.label(egui::RichText::new(label).color(Color32::from_rgb(0x5B, 0x8B, 0xF0)));
```

If a UI element needs a color that doesn't exist yet, add a new `pub const` (or a
derived helper, see `tint()` below) to `theme.rs` — don't reach for a raw literal as a
one-off. This keeps every color in the app discoverable from a single file and makes a
future re-theme mechanical.

**Severity colors** (warnings, errors, validation states), if this project ever needs
them: derive them from the existing role tokens rather than inventing a new, unrelated
color. A "this is bad" state should read as a variant of the accent/danger family the
user already recognizes, not a new hue competing for attention.

**Gap found in practice (client's first `theme.rs`):** this base token list has no role
for a *good*-state indicator (an active-stream dot, a "Saved." confirmation, a
"Viewing"/"Listening" tag) — only the bad-state ("Severity colors") case above is
covered. `simple-screensharing-client/src/theme.rs` added one more token, `SUCCESS`, for
exactly that family of positive/active states, following the same "reference by role,
add a new named `pub const` rather than a raw literal" rule this section already states.
If a future project hits the same gap, add the equivalent token rather than reaching for
a raw green literal.

**`WARNING` token (added 2026-07-26):** a second gap found the same way — neither the
bad-state (`DANGER`) nor good-state (`SUCCESS`) family fits a "this isn't broken, but
pay attention" state (the canonical case: a bandwidth-overshoot warning banner). Added
as its own amber/orange `pub const`, following the same "one named token, reference by
role" pattern as `DANGER`/`SUCCESS` — no separate derived background/border constants,
since neither of those tokens has any either; route translucent fills/borders derived
from it through `tint()` at the call site same as the others.

### Elevation tiers

**Added 2026-07-26** (UI redesign pass — see
`superdoc/brainstorm/ui-redesign-multiroom-streaming.md`): the client's first `theme.rs`
pass shipped a single flat `PANEL` background for every surface — cards, sidebar, the
app background — which the redesign's UX/Design-lens critique flagged as reading too
flat/single-elevation for the Discord/Slack-style look this project is moving toward.
Replaced with three tiers at slightly different luminance, so surfaces can read as
sitting at different visual depths without relying on `BORDER` as the only depth cue:

- **`PANEL_SUNKEN`** — recessed surfaces: sidebar recesses, scroll wells, and the
  letterboxed background behind a chrome-free video tile.
- **`PANEL_BASE`** — the general app/window background level (same value the old flat
  `PANEL` used).
- **`PANEL_RAISED`** — elevated surfaces that should read as floating above the base
  background. `theme::card()` fills with this tier.

`PANEL` still exists as a `#[deprecated]` alias for `PANEL_BASE` so nothing that
happened to reference the old single-tier token breaks — new code should reach for one
of the three tiered tokens above instead, picking whichever one matches the surface's
intended depth.

### Chrome-free media

**Added 2026-07-26:** the redesign plan calls for video tiles to drop card chrome
entirely — edge-to-edge media, no bordered-card wrapper — unlike form/settings cards,
which keep the card treatment. Use `theme::media_frame()` for a video tile or other
live-media surface instead of `theme::card()`: no border, no title, no inner margin, and
a `PANEL_SUNKEN` backdrop for letterboxing. Never wrap a video tile in `card()` — that
reads as a settings widget, not a piece of media.

## Cards / sections

A **card** is a bordered rectangle with an uppercase, accent-colored title that groups
related controls. This is the basic unit for grouping settings and status displays:

```rust
// Recommended API shape:
theme::card(ui, "Connection", |ui| {
    // controls…
});
```

- **Title** — an uppercase, small, bold label in `ACCENT`. Expose it as its own
  `theme::section_label(text)` helper so a title can also be drawn outside a card (e.g.
  above a chart or a live-view panel) without duplicating the styling.
- **Consistent spacing.** Pick one small spacing constant and use it everywhere cards
  meet: gaps between stacked cards, gaps between side-by-side columns, and a card's own
  inner margin. Something in the neighborhood of **7-8px** is a good default — both
  prior projects this guide draws from converged on it independently, landing on ~7px
  item spacing and 8px corner rounding without coordinating. Recommend this project pick
  one number for each (e.g. `SPACING = 7.0`, `ROUNDING = 8`) in `theme.rs` and use it
  everywhere rather than ad hoc `add_space` calls scattered through the UI.
  *Update (2026-07-26):* the client later bumped `ROUNDING` from `8` to `14` as part of
  the Discord/Slack-style redesign pass — see "Elevation tiers" above and
  `theme.rs`'s `ROUNDING`/`ROUNDING_SM` doc comments. `SPACING` is unaffected. The 7-8px
  convergence recorded above is still the right starting point for a *new* project; it's
  this client's specific rounding value that moved on.
- **Watch out for doubled gaps.** If the outer panel already applies an inner margin
  matching the card spacing constant, don't also `add_space()` before the first card —
  the two stack and the top gap ends up roughly double the side gaps. Let one or the
  other own the outer gap, not both.
- **Zero the container's own item spacing** when stacking cards if the card helper emits
  its own trailing gap, so egui's default spacing doesn't add on top and inflate the
  gap between cards.

### Layout options for a settings-style view

A tab or panel made of cards has two reasonable shapes, pick whichever fits the content:

- **Single column, capped width.** Cap the content to a sensible max width (e.g. ~500px)
  inside a vertical `ScrollArea`, so the cards read as a settings panel instead of
  stretching edge-to-edge on a wide window, while still shrinking gracefully on a narrow
  one.
- **Two columns.** Split the panel into two halves with a fixed gap (matching the
  spacing constant) via `ui.columns(2, …)` — useful when one side is controls and the
  other is a live view or preview. Don't reserve an empty second column just for
  symmetry; if there's nothing to put there, use the single-column capped-width layout
  instead.

## Control rows

Inside a card, lay out controls as **two-column rows**: a label in the left half, the
control in the right half. This keeps every row's control edge aligned down the card.

```rust
// Recommended API shape:
theme::slider_row(ui, "Packet buffer size", &mut value, 0..=500, 10.0, 0, " ms");
```

A `slider_row` (or equivalent `setting_row` helper for other control types) should draw
the label in the left half via a `row_label` helper (see below) and, in the right half,
the control filling available space followed by a **fixed-width value preview**.

### Reserved value-width

Reserve a **fixed width** for the value spinner/preview at the right edge of a control
row, sized to the widest value the control can ever show (e.g. `"100.0%"`). *Why:* a
`DragValue` or similar sizes itself to its current text, so without a reserved width the
row's right edge shifts sideways every time the value gains a digit (`9.0%` → `100.0%`),
which reads as the whole row jittering. Pin the preview to the right and let the slider
or other control fill whatever space remains, so the row edge stays fixed as the window
resizes and only the control in between grows or shrinks.

```rust
ui.add_sized(
    [theme::VALUE_W, ui.spacing().interact_size.y],
    egui::DragValue::new(&mut value).range(0.0..=20000.0).speed(50.0),
);
```

For a non-numeric value (a color swatch, a status badge, …), reserve the same kind of
fixed-width slot in place of a spinner, for the same reason: so the row's control edge
stays aligned regardless of what's rendered inside it.

### Row labels

Draw the left-column label of a two-column row through a shared `row_label` helper, not
a bare `ui.label`, if the row is built with `ui.columns`. `ui.columns` top-aligns each
column by default, so a plain label sits at the row's top edge while a taller control
beside it renders vertically centered — the label then looks like it's floating above
its control. A `row_label` helper should allocate the label a row of the standard
control height and center it, so every label + control row reads as one aligned
horizontal band, and should avoid the "justified" layout that lets `ui.columns` spread a
wrapping label's letters across the line.

### No trailing colons on labels

Field and section labels do **not** end in a colon — `"Listen port"`, not
`"Listen port:"`. The control sits in the right column, so the colon adds no
information; it's a small thing but it's the kind of inconsistency that's easy to let
drift row by row, so treat "no colon, ever" as the fixed rule for every slider, combo
box, checkbox, and text-input row label.

## Checkboxes & radios

Use a custom styled checkbox/radio everywhere instead of raw `egui::Checkbox` /
`egui::RadioButton` / `ui.radio_value`. The shared look: an ~18px rounded mark that is
**accent-filled with a white check (or centered dot for radios)** when on, and a hairline
outline when off, with the **whole row** acting as a single click target and a subtle
hover wash. Build both on one shared mark-drawing routine so they visually read as the
same widget family — a checkbox with a square mark, a radio with a circular one.

```rust
// Recommended API shape:
theme::checkbox_row(ui, &mut flag, "Enabled");                 // two-column row, label left
theme::styled_checkbox(ui, &mut flag, "Show grid");             // content-sized, no row split

ui.horizontal(|ui| {
    theme::styled_radio(ui, &mut unit, Unit::Kmh, "km/h");
    theme::styled_radio(ui, &mut unit, Unit::Mph, "mph");
});
```

- A **row variant** (`checkbox_row` or similar) for use inside cards, spanning at least
  the row's left half so short labels still read at a uniform width, but capped at the
  card's width so a long label wraps rather than stretching the card.
- A **content-sized variant** (`styled_checkbox`) for places with no two-column row, e.g.
  a popup menu.
- A **disabled variant** that dims the outline/check/label and ignores clicks, for
  controls that are conditionally inactive rather than hidden.
- A **radio variant** (`styled_radio`) that takes `current: &mut T` and the `value: T`
  this button represents, and sets `*current = value` on click — the same call shape as
  `ui.radio_value`, just routed through the shared styled mark.

If several radio rows need to line up in a column (e.g. a set of unit toggles stacked
vertically), give the first button of every row the same fixed minimum width — measured
from the widest first-position label — rather than leaving each row content-sized, or
the second button in each row will drift left/right depending on the first label's
length.

**Status in the client (2026-07-25):** `simple-screensharing-client/src/theme.rs`
implements everything else in this guide (cards, buttons, control rows, `tint()`) but
deliberately does *not* implement this checkbox/radio family yet — `ui.rs` has no
checkbox or radio control anywhere in the app, so there was nothing to convert and no
real call site to validate an implementation against. Add it, following the API shape
above, when the first real checkbox/radio need shows up, rather than carrying unused
widget code speculatively.

*Update (2026-07-26):* the first radio-*shaped* need did show up — Stream Control's
Absolute Maximum / Per-Stream Maximum bandwidth-mode toggle — and was served with a pair
of `selectable_label`s rather than by implementing this family, matching what the Settings
tab's language switcher already does. *Why:* a two-option exclusive choice rendered as two
pills is already unambiguous and already has a precedent in this app, so building the
shared styled-mark family for a single call site would be carrying widget code no wider
need has validated yet. The guidance above stands unchanged for the first *multi*-option
radio group, or the first checkbox.

Do not hand-roll `egui::Checkbox::new` or `ui.radio_value` anywhere once these helpers
exist — every checkbox/radio in the app should go through them so the accent-mark look
stays uniform.

## Buttons

Three button variants cover essentially every action in the app:

- **Primary** — solid `ACCENT` fill, `PRIMARY_TEXT` text, bold. Use for the one primary
  action on a screen (e.g. "Connect", "Start sharing").
- **Secondary** — neutral fill (`BTN`) with a matching border (`BTN_BORDER`). The default
  for everything else (Cancel-that-isn't-destructive, Reload, Open Folder).
- **Danger** — transparent fill, `DANGER`-colored text and a faint `DANGER` border. Use
  for destructive or abort actions (Delete, Disconnect, Clean Up).

```rust
if ui.add(theme::primary_button("Start sharing")).clicked() { /* ... */ }
if ui.add(theme::secondary_button("Cancel")).clicked() { /* ... */ }
if ui.add(theme::danger_button("Disconnect")).clicked() { /* ... */ }
```

Expose each as a `theme::*_button(text)` helper returning a `Response`, rather than
composing `RichText` + `Frame` styling inline at each call site.

**Two more variants added 2026-07-26** (app-shell pass — see
`superdoc/ui/APP-SHELL.md`). Both exist because the three above are all *form* buttons:
sized to their label, at egui's default `interact_size` height, meant to live inside a
card. Two places in a sidebar want the opposite.

- **Nav pill** (`theme::nav_button(text, active)`) — a chunky, fixed-height
  (`NAV_BUTTON_HEIGHT`), full-width rail item for a persistent view-switcher.
  `ACCENT`-filled when active, strokeless and transparent otherwise so the one filled
  pill unambiguously says "you are here". The *caller* owns the width, since only the
  caller knows the rail's measure:
  `ui.add_sized([ui.available_width(), theme::NAV_BUTTON_HEIGHT], theme::nav_button(label, active))`.
- **Icon button** (`theme::icon_button(glyph)`) — compact, frameless, transparent, dim
  text that egui's hover visuals lift. For dense list rows where several actions stack
  and a bordered padded button would dominate a row that is mostly label.

### Indicators

`theme::status_dot(ui, color, filled)` — a small painted disc (`filled`) or ring, for
per-row live/offline status. *Why painted rather than a `●`/`○` glyph:* a state that
animates its alpha (a "reconnecting" pulse) needs a per-frame color, and a painted disc
also has an exact, font-independent size, so every row's label starts at the same x
regardless of which state its dot is in.

`theme::warning_banner(ui, message)` — an inline block: `tint(WARNING, 28)` fill,
`WARNING` 1px stroke, `ROUNDING_SM` corners, `SPACING` inner margin, strong `WARNING`
text. For a condition the user should notice but which is not an error, stated with real
numbers (first use: the bandwidth-overshoot notice, `superdoc/features/stream-control.md`).
*Why its own primitive rather than a `colored_label`:* the plan specifies it as a *banner*
— enough visual weight to be noticed above a section, not a line of amber text lost among
controls — and it is the first of a family (a stalled destination, a degraded encoder
fallback) that must all look the same.

## Global style install

Per-widget helpers only cover the widgets an app builds *through* them. Everything egui
draws for itself — combo-box popups, scroll bars, tooltips, text-edit fields, modal
backdrops — otherwise keeps egui's stock palette, which is a *different* dark grey than a
custom `PANEL_*` set and reads as an unstyled patch sitting on top of a themed app.

Expose a `theme::install(ctx)` that pushes the tokens into egui's own `Visuals` baseline
(`panel_fill`, `window_fill`, `extreme_bg_color`, `faint_bg_color`,
`override_text_color`, `selection`, and each `widgets.*` tier's fill/stroke/rounding) and
call it **once per process**, not once per frame — it rewrites the whole `Visuals`
struct. Doing this is what makes the whole window belong to one palette instead of two.

## Text inputs

Text fields should fill the available width (`.desired_width(ui.available_width())`)
rather than a fixed pixel width, so they don't clip inside a narrow card or column and
don't leave awkward dead space in a wide one. If a field needs a trailing button (e.g. a
"Browse…" or "Copy" action), lay the row out right-to-left so the button is pinned to
the right and the field takes whatever space remains.

## Modals

Use one shared pattern for confirm/input dialogs (destructive actions, name entry, etc.):
a dim, full-screen backdrop that swallows clicks, plus a centered window on top holding
the message or input field and a primary/secondary button pair — swap the primary for a
danger button when the action being confirmed is destructive. Enter confirms, Esc or a
backdrop click cancels.

```rust
// Recommended shape: backdrop area (Order::Middle) + centered window (Order::Foreground)
// holding message/input + [danger_button|primary_button] + secondary_button("Cancel").
```

**Status in the client (2026-07-26):** implemented for its first real need — the
room-leave confirmation (`ui.rs`'s `ui_leave_confirm`, see
`superdoc/ui/APP-SHELL.md`) — via `egui::Modal`, which already supplies the
click-swallowing dim backdrop, the centered window and Esc/backdrop dismissal this
section describes, so no hand-rolled backdrop was needed. The dialog itself follows the
rest of the pattern: message plus a `danger_button` (the action being confirmed is
destructive) and a `secondary_button("Cancel")` pair. Other destructive actions that
lose nothing — forgetting a saved room, leaving a room with no active stream or watch —
deliberately stay single-click; a confirm is for work in flight, not for every red
button.

## Fonts

Install **Geist Mono** as the default UI font, so values, readouts, and body text all
stay monospace and columns line up without special-casing numeric fields. Recommended
font stack, mirroring what both prior projects independently landed on:

- **Geist Mono** — the primary UI font, used for body text and for
  `TextStyle::Monospace` alike, so there's no visual seam between labels and values.
- **Geist Mono Nerd Font** — icon glyphs only, installed as a **fallback** family after
  the primary text font, not as the primary font itself. *Why fallback and not primary:*
  mixing an icon font in as the primary font can blur regular text edges; appending it
  only as a fallback keeps body text crisp while still resolving icon codepoints that
  fall outside the primary font's glyph set.
- **Geist Mono Bold** (or a bold static weight) as a separate named font family, reserved
  for the wordmark/logo rather than for emphasis in body text.

**Status in the client (2026-07-25):** not installed yet. `theme.rs`/`ui.rs` adopted this
guide's spacing (`SPACING = 7.0`), rounding (`ROUNDING = 8`), color-token, card, and
button conventions, but the UI still renders with egui's built-in default font — no
Geist Mono (or Nerd Font fallback) font files were embedded, since doing so needs actual
font binary assets this pass didn't have access to fetch. This is a known, tracked gap,
*(2026-07-26: `ROUNDING` moved to `14`, see "Elevation tiers" and the "Consistent
spacing" update above — this font-install gap itself is otherwise still open.)*
not a silent skip: install the font stack described above as a follow-up.

If this project ever supports toggling between a monospace and a proportional UI font,
watch for the **icon+text separator problem**: a literal space character renders at very
different widths in a monospace vs. a proportional font, so a hardcoded `"\u{f071} label"`
string looks right in one font and cramped or gappy in the other. Route any glyph+label
string through a small helper (an `icon_sep(mono: bool)`-style function returning a
narrower or wider separator depending on which font is active) instead of embedding a
literal space, so the *visual* gap stays constant when the font toggles. This only
applies when the glyph and label share one string — a glyph passed as its own layout
item can use a fixed pixel gap instead.

## egui version footguns

These are objective behaviors of egui itself across the 0.29→0.33 range, not
project-specific opinions — worth knowing regardless of which egui version this project
starts on.

- **`StrokeKind::Outside` for hand-painted rect strokes.** Since egui 0.31,
  `Painter::rect` / `rect_stroke` take an explicit `StrokeKind`. Default to
  **`StrokeKind::Outside`** for any hand-painted rect stroke (custom checkbox/radio
  outlines, card borders drawn manually, etc.) — this matches how egui itself drew rect
  strokes before 0.31 (`PathStroke::from(stroke).outside()`), so a border tuned by eye
  keeps sitting where it was measured. Note that egui's *own* built-in widgets (buttons,
  fields) moved to `StrokeKind::Inside` in 0.31 and that's not something a call site can
  override — their borders sit slightly further in than they did pre-0.31, but widget
  sizing is unaffected.
- **`CornerRadius` is a `u8`, not an `f32`.** Since egui 0.31, `Rounding` was renamed to
  `CornerRadius` and its backing type changed from `f32` to `u8`. Any *literal* radius
  (e.g. `CornerRadius::same(8)`) is unaffected, but a *computed* radius (e.g. half a
  rect's height, for a pill shape) must be rounded **up** with `.ceil() as u8`, not
  truncated with a bare `as u8` cast — the tessellator clamps an oversized radius to half
  the rect's shorter side for free, so rounding up is always safe, while truncating a
  fractional radius down visibly squares off what should be a fully rounded end.
- **Translucent fills: don't call `Color32::from_rgba_unmultiplied` directly.** Between
  egui 0.29 and 0.33, the meaning of `from_rgba_unmultiplied` changed from a
  gamma-correct blend to a naive per-channel multiply. At low alpha values (which is most
  UI tint work — hover washes, selection highlights, translucent accent fills) this is
  not a subtle difference: a tint chosen by eye against the old behavior can end up
  rendering at a small fraction of its intended opacity under the new one, so a
  translucent color that used to read clearly can all but disappear after an egui
  upgrade. Write a small `theme::tint(base, alpha)` helper that reproduces the intended
  blend explicitly, and route every translucent fill/stroke through it instead of calling
  the raw egui function at each call site — that way a future egui upgrade is a one-place
  fix instead of a UI-wide "why did all the tints get fainter" hunt.

## Closing note

This guide covers the general-purpose egui conventions this project starts with. As the
desktop client actually gets built, expect this document to grow with feature-specific
additions — sections on whatever screens and widgets the screensharing UI ends up
needing — layered on top of the rules above, not replacing them.
