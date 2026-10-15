# Styling Guide (egui client)

Rewritten 2026-10-14 for the native egui UI in `client/src/` (`theme.rs`, `widgets/`,
`ui/`). It replaces an earlier target-state guide written before any UI code existed.
Read this before building or changing any control, row, card or screen.

## Where the rules come from (and who wins)

Two sources, with a fixed precedence:

1. **The former Electron app's design wins on looks** (that app is removed; its design lives on as the reference, see `FRONTEND-SHELL.md`): colours, sizes, radii, type, the segmented/checkbox
   look, copy. The approved room-centric HTML mockup
   (https://claude.ai/artifact/5yA1iHf1YnXqmABkZajCe4) is lifted from the former Electron app's
   `tokens.css` (deleted in the egui rework); every constant in `theme.rs` names the CSS variable
   it came from so the two can be diffed.
2. **The user's egui guide wins on construction**: how a control or row is built so it
   cannot overflow or drift. That guide is
   `/home/mo/Schinken/Programming/Local/ForzaTelemetryV3/docs/ui/STYLING-GUIDE.md`
   (a different project; read it for the longer rationale). The conventions adapted here
   are the sections below that say "from the Forza guide".

*Why this split:* the former Electron app is the look the user approved and keeps; the Forza guide
encodes hard-won egui layout lessons (egui grows a `Ui` to fit a too-wide child, so one
long row can push a whole card over its neighbour). We want the first one's appearance
built with the second one's discipline.

### Deliberate deviations from the Forza guide (decided in task I8)

| Forza guide | Here | Why |
| --- | --- | --- |
| Label | control 50/50 `columns(2)`, labels wrap | Fixed **120 px label slot**, text truncates with "…" (tooltip carries the full text). Rows in every card line up at the same x and a long German label can't change a row's height. |
| Value spinner `DragValue`, default alignment | `num_box`: right-aligned **mono** value in the fixed 72 px slot, drag to change, click to type | Matches the mockup's `.num` boxes; right alignment keeps digits column-aligned as values grow. |
| Segmented control: its own look | The mockup's segmented/checkbox look | The Electron-era look wins. |
| Geist Mono as the UI font | **Geist** (sans) for text, **GeistMono only for numbers** | The former Electron app was sans with mono readouts; a monospace UI made labels wide enough to squeeze room names (see `FRONTEND-SHELL.md`, deviations). |

## The layout rule: explicit nested boxes, fixed-size slots

**Build every screen as a tree of nested vertical and horizontal containers with fixed-size
slots, using `client/src/widgets/`. Never lay out a row with `ui.horizontal` +
`add_space`.**

*Why (the user's words):* "aggressively use panes/vboxes/hboxes… Otherwise you usually
fail at such tasks." Free-flowing egui layouts are where earlier attempts went wrong.

egui has **no flexbox**: `ui.horizontal` gives each widget whatever is left, so a fill
slot between two fixed ones (`[120 label][fill control][72 value]`) would eat the value
slot's space. The fix is done **once**, in `widgets::HBox`, by explicit width math:

1. the row allocates `available_width x height` up front (`allocate_exact_size`), so its
   size never depends on its contents;
2. `slot_spans` (pure, unit-tested) computes
   `fill = (row_w - pad_l - pad_r - sum(fixed) - gap*(n-1)) / n_fill`, never below 0;
3. each slot is drawn in a child `Ui` whose max rect **and clip rect** are the slot, so a
   widget can neither widen its slot nor paint into the next one (text truncates).

Consequences you can rely on: rows of a type have identical heights, and slot x-positions
line up across rows for any width because they are a function of the row width only.

Vocabulary: **vbox** = `ui.vertical` / a card or well frame (frames only paint and pad);
**hbox** = an `HBox` row; **slot** = `fixed(w)` cell; **fill** = `FILL` cell.

### Helpers (all in `widgets/`, all shown by `--gallery`)

- Layout: `HBox`/`HRow` (`HRow::show(ui, i, align, ..)`, `.text`, `.icon`), `slot_spans`,
  `columns`, `grid_cols`/`grid_cols_stable` (`layout.rs`).
- Rows, one function per row type so two rows of a type cannot differ (`rows.rs`):
  `ctrl_row`, `ctrl_row_value`, `slider_row`, `room_row`, `sharer_row`, `person_row`,
  `dest_row`, `pop_item`, `btn_row`.
- Containers (`containers.rs`): `card` (raised, accent uppercase title + optional count),
  `well` (sunken sidebar group), `band` (56 px header/footer strip), `grid_rows`
  (card/tile grid), `modal_shell` / `modal_frame` / `modal_footer`.
- Controls (`controls.rs`): `Btn` (primary/secondary/danger/danger_solid/ghost, icons,
  `large`), `icon_btn`, `Pill`, `status_dot`, `avatar`, `meter`, `segmented`, `checkbox`,
  `radio`, `slider`, `value_box`, `num_box`, `readout`, `select`, `text_field`,
  `collapser`, `heading`, `banner`, `info_tip`, `chip_dark`, `vsep`.
- Painting (`paint.rs`): text/icon/gradient/hatch/dashed/glow primitives.
- **Debugging**: `widgets::set_debug_slots(ctx, true)` outlines every vbox/hbox/slot and
  logs them (`slot_log`); run `simple-screensharing-client --gallery` to see every helper
  with fake data and as the visual QC screen for design work.

A new row shape: build it the same way (an `HBox` with named slots), add it to
`rows.rs`, show it in the gallery. Don't hand-roll it inside a screen.

### Panes: nothing draws outside its container (from the Forza guide)

Every page column, card and row stays inside its container's width. A too-wide row is
**cut** at the container edge; it can never widen the container. The clip is the safety
net, not the layout: if something is cut, fix its row (shrink / truncate / stack), don't
widen the pane. Custom-painted widgets take their size from `ui.available_width()` and
paint only inside the rect they allocated. Never `set_width`/`set_min_width` inside a card
to make room for a wide row.

## Tokens (`client/src/theme.rs`)

Reference colours and sizes **by role**, never a raw `Color32::from_rgb(..)` or a magic
number at a call site; if a role is missing, add a named `pub const` to `theme.rs`.

- **Sizes**: band 56 (`BAND_H`), control 32 / large 36 (`CTL_H`, `CTL_H_LG`), pill 18 / 22,
  row 32 (`ROW_H`) / subrow 24, dest row 40, label slot 120 (`LABEL_W`), value slot 72
  (`VALUE_W`), spacing 4/8/12/16 (`S1..S4`), sidebar 272, People 232, modal 520/640/1120.
  Radii `ROUNDING*` (14 card, 12 well, 9, 8, 6).
- **Colours** (former-Electron/mockup values): surfaces `SIDEBAR_RAIL` < `PANEL_SUNKEN` <
  `PANEL_BASE` < `PANEL_RAISED`; text `TEXT`/`TEXT_DIM`/`TEXT_FAINT`; `ACCENT` (+
  `_LIGHT`/`_DEEP`/`_LIGHTER`); state `SUCCESS` (live/connected), `WARNING`
  (reconnecting/degraded), `DANGER`; borders `BORDER`, `DIVIDER`, `INSET_BORDER`,
  `WELL_BORDER`; field/button fills `FIELD`, `BTN`. Translucent tokens (pill backgrounds,
  accent washes, scrim) are named consts built by `theme::rgba` (a `const fn` over
  `Color32::from_rgba_unmultiplied_const`).
- **Dark only**: the former Electron app never had a light theme.
- **Fonts**: Geist Regular + Bold (text), GeistMono Regular (numbers only, never bold),
  Material Symbols Outlined (icons), all embedded from `client/assets/fonts/` (licences in
  `LICENSES/`). `theme::font(Weight::{Regular,Bold,Mono}, size)`; type scale `FS_*`.
- **Icons**: `theme::ICONS` is a sorted table of `(name, codepoint)`. Pass icon *names*
  (`.icon("close")`, `theme::icon("close")`). A name missing from the table panics in
  debug builds and renders blank in release; the test
  `theme::tests::every_icon_name_used_in_src_is_in_icons` scans `src/` for icon-taking
  calls so a typo fails `cargo test` instead. Adding an icon: add the row (keep it sorted;
  `python3 -I client/assets/fonts/icon-codepoints.py <names>` prints the lines; needs fontTools); a new icon-taking helper
  gets a row in that test's `CALLS`.
  *Why a table + test:* the old egui UI rendered unsupported glyphs as tofu boxes and the
  only check was a screenshot per icon.
- `theme::apply(ctx)` installs fonts, visuals, spacing once at startup.

## Conventions adapted from the Forza guide

- **No trailing colons on labels.** *Why:* applied inconsistently in the original; none is
  the consistent choice, and the control sits in its own slot anyway.
- **Tooltips, not helper text.** Don't put explanatory text under an option; use a tooltip
  (`info_tip`, `.on_hover_text`, or the `tip` argument of `ctrl_row`) and only when the
  label alone doesn't explain it (units, side effects, interactions). Warnings and live
  status stay visible text. *Why:* the user found text under every option bloated the UI.
- **One styled checkbox and radio.** Never a raw `egui::Checkbox`/`RadioButton`; use
  `widgets::checkbox`/`radio` so the look stays uniform.
- **Exactly the same spacing between cards** (the `card`/`grid_rows` helpers own the gap;
  don't add `add_space` above the first card, the panel margin is the top gap).
- **Reserved value width.** The value box is always the fixed `VALUE_W` slot, sized for the
  widest value, so a row never shifts as a number gains a digit.
- **A pill slot is never narrower than its pill** (2026-10-15). Size a pill's slot from
  `Pill::width(ui)`, with the mockup width (84 / 96) only as the minimum: the action bar
  uses `action_bar::bar_pill_w` (widest of every label it can show, `BAR_PILLS`, in the
  current language, so the status text does not jump when Starting turns Live), the modal
  head and `dest_row` use `max(minimum, pill width)`, the room header sizes the slot to the
  pill. The browser mirrors this with `SL(84, pill, { min: true })` (`docs/js/ui/dom.js`).
  Clipping a pill is a bug, not a fallback; `ui::tests::pill_slots_fit_every_label_in_every_locale`
  checks every bar label in en and de. *Why:* the user saw the bar's fixed 84 px slot clip
  "CONNECTING" to "CONNECTI" (2026-10-15); German labels ("ÜBERNEHME", "WECHSELT") were
  wider still. Exception: the people row's 44 px live slot only ever shows "LIVE" (and a
  text-less Starting bar).
- **`StrokeKind::Outside` for hand-painted rect strokes** (egui >= 0.31 requires an
  explicit kind; Outside matches what older egui drew). Built-in widgets use Inside and
  can't be changed.
- **`CornerRadius` is `u8`.** A computed radius (half a height, pills) must be
  `.ceil() as u8`, never a truncating cast, or the rounded end squares off.
- **Translucent fills go through named tokens** (the `rgba` const fn), never ad hoc
  `Color32::from_rgba_unmultiplied` at a call site: its blending meaning changed between
  egui versions, so a tint picked by eye can vanish after an upgrade. One place to fix.
- **Modals** are `egui::Modal`: a `SCRIM` backdrop that swallows clicks plus a centred
  shell; Esc or a backdrop click closes (`ModalAction::close`). Use `modal_shell` + `modal_footer`; don't build windows by hand.
  Stream settings is a modal (not a side panel) because the user found the side panel too
  squished.

## i18n

Every user-visible string goes through `ui::tr(key, "English fallback")` /
`tr_with(key, en, &[("name", value)])` (rust-i18n; locales `client/locales/en.yml` and
`de.yml`, German in the informal **du**-form). A missing key falls back to the English
text instead of rendering the key. Keys are dotted lowercase (`ui.sidebar.rooms`).
Check with `python3 -I client/locales/check_keys.py`: every key used in `client/src/`
exists in both locales and the `%{placeholder}` sets match. Long German labels are why the
label slot truncates and why a `ctrl_row` can move its tip onto the label.

## Verifying a style change

`--gallery` is the quick QC screen for widgets and tokens; it is **not** proof that a
screen works. Verify screens against the real app per
`superdoc/claude-instructions/testing-and-verification.md` and the client's `run` skill.
