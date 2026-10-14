---
name: run
description: Launch and drive the real simple-screensharing-client app (Electron shell + built src-tauri sidecar) for verification. Use whenever asked to run, start, or screenshot this app, or to confirm a change works for real — not just against mock data in a browser.
---

# Run: simple-screensharing-client

This is the concrete, tested procedure for launching the **real** app — the Electron shell
talking to the real, compiled `src-tauri` sidecar over REST+SSE. It replaces ad hoc
Playwright/Electron harnesses: several agents independently built their own inconsistent
throwaway version of this before it existed. Use this skill instead of re-deriving it.

See `superdoc/claude-instructions/testing-and-verification.md` (force-loaded, one directory
up from this repo) for *when* real-app verification is required versus when plain
browser+mock is legitimate. This skill only covers *how* to launch it.

## Hard safety rule — check every time, no exceptions

DP-1, DP-2, and DP-3 are free for testing. **HDMI has the project owner's real, active,
maximized session on it.** Never let any test window or screen-capture region touch the
HDMI output.

Before launching anything, run:

```bash
xrandr --listmonitors
```

and confirm which output is HDMI and its geometry. Keep every test window and any capture
region confined to DP-1/DP-2/DP-3. If you're not sure a window landed off HDMI, check again
before proceeding — don't assume a remembered layout still holds.

**Checking monitor geometry beforehand is not enough by itself** — an incident this session
had a manually-spawned window (an ad hoc `electron .` + `xwininfo` launch, precisely the
pattern this skill exists to replace) briefly land at an X-coordinate inside the HDMI range,
caught and killed within seconds but only because it was noticed. Don't rely on noticing:
after a window is created, verify **its actual reported position** — e.g. with the Playwright
`_electron` pattern below, explicitly call `BrowserWindow.setPosition(x, y)` (or equivalent)
to a known-safe DP-1/2/3 coordinate immediately, then confirm the bounds it reports back are
what you set — *before* doing anything else with that window (screenshots, interaction,
capture). Treat "I launched it inside the safe range" and "I confirmed where it actually
landed" as two separate steps, not one.

## 1. Build the sidecar

Electron spawns the compiled `src-tauri` binary as its backend sidecar. Build it first,
every time you've changed Rust code (or if you're not sure it's up to date):

```bash
cargo build -p simple-screensharing-client-tauri
```

## 2. First-time setup: install Electron's node_modules

`electron/node_modules` is not always present in a given worktree — several agents hit this.
Check first, install if missing:

```bash
cd electron
[ -d node_modules ] || npm install
```

## 3. Launch the real app

### Quick manual "just look at it" launch

```bash
cd electron
electron .
```

This opens the real Electron window against the real sidecar. Use this for a fast visual
check. **This path does not control where the window lands** — the OS/window manager picks
its position, which is not guaranteed to be on DP-1/2/3. Prefer the scripted path below,
which explicitly places and verifies the window's position, whenever you're not physically
watching the screen as it opens.

### Scripted verification with Playwright's `_electron` driver

**Use Playwright's Node `_electron` API, not the Playwright MCP browser tools.** The MCP
browser tools attach to a standalone Chrome instance — they cannot drive the real Electron
renderer at all, and using them against this app silently verifies nothing. This is a real
trap that has been hit before. The correct driver is `require("playwright")._electron`,
launched directly against `electron/main.js`, as a plain Node script run with `node`.

Minimal working pattern (adapt selectors/assertions to what you're verifying):

```js
// verify.js — run with: node verify.js
const { _electron: electron } = require("playwright");
const path = require("path");

const ELECTRON_DIR = __dirname; // run this script from inside electron/

(async () => {
  const electronApp = await electron.launch({
    args: [path.join(ELECTRON_DIR, "main.js")],
    cwd: ELECTRON_DIR,
  });

  const page = await electronApp.firstWindow();
  page.on("console", (msg) => console.log(`[page-console:${msg.type()}] ${msg.text()}`));
  page.on("pageerror", (err) => console.log(`[pageerror] ${err.message}`));

  // Safety: force the window onto a known-safe monitor and CONFIRM it landed there —
  // don't just launch and assume. Re-run `xrandr --listmonitors` first if you haven't
  // already this session, and use real DP-1/2/3 coordinates, not these placeholders.
  const bounds = await electronApp.evaluate(({ BrowserWindow }, [x, y]) => {
    const win = BrowserWindow.getAllWindows()[0];
    win.setPosition(x, y);
    return win.getBounds();
  }, [50, 50]);
  console.log("[safety] window bounds after placement:", bounds);
  // bounds.x/bounds.y must be inside DP-1/2/3's range — verify against this run's
  // `xrandr --listmonitors` output before proceeding. Abort if it isn't.

  await page.waitForSelector("#nav", { timeout: 20000 });

  // ...drive/assert against the real app here (click tabs, check DOM, etc.)...

  await page.screenshot({ path: path.join(ELECTRON_DIR, "verify.png") });

  await electronApp.close();
})().catch((err) => {
  console.error("[harness] FAILED:", err);
  process.exit(1);
});
```

Run it from `electron/`:

```bash
cd electron
node verify.js
```

`playwright` is already a devDependency of `electron/package.json`, so no extra install is
needed beyond step 2 above (`npm install` in `electron/`).

Treat this script as throwaway per verification pass — write it fresh (or adapt the pattern
above) for the specific tabs/state/DOM you need to check, then discard it. Don't check
one-off verification scripts into the repo.
