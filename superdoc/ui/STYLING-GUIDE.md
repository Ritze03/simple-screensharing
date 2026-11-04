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
  `radio`, `slider` (`slider_detent` adds a tick + snap, used by the volume slider; `wheel_notches` counts and consumes the mouse wheel over a response, the volume slider steps with it), `value_box`, `num_box`, `readout`, `select`, `text_field`,
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
  (`VALUE_W`), spacing 4/8/12/16 (`S1..S4`), sidebar 272, modal 520/640/1120 (`PEOPLE_W` is unused since the People panel went, task I3).
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
  `Pill::width(ui)`, with the mockup width (84 / 96) only as the minimum: the modal head and `dest_row` use
  `max(minimum, pill width)`, the share menu's header sizes the slot to the pill. The browser mirrors this with
  `SL(84, pill, { min: true })` (`docs/js/ui/dom.js`). Clipping a pill is a bug, not a fallback. *Why:* the user saw the
  old action bar's fixed 84 px slot clip "CONNECTING" to "CONNECTI" (2026-10-15); German labels ("ÜBERNEHME", "WECHSELT")
  were wider still. (The bar's pill slot and `ui::tests::pill_slots_fit_every_label_in_every_locale` went with the action
  bar in task I3.)
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

## Avatar colour (cross-client contract, tasks #77 and #83, 2026-10-15)

