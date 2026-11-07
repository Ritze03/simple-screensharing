# HTML replica (design sandbox)

The static single-file HTML replica of the native client in `superdoc/ui/replica/index.html`, used to try redesigns before touching egui, and the design notes taken from it. Split out of `STYLING-GUIDE.md` on 2026-11-07 (text moved verbatim).

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
those two is by splitters, so an even grid stays one double-click away. Where the code lives: `STAGE.md` "The stage",
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
not need a screen share and has its own room (replica: `S.camRoom`). **Webcam in picture** (Layout menu, per view) moves a person's
webcam into their first playing screen share. It stays a tile while they have none, and also while you do not watch the
webcam, because that tile holds its Watch button. Position and size are fractions of the parent's picture, per webcam and
per view (replica: `V().pipPos`; browser: `pipPos(key, camId)` in `docs/js/ui/stage-state.js`; native: `ViewGeom::pip`, `ui/stage/pip.rs`), and `layoutPip` clamps only what is shown to the visible picture, so Crop does not lose the Split
position. *Why the handle sits on the corner facing the picture's middle:* a PIP pushed into a corner could otherwise only
shrink, and in Floating its handle would land on the window's own. (The replica's old Classic views hide webcams; the app has them, in both clients.)
