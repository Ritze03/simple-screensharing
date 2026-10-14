# Pop-Out Stream Viewer

## Reversal: hover-gate the stat bar AND the × close button too (2026-07-29)

Follow-up to (and partial reversal of) the "prominent stats" redesign recorded further down
this file and in `view.css`'s `.media-tile-statbar` comment: the owner explicitly asked, twice,
for the opposite of "always visible, no hover needed" — "make sure that all of the elements
disappear, if the window/area is not hovered... If popped out, the info overlay is also always
visible and the cross/close symbol is also always visible" (named directly as the bug, not a
misunderstanding to push back on).

**What changed, this window specifically** (`frontend/styles/viewer.css`):

- `.media-tile-statbar` — already hover-gated to opacity 0/1 in `view.css` (shared with the View
  tab); this window needed its own `.viewer-tile:hover .media-tile-statbar` /
  `:focus-within` rule added, same pattern as the pre-existing vignette/overlay rules, since this
  window's hover trigger class is `.viewer-tile`, not `.media-tile`.
- `.viewer-exit-btn` (the × close button) — was always `opacity: 1`, no hover-gating at all.
  Now `opacity: 0` at rest, fading in on hover/focus, same `0.12s ease` transition the rest of
  the overlay chrome uses. **The interesting part**: this button is NOT a descendant of
  `.viewer-tile` — `setupExitControls` (`viewer.js`) appends it via
  `document.getElementById("viewer-root").before(...)`, making it a *preceding sibling* of the
  tile grid, not a child of any tile. A CSS combinator can't reach backward from `:hover` to a
  preceding sibling, so `.viewer-tile:hover .viewer-exit-btn` cannot work as written. Rather than
  restructure the DOM (the button is deliberately outside the grid so it survives regardless of
  tile count/layout), this window's `<body>` fills the entire viewport (`html, body { width:
  100%; height: 100%; }`, already the case), so `body:hover`/`:focus-within` is functionally
  "the pointer is anywhere in this window" — exactly the trigger the button needs — and reaches
  it as a normal descendant selector. Verified live (Playwright `_electron`): hovering a point
  clearly outside the tile's own bounding box but still inside `<body>` reveals the × button
  while correctly leaving the tile's own statbar/vignette/overlay hidden (those still require
  hovering the tile itself, per their own `.viewer-tile:hover` rules) — the two hover surfaces
  are independent, as intended.

Also rounded/resized the View tab's pop-out/stop-watching buttons and this window's × button to
match — see `superdoc/features/stream-control.md`'s matching entry for the full write-up
(shared CSS/measurement, recorded once there to avoid duplicating the same paragraph in both
docs).

## Three overlay-parity bugs fixed: slider position, button restyle, focus-within stuck-open (2026-07-29)

Follow-up to this same day's entry below (which gave the pop-out window the View tab's real
`.media-tile-*` overlay DOM/CSS) — three bugs the port introduced, all in
`frontend/scripts/viewer.js` / `frontend/styles/viewer.css` / `frontend/styles/view.css`.

**Bug 1 — volume slider rendered top-left instead of bottom-left.** `viewer.js`'s
`renderTile` only ever appended ONE child (`.media-tile-overlay-bottom`) into
`.media-tile-overlay`, since this window intentionally has no pop-out/stop-watching buttons
(the "top" group `view.js` renders). But `.media-tile-overlay` is `flex-direction: column;
justify-content: space-between`, and `space-between` collapses to `flex-start` with only one
child — there's nothing to distribute space *between*. Fix: append an empty
`.media-tile-overlay-top` spacer alongside `bottom`, exactly mirroring what `view.js` always
does, restoring the two-item distribution `space-between` needs. No `view.css` change needed.

**Bug 2 — restyled the pop-out/stop-watching buttons and the pop-out window's × button to one
consistent look**, per the owner directly: "black button with outline and blurred
background". Previously `.media-tile-popout-btn` (`view.css`, shared by `.media-tile-stop-btn`)
was a pale near-transparent frost with no border, and `.viewer-exit-btn` (`viewer.css`) was a
black circle with no border and no blur — neither matched the target, and the two windows'
"exit this view" affordances looked different from each other. Both now use the same recipe:
`background: rgba(16, 17, 20, 0.55)` (translucent `--tile-bg` black, the same video-surface
black used elsewhere — `#101114` in decimal), `border: 1px solid rgba(255, 255, 255, 0.16)`
(a visible hairline so the chip reads as a distinct control against the video), and the
existing `backdrop-filter: blur(6px)`. `.media-tile-popout-btn:hover`/`.viewer-exit-btn:hover`
darken to `rgba(16, 17, 20, 0.75)`; `.media-tile-stop-btn:hover`'s existing
`background: var(--danger)` override is untouched and still reads correctly against the new
darker base. `.viewer-exit-btn` keeps its own shape/size/position (circular, fixed top-right).

**Bug 3 — the overlay/vignette never faded out in the pop-out window**, confirmed live (not
assumed) to be a classic `:focus-within` stuck-open case, not a `:hover` bug: after dragging
the volume slider then moving the mouse away, `document.activeElement` was still the
`<input class="media-tile-audio-slider">` and `getComputedStyle` on `.media-tile-overlay`
showed `opacity: 1` at rest. This window's tile fills virtually the whole viewport with no
other focusable chrome, so a slider that keeps browser focus after a mouse drag ends pins
`.viewer-tile:focus-within` true forever, independent of `:hover`. Fix: blur the slider on
`mouseup` (not on every `input` tick) — releases focus once a *mouse-driven* drag ends, but
never fires for a keyboard interaction (no `mouseup` for that), so a sighted keyboard user who
tabs to the slider still keeps it focused and visible. Confirmed live both ways: after a CDP
mouse drag-then-move-away, `activeElement` becomes `<body>` and both overlay/vignette opacity
drop to `0`; after a *programmatic* `.focus()` (simulating a keyboard tab) with the mouse away,
the slider stays focused and overlay opacity stays `1`, and a subsequent `ArrowRight` keypress
neither blurs it nor hides the overlay.