An avatar's background is a **pure function of the person's display name**, so one person has the same colours
in every room, for every viewer, on both clients, across restarts (the old scheme picked one of five gradients
by connection id, so the colours changed every reconnect). *Why:* user request 2026-10-15 ("fixed, generated
based on the name ... go through the whole hue"). It is a **140deg gradient** (light top-left to dark bottom-right),
like the old avatars: *Why (#83, user 2026-10-15):* #77 had made it flat and the user wanted the fade back ("you weren't
supposed to remove the fade ... there should still be a fade in the background"), with the colours still from the name.
The native and browser clients must compute **exactly**:

1. key = display name, `trim()`med, Unicode-lowercased (`str::to_lowercase` / `toLowerCase()`), as UTF-8 bytes;
2. `h` = FNV-1a 32-bit over the bytes (offset 2166136261, prime 16777619, wrapping u32 / `Math.imul(..) >>> 0`);
3. `hue = h % 360`; stop **A** = HSL(hue, **60 %**, **36 %**), stop **B** = HSL((hue + 350) % 360, **60 %**, **24 %**)
   (i.e. 10 degrees back, darker), each converted with the CSS Color 4 formula
   (`a = S*min(L,1-L)`, `f(n) = L - a*max(-1, min(k-3, 9-k, 1))`, `k = (n + hue/30) % 12`, channels `round(255*f)`);
4. painted as `linear-gradient(140deg, A, B)` (CSS gradient-line length, sRGB interpolation), white bold initial.

*Why S/L 60 / 36 / 24 and -10 degrees:* the white initial needs >= 3:1 (WCAG, bold text) against the **lighter** stop on
every hue, and yellow (60 degrees) is the worst: L_A = 36 % gives 3.26:1 (37 % would give 3.10, 40 % 2.68). B at 24 %
(>= 6.4:1) gives a clearly visible fade; the small hue drift copies the old gradients (green 140 -> 150, orange 15 -> 0,
purple 272 -> 265, yellow 47 -> 41). A single S/L for all hues keeps the palette even. Both test suites check both stops
on all 360 hues (and that A is the lighter one). *Consequences:* a person who renames gets new colours; two names can share
a hue (1 in 360); an empty name hashes to hue 61. Vectors, asserted by **both** test suites (`client/src/theme.rs`,
`docs/dev/tests.js`):

| name | hue | stop A | stop B |
| --- | --- | --- | --- |
| `Ritze` | 177 | `#25938d` | `#186252` |
| `mo` (also `  Mo `) | 245 | `#2e2593` | `#181f62` |
| `client-1` | 144 | `#259351` | `#18622a` |
| `client-2` | 321 | `#93256c` | `#621854` |
| `Zed` | 4 | `#932c25` | `#621820` |
| `user-d740` | 328 | `#93255f` | `#62184c` |
| `Größe` | 314 | `#932579` | `#62185d` |
| `` (empty) | 61 | `#919325` | `#625718` |

Code: native `theme::avatar_colors(name) -> [A, B]` / `avatar_hue`, `theme::AVATAR_GRADIENT_DEG`, drawn by
`widgets::paint_avatar(.., colors: [Color32; 2], ..)` with `paint_gradient` (a tessellated rounded rect with per-vertex
colours, egui has no gradient fill); browser `docs/js/ui/dom.js` `avatarColors` / `avatarBg` (stops computed to the 8-bit
hex and set inline as `background:linear-gradient(140deg, A, B)`, not left to CSS `hsl()`). Checked 2026-10-15 from the
rig and headless Chromium: sampled pixels of both match the analytic gradient within 1/255 per channel. Every avatar
(user band, People rows, share cards, "you" card) passes the owner's display name, never a connection id.

**Initial centring (native, #83).** `paint_avatar` does not use `paint_text`: that centres the *line box* (ascent +
descent) and egui then snaps the text to whole pixels independently of the disc, so the letter drifted by 1 px with the
avatar's sub-pixel position (up to 2.5 px for "Ä" at 1.25x; the user saw "R" 1 px low in the user band). Now the disc
rect is snapped first, the initial is centred horizontally on its advance box (a capital's side bearings are balanced;
the glyph quad has a faint AA column on one side and centring it put the letter +1 px right) and vertically on the
glyph quad (`Galley::mesh_bounds`: cap top to baseline, umlaut dots and descenders included), with an odd leftover
pixel going **up** (optical centre). `widgets::tests::avatar_initial_is_centred_on_the_disc_at_any_position_and_scale`
tessellates the avatar at fractional origins and 1/1.25/1.5/2x and asserts it. The browser centres with flexbox on
sub-pixel positions and measured centred (no change).

## i18n

Every user-visible string goes through `ui::tr(key, "English fallback")` /
`tr_with(key, en, &[("name", value)])` (rust-i18n; locales `client/locales/en.yml` and
`de.yml`, German in the informal **du**-form). A missing key falls back to the English
text instead of rendering the key. Keys are dotted lowercase (`ui.sidebar.rooms`).
Check with `python3 -I client/locales/check_keys.py`: every key used in `client/src/`
exists in both locales and the `%{placeholder}` sets match. Long German labels are why the
label slot truncates and why a `ctrl_row` can move its tip onto the label.

## HTML replica (design sandbox, 2026-11-03, task #131)

`superdoc/ui/replica/index.html` is a static, single-file HTML replica of the native client (fake data, no
backend) for trying redesigns before touching egui. *Why (user request 2026-11-03):* "build an artifact that is
exactly like the current GUI, so we can do modifications to it and actually make it nice". Its CSS tokens at the
top are named after the `theme.rs` constants (Rust name in a comment), so a change there maps back one line at a
time. The nav's **Classic** views are the current app (room watching / cards / sharing live / nobody sharing,
Watched streams, fullscreen stream); modals: Set up your stream, Stream settings, Settings, Create or join. Those
were matched against rig screenshots at 1920x1080 (the "Rig data" set uses the rig's names for side-by-side). It is
authored as a claude.ai artifact fragment (no `<!doctype>`/`<html>`, the publisher wraps it); open it locally inside
a doctype wrapper or the browser uses quirks mode. It is a copy, not a source of truth: when the egui look changes,
the replica drifts until someone updates it.

The default **Stage** views are the app since task I1 (the stage) and I3 (strip, control bar, menus, pop-out, fullscreen
chrome; 2026-11-04): a black stage with Split / Crop / Floating layouts, a people strip with avatars and a grouped control
bar. *Why (user request):* the middle part should work like a voice-call stage, with the people list, stream volume and
settings out of the way until asked. Its tokens are under "stage redesign" at the top of the replica and now have `theme.rs`
constants (`STAGE_*`, `STRIP_*`, `STAGE_CHROME`, `OVL_NUM`). Decisions worth keeping: a popped-out stream shows **only** as a
badge on its owner's avatar (click brings it back); the green ring means audible shared audio, because the app has no voice
chat; Split and Crop share one grid (row count that shows the most picture area), Crop only fills the cells; resizing in
those two is by splitters, so an even grid stays one double-click away. Where the code lives: `APP-SHELL.md` "The stage",
"The control bar", "The people strip and hover card", "Menus and dropdowns", "The whole stage can pop out". The **Classic**
views (the pre-stage layout) are not in the app any more: the replica keeps them only for side-by-side comparison.

**Watched** (Stage group, #136, 2026-11-03) is the same stage for every stream you watch, from all rooms. *Why (user
request):* "the same view should be on the watched streams section ... everything combined". A stream's key is
`room/owner/label` (your own share `room/me`), because one person can be in two rooms and share several streams; each
room and Watched keep their own layout state (`V()`), while watch, pop-out, volume and stats belong to the stream and
are shared. Watched has no Invite, Room settings or Leave, and shows Share and audio only while you have a share to act
on (a new share starts from a room, so no room picker). Several streams of one person are one avatar with a count.

**Emoji avatars** (#136, built in #106 / I6, 2026-11-04): a person can pick an emoji (Settings > Profile) that replaces the initial on a
background taken from the emoji; `avatar()` is the one helper (native: `ui::paint_person` -> `widgets::paint_avatar_emoji`, browser `dom.js` `avatarEl`).
The relay carries the emoji (`protocol-spec.md` §9.2); a peer or relay without it shows the initial.
Rendering rules (rewritten 2026-11-04, row 120, after the user tested the real client; native unless the browser is named):
- **Size:** **0.62 x the avatar's size** (`EMOJI_AVATAR_FRACTION`, the replica's `font-size: .62 x --av`), in both clients.
- **Centring:** native puts the emoji's **balance point** on the disc's centre: the midpoint of the solid pixels' (alpha >= 128) mass centre and the centre of their smallest
  enclosing circle (`emoji.rs` `balance_point`). *Why:* centring the crop box is what the first version did, and a box's centre is only the middle of the shape's extremes: the
  pizza slice ("doesnt seem to work for all icons") has its weight on the right and nothing in its lower left, so it sat visibly off with a perfectly centred box. The mass
  centre alone overshoots (🌙, 🎸); the enclosing circle alone follows thin parts (🎮 rises 2 px). The midpoint looked right on all 40 at 32 px. The crop itself (alpha >= 16)
  only trims the texture. The **browser still centres its bounding box** (`dom.js` `emojiInfo`), so it keeps the old pizza.
- **Outline:** a **1 px black outline** (2 device px at `pixels_per_point` 2) that follows the emoji's shape, to lift it off the background. Native dilates the scaled alpha with a
  round kernel and composites it behind the emoji **in the same texture** (`emoji.rs` `bake`), one quad; the browser stacks four `drop-shadow(1px 0 0 #000)` filters
  (`.avatar .emo`), which make the same 3 x 3 dilation. *Why baked, per drawn size:* the first version drew one half-size texture five times; sampled down about 3 to 1 without
  mipmaps it gave a broken stair-step line and a grainy picture. The picker's grid has no outline, like the replica.
- **Background:** the emoji's strongest hue as the usual two-stop gradient; a **colourless** emoji (the skull) gets the neutral **slate**
  `hsl(222, 14 %, 36 %)` to `hsl(222, 14 %, 24 %)`.
- **Source of the pictures:** native embeds **Noto Color Emoji** itself (`client/assets/emoji/`, CBDT bitmaps, OFL 1.1; egui cannot draw colour emoji and the AppImage / Flatpak /
  Windows builds cannot rely on a system emoji font), so any emoji the font has draws, a flag, a skin tone, a ZWJ sequence or a keycap included (rustybuzz shapes them, they are
  GSUB ligatures). The browser draws the system's emoji font.
- **What shows the initial:** the string comes from a peer, so anything that is not **one grapheme shaped to one bitmap, non-ASCII, at most 16 bytes** (`is_drawable`,
  `emoji_glyph`): text, two emoji, a digit (the font has plain digit bitmaps for its keycaps), an emoji newer than the font, a ZWJ sequence with no ligature.
- **Cost and bounds:** shaped, decoded and measured once per emoji, baked once per drawn size; the cache in `ctx.data` holds **128 emoji** (least recently drawn goes; negative
  results count) with **8 sizes** each, so a peer cycling through strings cannot grow it. Nothing runs per frame and nothing at start-up (the font is parsed on first use).
- **The avatar popover** (Settings > Profile > Avatar, `modals.rs` `avatar_row`): the app's menu frame (`menu::frame()`), **about 280 px wide** (8 cells of 32 px
  + 7 gaps of 2 px + the frame's 12 px = 282), opening under the avatar button. A **44 px header** (`HBox`, slots `[fixed(32), FILL]`) holds a **32 px avatar**
  and a **two-line title** ("Your avatar" / "Friends see it instead of your initial"), then the 8 x 5 grid, the **"Paste any emoji" field** with the replica's `.emoji-hint` line
  under it, then "Use my initial". A grid item closes the popover. The field works like the browser's `emojiMenu`: on every change it takes the **last drawable grapheme** and applies
  it, Enter closes; text without one warns "That isn't an emoji we can draw", an emoji over the wire's 16 bytes (a family) warns "too long to send" and is not applied.
  *Why the native field empties once it took an emoji* (the replica keeps it selected): the field is egui text, which has no colour emoji and shows a flag or a ZWJ sequence as loose
  symbols; the header's avatar is the feedback.
- **The hover ring** of the avatar button (1.5 px, 2 px off the disc, accent while the popover is open) is painted from the row's parent `ui`, after the row. *Why:* an `HBox` cell clips to
  its own rect, which is as tall as the disc and starts at its left edge, so the ring painted inside it lost its top, bottom and left (the user's "Avatar outline borked"). Anything that
  overhangs a cell belongs outside the cell.

**Fullscreen chrome** (#136, native `stage/chrome.rs`; the older per-stream twin is `popout.rs` `OVERLAY_IDLE`): in fullscreen the strip and bar lie over the stage, so
the streams do not jump. *Why (user request):* they should be out of the way: shown only while the pointer is over the
stage, gone after 3 s without movement. An open menu or a drag cancels the timer and keeps them up even off the stage;
when it ends they get a full 3 s again, so closing a menu never hides the bar at once. Outside fullscreen they always show.

**Stream settings and webcam** (#137, 2026-11-04; built in I8-I10, both clients). *Why (user request):* simpler stream
settings and a webcam next to the screen share. The modal's two right-hand cards keep their widgets and are renamed and
reordered: **Compression** (Codec, Bitrate; the user wanted something other than "Codec" as the title) and **Video** (FPS
dropdown, Preset). The presets replace Balanced and Advanced tuning: Text = 15 FPS, Quality encoding, keyframe 250 ms;
Quality = 60 FPS, Quality, 1000 ms; Gaming = 120 FPS, Performance, 250 ms. A preset sets the FPS once; changing the FPS by
hand keeps its encoder mode and keyframe. The share dropdown shows the same state. The
Source card's Change button is there while live too.

The webcam is a stream of its own (`<room>/me/cam`, `cam: true`) with a split button in the control bar: camera, its v4l2
modes (changing camera resets the mode), codec and bitrate; the encoder is fixed to Performance, keyframe 2000 ms. It does
not need a screen share and has its own room (`S.camRoom`). **Webcam in picture** (Layout menu, per view) moves a person's
webcam into their first playing screen share. It stays a tile while they have none, and also while you do not watch the
webcam, because that tile holds its Watch button. Position and size are fractions of the parent's picture, per webcam and
per view (`V().pipPos`), and `layoutPip` clamps only what is shown to the visible picture, so Crop does not lose the Split
position. *Why the handle sits on the corner facing the picture's middle:* a PIP pushed into a corner could otherwise only
shrink, and in Floating its handle would land on the window's own. (The replica's old Classic views hide webcams; the app has them, in both clients.)

## Audio controls (task I7, 2026-11-04)

Built from the replica's `audioModal` and bar; the rules are the stage's (split button, `menu::select`, popover frame), so only what is specific is listed:

- **Audio button** (a split button in the bar, `stage/bar.rs`): off = `music_off` icon, on = `music_note` in the live green (tooltip names the source); a click is Desktop audio on / off,
  the arrow opens the audio menu. It needs no screen share and stays enabled.
- **Audio menu** (`share::audio_menu`, a `PopoverKind::Bar` popover): `pop_item` rows with icon, name and a sub-line, **group headings** (Games, Browsers, Media, Voice chat,
  Devices, Other), a live **level meter** at the end of each source row (running only while the menu is open), a separator, the **Mute shared audio** switch row (`menu::switch_row`)
  while sharing, and **All audio settings…** (`tune` icon).
- **Audio settings menu** (`audio_settings.rs`, a modal of the stream settings' family): cards **Source** (Audio dropdown), **Volume** (the **Boost** slider, 50-200 %;
  **Boost is no longer in Settings**, the Settings > Shared audio card is gone) and **Desktop audio** (the **Exclude** list). Footer `[Stop sharing audio | Share desktop audio][fill][Done]`.
- **Exclusion list and its Add popover:** each entry is a text row with a Remove button; **Add** opens a popover (`PopoverKind::AudioExclude`) listing "Detected audio sources" (one click adds
  one; "Every detected source is excluded" when none is left) and an "Or enter a name" field with its own Add button.
- All strings are `ui.audio.*`, `ui.audio_settings.*`. Rig names: `audio.*` in `client/scripts/README.md`.

## Verifying a style change

`--gallery` is the quick QC screen for widgets and tokens; it is **not** proof that a
screen works. Verify screens against the real app per
`superdoc/claude-instructions/testing-and-verification.md` and the client's `run` skill.
