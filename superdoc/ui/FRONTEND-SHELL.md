# Frontend shell (HISTORICAL: the Electron/HTML frontend)

**This page is history.** The client's UI is now a single native egui app; see
`APP-SHELL.md` (layout, modules, rules) and `STYLING-GUIDE.md` (how it is built). Rewritten
2026-10-14 from a ~1480-line description of `client/frontend/` (plain HTML/CSS/JS, no
framework or bundler) shown in an Electron `BrowserWindow` over a Rust sidecar
(`client/src-tauri/`, REST + SSE; originally a Tauri webview). That frontend was deleted in the
egui rework (task I23).

The full old text (tab-by-tab behaviour, DTO shapes, event wiring, dated QC notes) is
preserved in git: `git show 3f2d5d3:superdoc/ui/FRONTEND-SHELL.md`.

## What lives on from it

The Electron app is the **visual and behavioural reference** for the egui UI: the colour
tokens (`client/frontend/styles/tokens.css` -> `client/src/theme.rs`, each constant names
its CSS variable), sizes, copy, the viewer window's rules (hover-gated chrome, Escape
leaves fullscreen but never closes), custom segmented/checkbox looks, the Material Symbols
icon set, and the Geist fonts. Where the egui styling guide says "Electron wins on looks",
this is the Electron it means.

## Design reasons worth keeping

These were recorded in the old page and still explain choices you may meet in the new code:

- **Geist Mono everywhere was a mistake for text.** A monospace face is much wider per
  character; the same row text that fits in a proportional font squeezed room names down to
  one visible letter, forcing a `not connected` sub-line and a 288 px sidebar. The egui UI
  therefore uses Geist (sans) for text and GeistMono only for numbers
  (`STYLING-GUIDE.md`, deviations).
- **A glyph font beats Unicode carets.** The first egui UI could only use symbols egui's
  bundled fonts happen to cover (several "obvious" triangles rendered as tofu); the
  Electron app and now the egui app embed Material Symbols and address icons by name.
- **Native `<select>` could not be themed**, so the Electron app replaced it with a custom
  dropdown (button trigger + body-appended list, because dropdowns sat inside
  `overflow: hidden` cards). The egui `select` helper plays that role; the sizing lesson
  carries over: dropdowns must match button height (32), and a leftover per-class padding
  from the native-select days had left them at 23 px. One helper per control type is how
  the egui widgets avoid that.
- **`[hidden]` vs `display`:** a class that sets its own `display` silently beats the UA
  `[hidden]` rule. (CSS-only trap; gone with the frontend.)
- **Mock/real fallback pattern:** the frontend could run standalone in a browser against
  mock data shaped exactly like the real DTOs. That made browser+mock
  verification cheap and tempting, and agents kept doing it instead of verifying against
  the real shell. The no-mock rule in `superdoc/claude-instructions/testing-and-verification.md` exists
  because of it, and the egui-era analogue is `--gallery`.
- **Process-model lessons** (hung commands wedging a single-threaded bridge, camelCase IPC
  argument keys, a single-slot video mailbox) belonged to the sidecar architecture; they
  are in `superdoc/architecture/overview.md` or git history, not needed by the egui client,
  which calls the `simple_screensharing_client` lib in-process.