`Why real Electron over mock+browser, and how the pop-out window was reached without an
interactive OS capture flow:` verified via Playwright's `_electron` driver against the real
built `src-tauri` sidecar (per this repo's testing-and-verification rule) — the real pop-out
`BrowserWindow`, real `preload.js`, real backend for every command except one: driving a full
live two-peer relay session purely to get *a* stream resolvable would have been disproportionate
for CSS/DOM-layout bugs, so the harness opened the pop-out window via the real
`window.__ELECTRON__.openPopout(...)` IPC path with a fabricated stream key, and stubbed only
that one fabricated key's `stream_video_url` response (via a `-r`-injected hook wrapping
`ipcMain.handle('tauri-invoke', ...)` — `NODE_OPTIONS` doesn't reach Electron's main process
through Playwright's launcher, which deletes it, but `-r` as an `electron.launch` arg does)
so `renderTile` would actually run; the resulting tile's own `set_volume` calls still hit the
real backend unchanged (harmlessly erroring, same as any other unresolvable stream, already
handled by `pushVolume`'s existing `try`/`catch`). Also discovered live: this environment's
tiling WM (Hyprland) doesn't reliably honor `BrowserWindow.setPosition`'s requested screen
coordinates (a real X11 `xdotool` cursor move to the computed on-screen tile center did not
register as a hover) — consistent with the F11/fullscreen positioning limitation the
2026-07-29 entry below already recorded for this same session infra — so hover/mouse-away
verification stayed on Playwright's CDP-synthetic mouse (proven to correctly drive real
`:hover` state when the target coordinate is on-page) rather than real OS input. Also ran the
full `cargo test --workspace`: 181 + 12 passed, 0 failed, 22 + 4 ignored (pre-existing).

## Pop-out tile now reuses the View tab's real `.media-tile-*` overlay (2026-07-29)

Closed the last drift gap the 2026-07-28 entry below left open: that entry gave the pop-out
window's tile *functional* parity (nameplate, fps, mute/volume) but still through its own
hand-copied `.viewer-tile-*` CSS vocabulary, independently maintained from `view.css`'s
`.media-tile-*` rules — no shared always-visible statbar pill, no vignette layer, and
`viewer.html` didn't even link `tokens.css` (so `viewer.css` had zero access to the app's
design tokens, confirmed via a `var(--...)` grep coming back empty). The owner's ask: "A
popped out window should have the same overlay as a stream inside of the 'View'-view."

**What changed**, in `frontend/viewer.html` / `frontend/scripts/viewer.js` /
`frontend/styles/viewer.css`:

- `viewer.html` now links `styles/tokens.css` (design tokens) and `styles/view.css` (the
  real `.media-tile-*` rules) directly, in addition to its own `styles/viewer.css`. Chose
  **linking the real stylesheet over copying rules again** — copying is exactly what caused
  this drift the first time (2026-07-28's `.viewer-tile-*` vocabulary silently fell behind
  `view.css`'s "prominent stats" redesign that same day). `view.css` turned out to be clean
  enough to link wholesale: its `.media-tile-*` rules don't leak anything that conflicts with
  this window's own chrome (confirmed — `.media-tile` itself, with its `aspect-ratio: 16/9`
  and green "live" ring, was deliberately **not** applied to this window's tile wrapper; see
  next point).
- `viewer.js`'s `renderTile` now builds the actual `.media-tile-statbar`/
  `.media-tile-vignette`/`.media-tile-overlay`/`.media-tile-audio*` DOM `view.js`'s own
  `renderTile` builds — same dot/name/resolution/fps/bitrate/audio-badge statbar, always
  visible; same hover-only vignette + overlay. The tile *wrapper* stays this window's own
  `.viewer-tile` class (not `.media-tile`) on purpose: `.media-tile` carries a fixed
  `aspect-ratio: 16/9` and a green live-ring `box-shadow` sized for the View tab's grid
  cells, both wrong for this window's full-bleed letterboxed single-stream stage. Since
  `view.css` gates the vignette/overlay's hover-reveal behind `.media-tile:hover`/
  `:focus-within`, `viewer.css` now carries two small additional selectors —
  `.viewer-tile:hover .media-tile-vignette`/`.media-tile-overlay` (and `:focus-within`
  equivalents) — that hook the same reused classes to *this* window's own hover trigger,
  without duplicating any of their declarations.
- Live bitrate is new here: `tileMeta` never carried it before, and there was no
  `stream-stats-updated` listener in this window at all (only the main window's `shell.js`
  had one, fanning it out to `view.js`'s `updateStats`). `viewer.js` now registers its own
  `window.__TAURI__.event.listen("stream-stats-updated", ...)` (this window shares the same
  `preload.js`, so `window.__TAURI__.event` is available) and patches the statbar's bitrate
  span the same way `view.js` does. Fps stays on the existing `tile.onFps` callback path
  (owned by this window's own `TileVideo` controller), unchanged.
- **Pop-out button: dropped entirely** — popping out again from inside the pop-out window is
  nonsensical. **"Stop watching": also dropped, not replaced** — closing this window (the ×
  button, or the OS close affordance) already fully stops watching this stream, so a second
  control doing the same thing would be redundant. `overlay-top` is simply never appended in
  this window's `renderTile`, unlike `view.js`'s (which still renders both buttons there).
- The × close button, F11 fullscreen toggle, and Escape-only-exits-fullscreen behavior are
  all untouched by this change (no lines in `setupExitControls`/`setFullscreen` touched) —
  re-verified live afterward anyway, see below.

`Why real Electron over mock+browser:` per this repo's testing-and-verification rule, verified
live via Playwright's `_electron` driver against the real built `src-tauri` sidecar and a
real relay server (`simple-screensharing-server`, launched on a scratch port for this pass)
— a real room, a real self-share ("Share another screen" → "Go live"), watched from the
"Not on screen" rail, then popped out. Confirmed: the pop-out window's statbar showed the
same dot/name/room/resolution/fps/bitrate the View tab's tile showed (`453 kbps` etc., live
and updating); `getComputedStyle` on `.media-tile-vignette`/`.media-tile-overlay` showed
`opacity: 0` at rest and `opacity: 1` on hover, confirming the hover-trigger wiring actually
works, not just "renders once"; the main window's tile correctly went to a held slot while
popped out and correctly returned to normal rendering when the pop-out was closed via its ×
button (window count 2 → 1). F11/Escape: exercised both via keypress and, to rule out a
keypress-delivery issue, via a direct `window.__ELECTRON__.setFullscreen(true)` IPC call —
in both cases `BrowserWindow.isFullScreen()` stayed `false` in this run (window did move to
`(0,0)` but didn't resize to the monitor's full dimensions). This reads as an environment
limitation of this particular concurrent/multi-agent Xwayland session (a matching
2026-07-28 entry below records a *previous* pass where `isFullScreen() === true` was
confirmed with the identical mechanism), not a regression — this change's diff never touches
`setupExitControls`/`setFullscreen`/`main.js`'s fullscreen IPC handlers at all. Escape did
correctly never close the window either way (window count stayed at 2 throughout both F11
attempts), which is the safety-critical half of that contract. Also ran the full `cargo test
--workspace` suite (this is a frontend-only change, but per the required-verification
checklist): 181 + 12 passed, 0 failed, 22 + 4 ignored (pre-existing, require `--ignored` or
real hardware, unrelated to this change).

## Auto-return on close, and full stats/volume parity in the pop-out window (2026-07-28)

Two gaps this file's own header/comments used to flag explicitly as known limitations —
both closed:

1. **Closing a pop-out window now automatically clears the main View tab's held slot.**
   Previously (see the "Deliberately not built here"/held-slot notes elsewhere in this file
   and `view.js`'s old header comment) there was no IPC signal for "the pop-out window
   closed", so a held dashed-border placeholder only cleared via the tile's own "Bring it
   back" button — closing the pop-out window itself left the main grid stuck. Fixed with a
   real signal: `electron/main.js`'s `openPopoutWindow` now sends `webContents.send(
   "popout-closed", streamKey)` to the main window from inside the pop-out `BrowserWindow`'s
   own `'closed'` event handler (fires for *any* close — the × button, the OS window-frame
   close control, etc.), `electron/preload.js` exposes it as `window.__ELECTRON__
   .onPopoutClosed(callback)`, and `view.js` registers a listener once at module load (not
   inside `init()`, which re-runs on every `refresh()` and would otherwise stack a duplicate
   listener per refresh) that deletes the closed stream's id from `poppedOutIds` and
   re-renders. `renderTile` already fell through to normal (non-held) rendering for any id
   not in `poppedOutIds`, so no new rendering logic was needed — only the missing signal.
2. **The pop-out window now shows the same nameplate/fps/volume info the main grid's tile
   overlay shows**, not just the bare video. `view.js`'s `popOutWatch` now passes
   `ownerLabel`/`roomLabel`/`hasAudio`/`resolution` alongside `streamKey` into
   `window.__ELECTRON__.openPopout(...)`; `openPopoutWindow` forwards them as extra query
   params on the `viewer.html` load (same mechanism the existing `streamKeys` param already
   used — see "Data flow" below) rather than having `viewer.js` re-derive them itself (that
   would mean re-fetching `room_detail` and re-joining against `members`/`streams`, logic
   `view.js`'s `fetchWatchedStreams` already did once). `viewer.js`'s `renderTile` builds the
   same nameplate + hover-reveal overlay (fps span, and — only when `hasAudio` is true — a
   mute/volume pill wired to the real `commands::set_volume`, mirroring `view.js`'s own
   `buildAudioGroup` swap-in-place-on-toggle pattern) that the main grid's `.media-tile`
   shows, styled via new `.viewer-tile-*` rules in `frontend/styles/viewer.css` (copied/
   adapted from `view.css`'s `.media-tile-*` rules rather than shared via a `<link>`, since
   this window intentionally loads no other app-shell CSS). Kept "lean, chrome-free until
   interacted with", matching the main grid: the nameplate is always visible, the fps/audio
   overlay only reveals on hover/focus.

`Why fabricated test data instead of a real capture stream to verify this live:` verified
against the real Electron app (real `main.js`/`preload.js`/`view.js`/`viewer.js`, real
`BrowserWindow` creation and `'closed'` event, real DOM clicks — not shortcut function
calls) with a scratch fake sidecar standing in for the real Rust backend. A real capture
needs either a second real peer over a real relay server, or a real interactive
xdg-desktop-portal `ScreenCast` consent grant — the latter's monitor-picker UI is
compositor-drawn and could legitimately render across every connected output (including one
carrying a real, unrelated live session) to let the user choose one, which wasn't a risk
worth taking to script blind. The fake sidecar returned a fabricated-but-realistically-shaped
joined room with one live video+audio stream, so the real frontend/Electron code under test
ran against realistic data with zero screen-capture risk — confirmed live: popping out
showed the real nameplate/fps-span/mute-button/slider in the pop-out window with the correct
query string, the main tile went to a held slot, and closing the pop-out via its real × button
returned the main tile to normal rendering with no further action taken. Actual H.264 frame
decoding (which needs a real producer/capture) was not exercised in this pass — the pop-out
window correctly showed its own "Stream error" placeholder against the fake, unreachable
video URL, same as a real decode failure would look.

**Added 2026-07-28. Consolidated onto this shape later the same day — see the "2026-07-28:
consolidation" section below for the full before/after and why.** A standalone,
chrome-minimal Electron window that shows exactly one watched stream, independent of the
main app window's tabbed shell. Lives in `simple-screensharing-client`; only exists
post-Electron migration (`superdoc/architecture/overview.md`'s "Process model" section) —
Electron's multi-`BrowserWindow` support is what makes a *real* OS-level fullscreen window
possible at all.

## What it does

Each tile in the View tab's Watch grid has a "Pop out" button
(`frontend/scripts/view.js`'s `popOutWatch`). Clicking it opens a lean, chrome-free
`viewer.html` window for exactly that stream — or, if that stream is already popped out,
focuses the existing window instead of opening a duplicate. **One independent window per
stream**, not a singleton: popping out a second stream never closes the first, and any
number of streams can be popped out simultaneously. Each window reuses the exact same
decode pipeline the main window's tiles use (`superdoc/features/video-pipeline.md`).

A pop-out window always opens as a normal, resizable, OS-framed window (chrome-free means
no *app-shell* chrome — no nav tabs/sidebar — not no OS window frame). **F11** inside it
toggles real OS fullscreen (EWMH `_NET_WM_STATE_FULLSCREEN` on X11 / `xdg-shell` fullscreen
on Wayland, *not* Electron's more aggressive kiosk mode). Exiting fullscreen — via F11
again, Escape, or an OS-level fullscreen-exit gesture (WM shortcut) — returns the window to
normal windowed pop-out and **never closes it**; only the small `×` exit button (or the
window's own OS close control) closes the window. An OS-level fullscreen exit is detected
via Electron's `leave-full-screen` window event and forwarded to the renderer
(`viewer-left-fullscreen`) purely to keep UI state in sync — that handler does not, and
must not, call close.

## Architecture

| Piece | Role |
| --- | --- |
| `electron/main.js` | Owns `popoutWindows`, a `Map<streamKey, BrowserWindow>` — one entry per popped-out stream, not a singleton. `ipcMain.handle` registers `open-popout-window`, `viewer-set-fullscreen`, `viewer-close`. `openPopoutWindow(streamKey)` focuses an existing window for that key or builds a new `BrowserWindow` and loads `frontend/viewer.html`. The fullscreen/close handlers resolve *which* window to act on via `BrowserWindow.fromWebContents(event.sender)` — the calling renderer's own `WebContents` — rather than any single module-level reference, which is what lets every pop-out window independently fullscreen/close itself with no key threaded through the IPC call. |
| `electron/preload.js` | Exposes `window.__ELECTRON__` — a **separate namespace from `window.__TAURI__`**, deliberately: `__TAURI__` is a narrow Tauri-`invoke`/event compatibility shim for talking to the Rust sidecar, while this is Electron-native window management with no Tauri equivalent at all. The same preload script runs inside every pop-out window too (`main.js` passes the identical `preload` path), so both `window.__ELECTRON__` and `window.__TAURI__` are available there — `viewer.js` can call back into `__TAURI__` for stream data (`stream_video_url`) with no new backend contract needed. |
| `frontend/viewer.html` / `frontend/scripts/viewer.js` | The pop-out window's actual content: a `ceil(sqrt(n))`-column tile grid (still generic over N tiles — shared, already-tested code — even though every pop-out window today always shows exactly one), an exit button, F11/Escape handling, and per-tile `window.TileVideo.startTileVideo` calls. Deliberately **not** part of `index.html`'s tabbed app shell — a separate, focused page for exactly one purpose, loaded into its own `BrowserWindow`. |
| `frontend/scripts/tile-video.js` | The shared decode module (`window.TileVideo`) both `view.js` (main window) and `viewer.js` (pop-out window) call into, so the two never drift into two different decode implementations. See `superdoc/features/video-pipeline.md`. |
| `frontend/scripts/view.js` | Owns `poppedOutIds` (which tiles are currently popped out, for the held-slot placeholder) and the per-tile "Pop out" button that calls `window.__ELECTRON__.openPopout({ streamKey })`. |

## Data flow: a stream key, not a raw tile object, crosses the window boundary

`view.js` passes a `streamKey` — `"room_handle:owner:stream_id"` (this project's existing
tile-id scheme; `owner` is a lowercase-hex `ConnId` string with no colons in it, per
`dto::hex_u64`, so a plain 3-way split on `:` is safe) — not a resolved video URL or tile
object. `electron/main.js` passes it through unread, wrapped in a one-element array, as an
opaque JSON-encoded query-string parameter (`?streamKeys=...`) on the `viewer.html` load;
`viewer.js` parses it and calls the real `stream_video_url` command (via `window.__TAURI__`,
the same command `view.js`'s own `fetchWatchedStreams` uses) to resolve it to a video URL,
once loaded.

`Why a query string instead of an IPC message sent after the window loads:` the query
string is available synchronously to `viewer.js`'s own startup code, with no race against
when the renderer attaches a listener for a postload IPC message — `main.js`'s own comment
on `openPopoutWindow` calls this out explicitly.

## Known platform-specific traps (Linux, found and fixed against real window managers)

Found and fixed by testing against a real Hyprland/dwindle tiling session and real
X11/XWayland behavior while this was still the fullscreen/borderless *display-takeover*
window described further down — the tiling-WM mitigation this section used to describe was
removed as part of the 2026-07-28 consolidation (a normal-sized, normal-positioned pop-out
window doesn't fight a tiling WM the way a window trying to claim exact display bounds
does), but the two general Linux/Electron facts below are still real constraints worth
knowing if this code is touched again:

- **Positioning race**: calling `setFullScreen(true)` before a window is positioned/shown
  is unreliable on Linux WMs and can fullscreen the wrong (usually primary) display instead
  of the requested one. Not a live concern for this feature's current F11 handler, which
  only ever calls `setFullScreen` on a window the user is already interacting with (already
  mapped, shown, and focused) — this trap only bites code that tries to open *already*
  fullscreened, which nothing here does anymore.
- **Native Wayland has no absolute-positioning protocol at all** — a platform constraint,
  not something fixable from Electron's API surface. `BrowserWindow`'s `x`/`y` (and, by
  extension, wherever a pop-out window's default/OS-chosen position ends up) are a hint at
  best under native Wayland; X11/XWayland sessions are unaffected (X11 honors requested
  positions). Not actively worked around here — pop-out windows don't request a specific
  display today (see "Deliberately not built here" below), so there's nothing this
  constraint currently breaks.

## Jitter buffer setting now respected (2026-07-28)

`viewer.js`'s `init()` calls `fetchMaxJitterBufferMs()` (mirrors `view.js`'s function of
the same name exactly — same `get_settings` invoke, same `window.TileVideo.setMaxJitterBufferMs`
setter) before resolving/rendering any tiles. **Bug that existed before this fix**: because
this window loads `tile-video.js` fresh in its own JS context (see the "Architecture" table
above), and `viewer.js`'s `init()` never called `get_settings`, the pop-out window silently
fell back to `tile-video.js`'s hardcoded `DEFAULT_MAX_JITTER_BUFFER_MS` (2000ms) regardless
of the user's configured `max_jitter_buffer_ms` (0-5000ms via the Settings slider) — a
correct value in the main View tab, wrong in the pop-out. Fixed by applying the same
fetch-and-apply pattern `view.js` already used.

No live-update: if the user changes the setting while a viewer window is already open, that
window keeps the value it fetched at open time — matches the rest of the app, which has no
settings-changed live-push event anywhere (`shell.js`'s `listen()` calls only cover
room/presence/stream-stats events); building one is out of scope for this fix.

## Ghost "Stream error" label fixed (2026-07-28)

Popping a stream into fullscreen/borderless (or, it turned out, just watching one plain
on-screen) could leave a "Stream error" label stuck over video that was visibly playing
fine — `tile-video.js`'s `errorPlaceholder` had a `showError()` but no way back to hidden,
so any transient/self-recovering hiccup around this feature's fresh second connection to an
already-flowing route (`stream_video_url`) left a permanent false-positive overlay. Fixed by
adding a `hideError()` that clears it on the next successfully decoded frame. Full root
cause and fix: `superdoc/features/video-pipeline.md`'s "`errorPlaceholder` now clears itself
once playback recovers" section — the bug and fix live entirely in the shared
`window.TileVideo` module, nothing viewer-window-specific was needed.

## Ghost "Stream error" label, follow-up: the real bug was CSS, not `hideError()` (2026-07-28)

The label came back after the fix above — still visible, in both the pop-out window and
plain on-screen tiles, with video visibly playing fine underneath. **This is a different,
new bug from the one above, exposed by testing on the real running app instead of only at
the JS-logic level**: `hideError()`/`showError()` toggle the `hidden` attribute on
`errorPlaceholder` exactly as designed and were never at fault. The attribute simply had no
visual effect, in *either* direction, because `.viewer-tile-placeholder` (`viewer.css`) and
its `view.css` twin `.media-tile-placeholder` both set `display: flex` unconditionally, with
no `[hidden]` override — the same specificity tie as the UA stylesheet's own
`[hidden] { display: none }`, and the author rule loads later and silently wins. This is the
exact class of bug `shell.css`'s `.add-room-card[hidden]` rule already documents and fixes
for a different element; neither `.media-tile-placeholder` nor `.viewer-tile-placeholder` had
been given the same treatment.

Net effect verified live: a tile whose controller had *never once* called `showError()` (no
`console.error` from any of `tile-video.js`'s five error sites, `hidden="hidden"` present and
untouched the whole time) still rendered "Stream error" over live, correctly-decoding
1920×1080 video — proving the bug was 100% presentation-layer, with zero relationship to the
`showError`/`hideError` call sites or their timing.

**Fix**: added `.media-tile-placeholder[hidden] { display: none; }` to `view.css` and
`.viewer-tile-placeholder[hidden] { display: none; }` to `viewer.css`, mirroring
`.add-room-card[hidden]`'s existing pattern exactly.

**Verified live**, two real Electron client instances (a sharer and a viewer) joined to the
same room through a real local relay (`simple-screensharing-server`, real QUIC on
`127.0.0.1:4433`), real screen capture, real WebCodecs decode, both windows kept off
HDMI-A-1 throughout:

- **False positive, before the fix**: watching a live, correctly-decoding stream (canvas
  reaching its real 1920×1080 dimensions, zero `console.error` output from `tile-video.js`)
  still showed "Stream error" in the plain View tab tile — confirmed via
  `document.querySelector('.media-tile-placeholder').outerHTML` showing `hidden="hidden"`
  side by side with the label rendered on screen in a screenshot.
- **False positive, after the fix, plain tile**: same live stream, same tile — label gone,
  `getComputedStyle(...).display === 'none'` while `hidden` stayed set the whole time, video
  visibly playing.
- **False positive, after the fix, pop-out window**: popped the same stream out via the real
  "Pop out" button (`viewer.html`, its own `BrowserWindow`); `.viewer-tile-placeholder`
  `hidden="hidden"` and `getComputedStyle(...).display === 'none'` while its canvas actively
  decoded real frames (0 → 1920 width as frames arrived).
- **False positive, after the fix, real OS fullscreen**: fullscreened that same pop-out
  window (`BrowserWindow.setFullScreen(true)`, the same mechanism `viewer.js`'s F11 handler
  drives) — bounds became exactly `{x:0, y:0, width:1920, height:1080}` on DP-1 (confirmed
  never touching HDMI-A-1), `isFullScreen() === true`, placeholder still correctly hidden and
  computed `display: none` throughout the transition, canvas still decoding.
- **Genuine error still shows correctly**: called the real `startTileVideo` against a
  deliberately unreachable URL — the real fetch-rejection code path fired, `showError()` ran,
  and the placeholder came up (`hidden` attribute removed, computed `display: flex`),
  confirming the fix only corrects the *presentation* of `hidden`, not the underlying
  error-detection logic, which is untouched and still works.

`Why this wasn't caught by the original fix's testing:` that pass verified the *JS* half —
that `hideError()` gets called and clears the attribute — without a real running-app pixel
check against a genuinely live stream, so the CSS override gap never surfaced.

`Why the label reads as intermittent rather than "always on" if the CSS bug makes it
permanently visible:` the placeholder (`position: absolute; inset: 0`, no background) paints
directly on top of the canvas in DOM order for as long as the tile exists — it was never
actually toggling, it was rendered by the DOM continuously. It *reads* as intermittent
because a viewer only notices the label where it has enough contrast against whatever the
canvas happens to be drawing underneath (a mostly-dark captured frame reads as "just a dark
tile", not "an error label sitting over working video") — not because the element's own
visibility was ever actually changing.

## Watch grid gained a per-tile "stop watching" button (2026-07-28)

Not part of this feature's own IPC (no Electron/`__ELECTRON__` involved), but touches the
same tile overlay this doc's "Architecture" table already points at (`view.js`'s
`renderTile`), so noted here rather than inventing a new doc for one button. Each on-screen
tile's `.media-tile-overlay-actions` now has a second button next to "Pop out": a close-icon
"Stop watching" control (`view.js`'s `stopWatching`). It calls the exact same
`toggle_watch(room_handle, owner, stream_id)` Tauri command the "NOT ON SCREEN" rail's "Play"
action (`playNotOnScreen`) already used to *start* a watch — `toggle_watch` is a genuine
toggle server-side (`src-tauri/src/commands.rs`: inserts a `WatchEntry` if absent, removes it
if present), so calling it again for an already-watched key is the "stop" half of the same
command, not a new one. After the call, `init()` refetches; the tile drops out of the grid
and, since the stream is still announced in its room, `fetchWatchedStreams`'s existing
`detail.streams` minus `detail.watched` join naturally re-lists it in the "NOT ON SCREEN"
rail with no extra wiring. Deliberately not added to the held-slot placeholder
(`renderHeldSlot`, for tiles popped out to their own window) — a held tile is still being
watched elsewhere, so "stop watching" there would need to also account for that separate
window, which is out of scope for this pass; stop-watching is only exposed on the normal
(non-held) tile rendering.
Mock branch (no `window.__TAURI__`): removes the tile from `mockWatched` and pushes an
equivalent entry onto `notOnScreenStreams` directly, mirroring `playNotOnScreen`'s reverse
move, so a Playwright/browser-preview run can exercise stop-then-restart without a backend.

## 2026-07-28: consolidation — three window-creation mechanisms became one

Before this pass there were **three** separate "view a stream in its own window"
mechanisms in this codebase, which the project owner asked to be consolidated into one:

1. **"Pop out" button** (per-tile, in the View tab grid) — `view.js`'s old `popOutWatch`
   called the Tauri command `pop_out_stream` (`src-tauri/src/commands.rs`), which opened a
   **native Tauri `WebviewWindowBuilder` window** — a completely different window-creation
   mechanism from #2/#3 below, and a leftover from before the Tauri→Electron shell migration
   (`superdoc/ui/FRONTEND-SHELL.md`'s 2026-07-28 entry) that the rest of the app no longer
   used for window management at all. It pointed at `index.html?popout=1&room=...&owner=
   ...&stream=...`, but no JS anywhere ever read that `popout=1` query param (confirmed by
   grep across `frontend/` before removing it) — so it loaded the **entire app shell**, nav
   tabs and sidebar included, in a second window. Its own doc comment admitted as much:
   "This command only proves the window-opening mechanism — the popout page's real content
   is the frontend worker's job."
2. **"Fullscreen selected" button** (multi-select via tile checkboxes) — opened the
   singleton fullscreen/borderless Electron viewer window this doc used to describe above,
   in fullscreen mode.
3. **"Borderless selected" button** — same Electron mechanism, borderless mode.

The project owner's request: remove Borderless entirely, make "Pop-Out" the one real
window-creation mechanism (replacing what Fullscreen used to do), add F11 to toggle real
fullscreen on it, and fix exiting fullscreen so it no longer closes the window.

**What changed:**

- **Borderless is gone** — the button, `electron/main.js`'s `mode === "borderless"`
  branch (including the whole tiling-WM `resizable: false`-then-`true` mitigation, which
  existed specifically to make a window claim exact display bounds — no longer needed once
  no window tries to do that), and the now-dead `.view-viewer-actions`/
  `.view-viewer-action-btn`/`.media-tile-select-checkbox` CSS.
- **`pop_out_stream` (the Tauri command) is gone** — deleted from `commands.rs`, `main.rs`'s
  `invoke_handler` list, and `api_server.rs`'s dispatch table. **Electron's
  `BrowserWindow`-based mechanism was kept as the one going forward, not the Tauri one** —
  per `superdoc/ui/FRONTEND-SHELL.md`'s migration history, the whole app shell moved from
  Tauri-webview to Electron-shell-plus-Rust-sidecar specifically because Electron's
  multi-`BrowserWindow` support is what a real desktop-app window story needs; a native
  Tauri window surviving in an otherwise fully-Electron-shelled app was exactly the kind of
  migration leftover worth resolving, and it was also the *less* fleshed-out of the two
  (never wired to the frontend's `?popout=1` param, versus the Electron path's already-real
  chrome-free `viewer.html` rendering).
- **The old singleton fullscreen/borderless viewer window is gone as a *singleton*** — its
  window-creation code (`openViewerWindow`), IPC handler names, and single module-level
  `viewerWindow` reference were replaced by `openPopoutWindow`/`popoutWindows` (per-stream,
  see "Architecture" above). The per-stream multi-window behavior `pop_out_stream` already
  had (keyed by `format!("popout-{room_handle}-{owner:016x}-{stream_id}")`, focus-existing
  on a repeat call for the same stream) was **preserved**, not regressed, onto the Electron
  path — confirmed live: two independent streams popped out into two independent windows
  simultaneously, and popping out an already-popped-out stream a second time focused the
  existing window instead of creating a duplicate.
- **F11 added** — `viewer.js`'s keydown handler now toggles `window.__ELECTRON__
  .setFullscreen(!isFullscreen)` on F11, reusing the exact same `viewer-set-fullscreen` IPC
  call the old fullscreen mode used to trigger automatically at window-open time.
- **Fixed: exiting fullscreen no longer closes the window.** Root cause, found by reading
  the pre-consolidation `viewer.js`: its `exitViewer(fromOsFullscreenExit)` was the single
  handler for the × button, Escape, *and* the OS-level `leave-full-screen` forward — and it
  **unconditionally called `closeViewer()` at the end regardless of which of those triggered
  it**, including the OS-level-exit branch, which only skipped the redundant
  `setFullscreen(false)` call and then closed anyway. So any fullscreen exit — F11-equivalent,
  Escape, or a WM/compositor fullscreen-exit gesture — closed the whole pop-out, not just the
  fullscreen state. Fixed by splitting the concerns: `closeViewer()` (the × button only) and
  `setFullscreen(next)` (F11, and Escape-while-fullscreen) are now two separate functions
  that never call each other, and `onViewerLeftFullscreen`'s handler *only* updates the local
  `isFullscreen` mirror — it never calls close. Escape while **not** fullscreen is now a
  no-op (a normal windowed pop-out already has its own close affordances — the × button, or
  the OS window frame's close control — so Escape closing it too would be a surprising extra
  way to lose the window).

**Verified live** (Electron `_electron` driver + real X11/XWayland windows, on a
multi-monitor machine where one output had to stay untouched for safety — every window
checked via `BrowserWindow.getBounds()`/`getAllWindows()` before and after each step to
confirm it never landed there):
- Clicking Pop-out on a tile opens `viewer.html` (confirmed by URL and by screenshot — no
  nav tabs/sidebar, just the video area and the × button), not `index.html`.
- Two different streams popped out into two distinct, simultaneously-open `BrowserWindow`s
  (distinct Electron window `id`s); toggling fullscreen on one left the other completely
  untouched.
- Popping out an already-popped-out stream a second time left the window count unchanged
  (focused the existing window, no duplicate).
- F11 on a pop-out window produced real OS fullscreen: `isFullScreen() === true`, bounds
  exactly matched the target display's full `1920x1080` at its origin.
- Exiting fullscreen — both via the renderer's own Escape handler and via a direct
  main-process `setFullScreen(false)` call made to simulate an OS-level exit through the
  same `leave-full-screen` code path a real WM gesture uses — left the window **open** both
  times, `isFullScreen()` back to `false`, bounds back to the pre-fullscreen windowed size.
- The × button closed only the window it was clicked in; the other pop-out window stayed
  open.
- `document.querySelector('.view-viewer-actions')` and `.media-tile-select-checkbox` both
  resolved to `null` in the real running app — the Fullscreen/Borderless controls and the
  per-tile selection checkbox are gone from the DOM, not just visually hidden.

Not independently re-driven through a real relay + real watched stream in this pass (that
infrastructure is heavier than the window-mechanism this consolidation is about, and is
already covered by `superdoc/features/video-pipeline.md`'s own real-stream verification) —
the pop-out button's one-line `onClick` handler forwards directly to the exact
`window.__ELECTRON__.openPopout` call exercised above, so this is a thin, already-read
wiring layer over a mechanism that was verified end-to-end.

## Deliberately not built here

Per-tile aspect-ratio-aware grid layout in **this** file's pop-out window
(`frontend/scripts/viewer.js` still uses the plain `ceil(sqrt(n))` columns /
`ceil(n/cols)` rows split described below, unchanged by the main-window grid work in the
next section — a pop-out window always renders exactly one tile today, so the shape of
its grid math has no visible effect yet and wasn't worth touching in this pass); a
display-picker for where a pop-out window opens (it opens at whatever position/display
the OS/WM defaults a new window to — the old `list-displays` IPC and `displayId`
targeting that existed for the fullscreen/borderless feature were removed as dead code in
the 2026-07-28 consolidation, since nothing needs to force a pop-out window onto a
specific display); resizing/repositioning a pop-out window after it's open without
closing and reopening it.

## Main-window Watch grid: `ceil(sqrt(n))` replaced by an area-maximizing rows x cols search (2026-07-28)

The **main window's** Watch grid (`frontend/scripts/view.js`'s `render()`/
`computeGridShape`/`applyGridLayout` — not this file's pop-out window, see the note just
above) no longer picks a fixed "square-ish" `ceil(sqrt(n))` column count. That heuristic
only looked at the tile count, never the window's actual shape: in a tall/narrow window,
2 side-by-side 16:9 tiles (`ceil(sqrt(2))` = 2 columns) each shrink to fit the width,
wasting most of the window's height as letterboxing above/below — stacking them (1
column, 2 rows) would use the same width per tile but grow each tile far taller, using
the available area much better.

**`Why:`** the project owner's request was specifically "stack them, if that maximises
the used viewing area... it should place them dynamically" — a fixed `sqrt(n)` shape
can't respond to window aspect ratio at all, by construction.

**The algorithm** (`computeGridShape(n, availWidth, availHeight, gap, aspect=16/9)`,
pure/DOM-free): for every `cols` from 1 to `n` (`rows = ceil(n / cols)`, the minimum rows
that fit `n` tiles at that column count), compute the size a single 16:9 tile would
render at if `availWidth x availHeight` were evenly divided into that `rows x cols` grid
(minus `gap` between cells), clamped to 16:9 (width-limited or height-limited, whichever
is smaller). Pick whichever `rows x cols` shape maximizes that per-tile area — since every
candidate is a uniform grid, maximizing per-tile area is equivalent to maximizing total
displayed video area across all `n` tiles. This is the rows x cols grid search the task
brief suggested, not a general bin-packing solver — deliberately: with tiles constrained
to a uniform grid and a fixed aspect ratio, a full 2D search over every `cols` in `[1, n]`
is cheap (`n` is realistically single digits) and already finds the true optimum for that
search space.

`n` includes held-slot placeholders (`poppedOutIds`), matching the pre-existing "grid
never reshuffles just because a tile popped out" behavior.

**Where the numbers come from:** `applyGridLayout()` measures `.content` (the app shell's
shared scroll container, stable across which tab is active — see `shell.css`) minus the
real rendered heights of `.view-header`/`.view-controls` above the grid and
`.view-not-on-screen` below it (if rendered), rather than assuming a fixed layout. It
writes the chosen shape as `--grid-cols`/`--grid-rows` and the resulting per-tile pixel
size as `--tile-w`/`--tile-h` (already 16:9-correct) onto the grid element; `view.css`'s
`.view-grid` consumes all four as explicit `grid-template-columns`/`grid-template-rows`
track sizes (rather than `1fr`), with `justify-content`/`align-content: center` to
letterbox/pillarbox the grid *as a whole* when the chosen shape doesn't exactly fill the
container. Because the track sizes are already exactly aspect-correct, `.media-tile`'s
own `aspect-ratio: 16/9` is automatically satisfied with no changes to the tile's own
markup/CSS.

Recomputed on tile-count change (inline in `render()`, after the grid's DOM is built) and
on a debounced (120ms) `ResizeObserver` on `#view-root` — chosen over observing `.content`
directly because `#view-root` also goes from a 0x0 box to its real size when
`shell.js`'s `setActiveTab` un-hides this tab's panel (a plain attribute flip that shell.js
does with no callback into this file), so the same observer transparently recovers from a
`render()` that happened while this tab was hidden (all rects read 0 then, since a
`display: none` ancestor collapses descendant layout) without any new coupling between
`shell.js` and `view.js`.

**Verified** with a standalone `node -e` port of `computeGridShape`'s math (this codebase
has no JS test framework — see this doc's other 2026-07-28 entries for the same pattern)
against `(n, width, height)` triples for a wide 16:9-ish window, a narrow portrait window,
and a roughly-square window, for `n` in `{2, 3, 4, 6}`: wide favored more columns (e.g.
`n=4` → 2x2, `n=6` → 3x2), narrow favored a single column at every `n` (the exact
"stacking" behavior the request called for), and the square case leaned toward fewer
columns than the old `ceil(sqrt(n))` heuristic would have picked (e.g. `n=3` → 1x3, not
2x2) whenever the available height comfortably exceeds the width. Cross-checked live
against the real (mock-data) frontend served over plain HTTP and driven with Playwright at
500x1000 (narrow — 3 mock tiles stacked 1x3, using the full available width instead of
each shrinking to a third of it), 1400x700 (wide — 3 tiles as 2x2, large tiles), and
900x850 (square-ish content area — 3 tiles as 1x3, matching the pure-math prediction), all
without touching the real HDMI-connected display.

## Watch grid area-maximizing layout, follow-up: tiles overflowing the viewport on certain window shapes (2026-07-28)

Follow-up to the entry just above, found the same day: the project owner reported the
Watch grid (2 mock tiles, 1 column x 2 rows — exactly the "stack them" shape the feature
above was built for) rendering with its second tile's bottom edge cut off past the
visible viewport, on "certain aspect ratios of the window."

**Root cause:** `applyGridLayout()`'s `availHeight` measurement (`frontend/scripts/
view.js`) derived the space above the grid from a hand-enumerated list of siblings —
`.view-header`'s and `.view-controls`' rendered heights, each plus a hardcoded margin
literal — and never accounted for the `<h2>View</h2>` tab title that `index.html` renders
*above* `#view-root` (and therefore above `.view-header` too). That `<h2>` measures
~35px (19px line box + `.tab-panel h2`'s 16px `margin-bottom`, `shell.css`). Every
`applyGridLayout()` call computed `availHeight` about 35px too tall as a result.

This was harmless whenever *width* was the binding constraint (a wide/short window has
height to spare, so 35px of extra headroom just went unused). It only produced visible
overflow on shapes where height was the binding constraint and the computed layout came
close to filling the container — reproduced live with a roughly-square 800x800 content
area and 3 mock tiles: the grid itself fit, but the extra 35px pushed the "not on screen"
rail (which renders directly below the grid) past `.content`'s bottom edge, forcing
`.content` to scroll (`scrollHeight` 835 vs `clientHeight` 800 — the 35px gap matches the
missing `<h2>` height almost exactly). The `computeGridShape` math itself (the rows x cols
search and its gap accounting, `(rows-1)*gap`/`(cols-1)*gap` — correctly N-1 gaps for N
rows/cols, no fencepost bug) was not at fault, and neither was measurement timing —
`applyGridLayout()` runs synchronously right after the grid's DOM is built, and reading
`getBoundingClientRect()`/`clientHeight` forces a synchronous layout flush, so there was
no stale-measurement window to chase.

**The fix:** stopped deriving the "space above the grid" from an enumerated sibling list
at all. `applyGridLayout()` now reads `currentGridEl.getBoundingClientRect().top`
directly — the grid's own rendered top position already reflects the combined layout of
every element before it in the DOM (the `<h2>`, `.view-header`, `.view-controls`, and
anything added above the grid in the future), with no separate literal to keep in sync.
The "not on screen" rail (which sits *after* the grid) still needs its own hardcoded
`margin-top` literal (`16px`, matching `.view-not-on-screen` in `view.css`) since it can't
be derived the same way without a second layout pass.

**Verified** the same way as the feature's own entry above (Playwright against the real
mock-data frontend, `.content`'s and the grid's real `getBoundingClientRect()`/
`clientHeight`/`scrollHeight`), across 2/3/4 mock tiles and four window shapes — a very
tall/narrow 420x900, a moderate tall 560x820 (closest match to the owner's screenshot,
n=2 lands on cols=1/rows=2 as expected), a wide/short 1200x500, and a square 800x800. All
twelve combinations landed with the grid's (and rail's) rendered bottom edge above
`.content`'s visible bottom edge (`.content`'s `scrollHeight` no longer exceeds its
`clientHeight` in the previously-failing square case) — before the fix, the square/n=3
case alone showed a 35px `scrollHeight`/`clientHeight` gap; after, that gap is 0 across
every combination tested.

**Defense-in-depth (considered, not added as new CSS):** `.content` (`shell.css`) already
wraps the grid with `overflow: auto`, so even a future measurement bug of this kind
degrades to a scrollbar rather than tiles silently clipped or spilling with no way to
reach them — see the comment left on `.view-grid` in `view.css`. No new `overflow` rule
was needed on `.view-grid` itself; the actual fix belonged in the `view.js` measurement,
not in papering over it with more CSS.
