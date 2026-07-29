// Electron main process: replaces the Tauri webview shell.
//
// Responsibilities:
//   1. Spawn the built `src-tauri` binary as a backend sidecar (it runs the
//      REST+SSE api_server described in `src-tauri/src/api_server.rs`, plus
//      all the existing capture/network logic; its own Tauri window is
//      hidden via `"visible": false` in `src-tauri/tauri.conf.json`, so it
//      never shows its own UI — Electron is the only visible window).
//   2. Capture the `API_TOKEN=<hex>` and `API_PORT=<port>` lines the sidecar
//      prints on stdout once, before it starts accepting connections. The
//      port is ephemeral (OS-assigned) so multiple sidecar instances can run
//      side by side without a bind conflict; the sidecar binary itself can
//      be overridden via the `SIDECAR_BIN` env var (full path), e.g. to
//      point at a `--release` build instead of the debug default.
//   3. Poll the server's TCP port until it actually accepts connections
//      (the token being printed doesn't guarantee the listener is bound).
//   4. Create the BrowserWindow (secure defaults, preload script) pointing
//      at `frontend/index.html`.
//   5. Bridge `ipcMain.handle('tauri-invoke', ...)` to `POST /command/<name>`
//      and forward the `/events` SSE stream (consumed here in the main
//      process, since a renderer-side `EventSource` cannot set the
//      `Authorization` header the server requires) to the renderer via
//      `webContents.send(eventName, payload)`.

"use strict";

const { app, BrowserWindow, ipcMain, Menu } = require("electron");
const { spawn } = require("child_process");
const path = require("path");
const http = require("http");
const net = require("net");
const readline = require("readline");

const API_HOST = "127.0.0.1";

// SIDECAR_BIN: optional full path to the sidecar binary to spawn, overriding
// the default debug-build path below. Lets a caller (e.g. a dev script that
// wants representative capture/encode/decode performance) point Electron at
// a `--release` build without any code change.
const SIDECAR_PATH = process.env.SIDECAR_BIN || path.join(__dirname, "..", "target", "debug", "simple-screensharing-client-tauri");
const FRONTEND_INDEX = path.join(__dirname, "..", "frontend", "index.html");
const VIEWER_HTML = path.join(__dirname, "..", "frontend", "viewer.html");
const PRELOAD = path.join(__dirname, "preload.js");
// 256px source: large enough for the window manager/taskbar to downscale
// cleanly on Linux without passing multiple sizes (Electron scales a single
// PNG itself). See `superdoc/ui/APP-SHELL.md`'s icon note for the full kept-
// resolution rationale.
const APP_ICON = path.join(__dirname, "..", "frontend", "assets", "icons", "mark-256.png");

let apiToken = null;
let apiPort = null;
let mainWindow = null;
let sidecarProcess = null;
let sseAbort = false;

// Pop-out windows: one independent `BrowserWindow` per popped-out stream,
// keyed by streamKey (`${room_handle}:${owner}:${stream_id}`, the same
// tile-id scheme `view.js` uses) — NOT a singleton. See `openPopoutWindow`
// below and `superdoc/features/viewer-window.md`'s 2026-07-28 entry for why
// this replaced the old singleton fullscreen/borderless viewer window.
const popoutWindows = new Map();

// ---------------------------------------------------------------------
// Sidecar spawn + token capture
// ---------------------------------------------------------------------

function spawnSidecar() {
  const fs = require("fs");
  if (!fs.existsSync(SIDECAR_PATH)) {
    console.error(
      `[electron] Sidecar binary not found at ${SIDECAR_PATH}.\n` +
        `[electron] Build it first: cd src-tauri && cargo build`
    );
    app.quit();
    return Promise.reject(new Error("sidecar binary missing"));
  }

  return new Promise((resolve, reject) => {
    sidecarProcess = spawn(SIDECAR_PATH, [], {
      stdio: ["ignore", "pipe", "pipe"],
    });

    sidecarProcess.on("error", (err) => {
      console.error("[sidecar] failed to spawn:", err);
      reject(err);
    });

    sidecarProcess.on("exit", (code, signal) => {
      console.error(`[sidecar] exited (code=${code}, signal=${signal})`);
      if (!apiToken || !apiPort) {
        reject(new Error("sidecar exited before printing API_TOKEN/API_PORT"));
      }
    });

    // Forward stderr straight through for debuggability.
    const errRl = readline.createInterface({ input: sidecarProcess.stderr });
    errRl.on("line", (line) => console.error(`[sidecar] ${line}`));

    // Line-buffer stdout: watch for API_TOKEN=... and API_PORT=... (order
    // between them doesn't matter), then keep forwarding every subsequent
    // line to our own console too. Resolve once both have been captured.
    const outRl = readline.createInterface({ input: sidecarProcess.stdout });
    outRl.on("line", (line) => {
      console.log(`[sidecar] ${line}`);
      if (!apiToken) {
        const m = line.match(/^API_TOKEN=([0-9a-f]{64})$/);
        if (m) apiToken = m[1];
      }
      if (!apiPort) {
        const m = line.match(/^API_PORT=(\d+)$/);
        if (m) apiPort = Number(m[1]);
      }
      if (apiToken && apiPort) {
        resolve();
      }
    });
  });
}

// ---------------------------------------------------------------------
// Wait for the API server's TCP listener to actually accept connections
// ---------------------------------------------------------------------

function waitForServer(timeoutMs = 5000, intervalMs = 200) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve, reject) => {
    function attempt() {
      const socket = net.connect({ host: API_HOST, port: apiPort }, () => {
        socket.end();
        resolve();
      });
      socket.on("error", () => {
        socket.destroy();
        if (Date.now() >= deadline) {
          reject(new Error(`api_server did not start accepting connections within ${timeoutMs}ms`));
        } else {
          setTimeout(attempt, intervalMs);
        }
      });
    }
    attempt();
  });
}

// ---------------------------------------------------------------------
// Command bridge: POST /command/<name>
// ---------------------------------------------------------------------

// Real Tauri's `invoke()` accepts JS-conventional camelCase argument keys
// (e.g. `{ roomHandle: 3 }`) and auto-converts them to the Rust command's
// snake_case parameter names before dispatch — every call site across
// `frontend/scripts/*.js` (shell.js, view.js, stream-control.js, settings.js)
// was written against that convention. `api_server.rs`'s `dispatch()` has no
// such leniency: it looks up each argument by the Rust parameter name
// exactly (see its module doc), so a raw passthrough of camelCase keys here
// mismatches on every command that takes arguments. This replicates Tauri's
// side of that contract so the frontend needs no changes — e.g.
// `POST /command/room_detail` was arriving with a *missing* `room_handle`
// key (sent as `roomHandle`), which reached `src-tauri`'s `get_u32` and
// failed with "missing or invalid u32 field `room_handle`" (surfaced by
// `stream-control.js`'s `init()` calling `room_detail` unconditionally on
// first load, before its first `render()`, so the whole tab body stayed
// empty — other tabs happened not to hit this: `view.js` only calls
// `room_detail` inside a loop over already-joined rooms, empty on first
// load, and `settings.js` never calls it at all).
function camelToSnake(key) {
  return key.replace(/[A-Z]/g, (c) => "_" + c.toLowerCase());
}

function convertArgKeys(args) {
  if (!args || typeof args !== "object") return args;
  const out = {};
  for (const [k, v] of Object.entries(args)) out[camelToSnake(k)] = v;
  return out;
}

function postCommand(name, args) {
  const body = Buffer.from(JSON.stringify(convertArgKeys(args) || {}), "utf8");
  return new Promise((resolve, reject) => {
    const req = http.request(
      {
        host: API_HOST,
        port: apiPort,
        path: `/command/${encodeURIComponent(name)}`,
        method: "POST",
        headers: {
          Authorization: `Bearer ${apiToken}`,
          "Content-Type": "application/json",
          "Content-Length": body.length,
        },
      },
      (res) => {
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => {
          const text = Buffer.concat(chunks).toString("utf8");
          let parsed;
          try {
            parsed = text.length ? JSON.parse(text) : null;
          } catch (e) {
            reject(new Error(`invalid JSON response for command '${name}': ${e.message}`));
            return;
          }
          if (res.statusCode === 200) {
            resolve(parsed);
          } else {
            const message = parsed && parsed.error ? parsed.error : `command '${name}' failed with status ${res.statusCode}`;
            reject(new Error(message));
          }
        });
      }
    );
    req.on("error", reject);
    req.write(body);
    req.end();
  });
}

ipcMain.handle("tauri-invoke", async (_event, cmd, args) => {
  return postCommand(cmd, args || {});
});

// ---------------------------------------------------------------------
// SSE bridge: GET /events -> webContents.send(eventName, payload)
// ---------------------------------------------------------------------

function connectSse() {
  if (sseAbort) return;

  const req = http.get(
    {
      host: API_HOST,
      port: apiPort,
      path: "/events",
      headers: { Authorization: `Bearer ${apiToken}` },
    },
    (res) => {
      let buffer = "";
      let pendingEvent = null;
      let pendingData = null;

      res.setEncoding("utf8");
      res.on("data", (chunk) => {
        buffer += chunk;
        let idx;
        while ((idx = buffer.indexOf("\n")) !== -1) {
          const line = buffer.slice(0, idx).replace(/\r$/, "");
          buffer = buffer.slice(idx + 1);

          if (line.startsWith(":")) {
            // keep-alive comment, ignore
            continue;
          }
          if (line === "") {
            // blank line terminates a frame
            if (pendingEvent !== null && pendingData !== null) {
              dispatchEvent(pendingEvent, pendingData);
            }
            pendingEvent = null;
            pendingData = null;
            continue;
          }
          if (line.startsWith("event:")) {
            pendingEvent = line.slice("event:".length).trim();
          } else if (line.startsWith("data:")) {
            pendingData = line.slice("data:".length).trim();
          }
        }
      });

      res.on("end", () => {
        console.error("[sse] connection ended, retrying");
        retrySse();
      });
      res.on("error", (err) => {
        console.error("[sse] stream error:", err);
        retrySse();
      });
    }
  );

  req.on("error", (err) => {
    console.error("[sse] request error:", err);
    retrySse();
  });
}

let sseRetryDelay = 500;
const SSE_RETRY_MAX = 10000;

function retrySse() {
  if (sseAbort) return;
  setTimeout(() => {
    connectSse();
  }, sseRetryDelay);
  sseRetryDelay = Math.min(sseRetryDelay * 2, SSE_RETRY_MAX);
}

function dispatchEvent(eventName, dataText) {
  sseRetryDelay = 500; // reset backoff on a successful frame
  let payload;
  try {
    payload = JSON.parse(dataText);
  } catch (e) {
    console.error(`[sse] failed to parse data for event '${eventName}':`, e);
    return;
  }
  if (mainWindow && !mainWindow.isDestroyed()) {
    mainWindow.webContents.send(eventName, payload);
  }
  // Pop-out windows have no `shell.js` to fan events out for them — they
  // listen for the one they care about (`stream-stats-updated`, see
  // `viewer.js`'s `listenForStats`) directly via `window.__TAURI__.event
  // .listen`, which is backed by a plain `ipcRenderer.on(eventName, ...)` in
  // `preload.js`. Without this, that `.on()` registration never received
  // anything: SSE events used to be forwarded to `mainWindow` only, so a
  // pop-out window's bitrate readout stayed permanently blank (root cause
  // of the pop-out "bitrate never shows" bug — confirmed live, fixed here).
  // Forwarding every event to every pop-out window (not just the one whose
  // stream this event happens to concern) is harmless: a pop-out window
  // only has an `ipcRenderer.on` listener for events it actually asked for
  // (just `stream-stats-updated`), so sending it e.g. `room-connected` is
  // simply never read.
  for (const win of popoutWindows.values()) {
    if (win && !win.isDestroyed()) win.webContents.send(eventName, payload);
  }
}

// ---------------------------------------------------------------------
// Window creation
// ---------------------------------------------------------------------

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 800,
    height: 600,
    title: "Simple Screensharing",
    icon: APP_ICON,
    webPreferences: {
      preload: PRELOAD,
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  mainWindow.loadFile(FRONTEND_INDEX);
}

// ---------------------------------------------------------------------
// Pop-out windows: the one "view a stream in its own OS window" mechanism.
//
// 2026-07-28 consolidation (see `superdoc/features/viewer-window.md`'s
// matching dated entry): this replaced two other, overlapping mechanisms —
// (1) the old singleton fullscreen/borderless viewer window that used to
// live here (opening a new one always closed/replaced whatever was open),
// and (2) `src-tauri/src/commands.rs`'s `pop_out_stream` Tauri command,
// which opened a *native Tauri* `WebviewWindowBuilder` window pointed at
// the full `index.html` app shell (nav tabs, sidebar, everything) — a
// leftover from before the Tauri->Electron shell migration
// (`superdoc/ui/FRONTEND-SHELL.md`'s 2026-07-28 entry) that the rest of the
// app no longer uses for window management at all. Electron's
// multi-`BrowserWindow` support was already the mechanism used for real
// fullscreen; it's also the better home for "one lean window per stream",
// so that's what this became, and `pop_out_stream` was deleted rather than
// kept alongside it.
//
// One `BrowserWindow` PER STREAM (`popoutWindows`, keyed by streamKey) —
// deliberately NOT a singleton, unlike the old fullscreen/borderless
// window: popping out a second stream must not close the first (this
// matches `pop_out_stream`'s pre-existing per-stream-label behavior, which
// this consolidation preserves rather than regresses). Each window opens
// as a normal, lean, chrome-free (no app-shell chrome — still has a normal
// OS window frame/titlebar) windowed pop-out; F11 inside it toggles real
// OS fullscreen without closing the window, including on exit (see
// `viewer-set-fullscreen`/`leave-full-screen` below and `viewer.js`'s
// keydown handler).
// ---------------------------------------------------------------------

// Returns the `win.loadFile` promise so a `loadFile` failure (e.g. a bad/
// missing `VIEWER_HTML` path) rejects this function's own promise instead
// of failing silently — the `open-popout-window` IPC handler below awaits
// this and lets the rejection propagate to the renderer's `openPopout()`
// caller as a real error instead of a silent hang.
async function openPopoutWindow(streamKey, meta) {
  meta = meta || {};
  const existing = popoutWindows.get(streamKey);
  if (existing && !existing.isDestroyed()) {
    // Same stream already popped out — focus it instead of opening a
    // duplicate window, mirroring the old Tauri `pop_out_stream` command's
    // per-label reuse.
    existing.focus();
    return;
  }

  const win = new BrowserWindow({
    width: 640,
    height: 400,
    title: "Simple Screensharing — Stream",
    // Each pop-out is its own top-level OS window and gets its own taskbar
    // entry (Linux/Windows), so it carries the same app icon as the main
    // window rather than falling back to Electron's default icon.
    icon: APP_ICON,
    webPreferences: {
      preload: PRELOAD,
      contextIsolation: true,
      nodeIntegration: false,
    },
  });
  popoutWindows.set(streamKey, win);

  win.on("leave-full-screen", () => {
    // Forwards an OS-level fullscreen exit (e.g. an Escape/WM gesture, not
    // triggered through our own setFullscreen(false) IPC call) to this
    // window's own renderer so its UI state can stay in sync. Deliberately
    // does NOT close the window — see viewer.js's `onViewerLeftFullscreen`
    // handler; conflating "no longer fullscreen" with "close the window"
    // was the pre-consolidation bug this feature explicitly fixes.
    if (!win.isDestroyed()) {
      win.webContents.send("viewer-left-fullscreen");
    }
  });

  win.on("closed", () => {
    // Only clear the map entry if it still points at this window — a stale
    // `closed` event from a window that was already replaced (same
    // streamKey popped out again after the first was closing) must not
    // delete a newer window's entry.
    if (popoutWindows.get(streamKey) === win) popoutWindows.delete(streamKey);

    // "The pop-out window closed" signal (2026-07-28): tells the main
    // window's renderer which streamKey's window just closed, so `view.js`
    // can drop it from `poppedOutIds` and let that tile resume normal
    // rendering automatically — see that file's header note this replaces
    // ("no real 'the window closed' signal ... no way to clear this
    // automatically"). Fired unconditionally on every close (manual × close,
    // OS close button, or window destroyed some other way), not just the
    // map-cleanup branch above, since the renderer-side state needs to
    // clear regardless of whether this was the "current" window for that
    // streamKey.
    if (mainWindow && !mainWindow.isDestroyed()) {
      mainWindow.webContents.send("popout-closed", streamKey);
    }
  });

  // streamKeys is handed to viewer.js via a query string on the loaded file
  // URL (?streamKeys=<json-encoded-array-of-one>), read on load via `new
  // URLSearchParams(location.search)`. Chosen over an IPC message sent
  // after `did-finish-load` because it's available synchronously to the
  // viewer's own startup code with no race against when the renderer
  // attaches its listener. Wrapped in a one-element array because
  // viewer.js's tile-resolving/grid code is still generic over N tiles
  // (unchanged, still shared/tested code) even though every pop-out window
  // now always shows exactly one.
  //
  // `meta` (2026-07-28, functional parity pass): the caller's own
  // `ownerLabel`/`roomLabel`/`hasAudio`/`resolution` for this tile, passed
  // straight through as extra query params rather than having viewer.js
  // re-derive them itself (which would mean re-fetching `room_detail` and
  // re-joining against `members`/`streams` — logic `view.js`'s
  // `fetchWatchedStreams` already did once to build the tile `popOutWatch`
  // was called with). `hasAudio` is stringified as "1"/omitted rather than
  // "true"/"false" so `viewer.js`'s `params.get("hasAudio") === "1"` check
  // stays a plain equality, matching how absent-vs-present is used
  // elsewhere in this file's query-string handling.
  const params = new URLSearchParams();
  params.set("streamKeys", JSON.stringify([streamKey]));
  if (meta.ownerLabel) params.set("ownerLabel", meta.ownerLabel);
  if (meta.roomLabel) params.set("roomLabel", meta.roomLabel);
  if (meta.hasAudio) params.set("hasAudio", "1");
  if (meta.resolution) params.set("resolution", meta.resolution);
  return win.loadFile(VIEWER_HTML, { search: params.toString() });
}

ipcMain.handle("open-popout-window", (_event, opts) => {
  const streamKey = opts && opts.streamKey;
  if (!streamKey) return Promise.reject(new Error("open-popout-window requires a streamKey"));
  // Returning (not just calling) `openPopoutWindow`'s promise is what lets
  // a `loadFile` rejection inside it propagate through `ipcMain.handle` as
  // a rejected `invoke()` on the renderer side.
  return openPopoutWindow(streamKey, opts || {});
});

// Both handlers below resolve which pop-out window they apply to via
// `event.sender` (the calling renderer's own `WebContents`) rather than any
// module-level "the current window" reference — each pop-out window's own
// preload/renderer calls these implicitly targeting itself, which is what
// makes multiple simultaneous pop-out windows each independently
// fullscreen-able/closable with no key-threading needed on the JS side.
ipcMain.handle("viewer-set-fullscreen", (event, fullscreen) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (win && !win.isDestroyed()) {
    win.setFullScreen(!!fullscreen);
  }
});

ipcMain.handle("viewer-close", (event) => {
  const win = BrowserWindow.fromWebContents(event.sender);
  if (win && !win.isDestroyed()) {
    win.close();
  }
});

// Unlike the two handlers above (which resolve their target window via the
// calling renderer's own `event.sender`), this one is called FROM the main
// window to close a *different* window — the pop-out window for a given
// stream — so it resolves its target via `streamKey` against `popoutWindows`
// instead. Backs `view.js`'s "Bring it back" held-slot button: closing the
// real pop-out `BrowserWindow` here fires that window's own `closed` handler
// above unconditionally, which sends `popout-closed` back to `mainWindow` —
// `view.js`'s existing `onPopoutClosed` listener does the
// `poppedOutIds`/render cleanup from that signal, so this handler itself
// doesn't need to (and shouldn't) duplicate it.
ipcMain.handle("close-popout-window", (_event, streamKey) => {
  const win = popoutWindows.get(streamKey);
  if (win && !win.isDestroyed()) {
    win.close();
  }
});

// ---------------------------------------------------------------------
// App lifecycle
// ---------------------------------------------------------------------

app.whenReady().then(async () => {
  Menu.setApplicationMenu(null);
  try {
    await spawnSidecar();
    await waitForServer();
  } catch (err) {
    console.error("[electron] failed to start sidecar/api_server:", err);
    app.quit();
    return;
  }

  connectSse();
  createWindow();

  app.on("activate", () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

// Kill the sidecar and make sure it's actually gone: ask nicely (SIGTERM),
// then force it (SIGKILL) if it hasn't exited within a short grace period.
// Idempotent — safe to call from multiple lifecycle/signal paths.
let sidecarKillRequested = false;
function killSidecar() {
  if (!sidecarProcess || sidecarKillRequested) return;
  sidecarKillRequested = true;
  const proc = sidecarProcess;
  proc.kill("SIGTERM");
  const forceKillTimer = setTimeout(() => {
    if (!proc.killed) proc.kill("SIGKILL");
  }, 2000);
  // Don't let this timer keep the event loop (or process exit) alive.
  forceKillTimer.unref();
  proc.once("exit", () => clearTimeout(forceKillTimer));
}

app.on("window-all-closed", () => {
  sseAbort = true;
  killSidecar();
  if (process.platform !== "darwin") app.quit();
});

app.on("before-quit", () => {
  sseAbort = true;
  killSidecar();
});

// Explicit belt-and-suspenders signal handling, on top of the
// before-quit/window-all-closed paths above.
//
// Verified experimentally (see task history / commit message): on this
// Electron version (43.x, Linux), the browser process already installs its
// own internal handling for SIGTERM/SIGHUP/SIGINT that triggers Electron's
// normal quit flow — `before-quit` fires and the existing
// `sidecarProcess.kill()` above already runs, even when the signal is sent
// directly to just this process's PID (not its whole process group), and
// even though `sidecarProcess` is spawned without `detached: true` (default
// `false`, same process group as this one). So the sidecar was not actually
// found to be orphaned by an external SIGTERM/SIGHUP in testing.
//
// That said, that cascade is undocumented Electron/Chromium-internal
// behavior, not a public contract — it could change across Electron
// versions or platforms. These explicit handlers make the guarantee
// self-contained and add a SIGKILL fallback (via `killSidecar`'s grace
// timer) that the implicit path doesn't provide if the sidecar ever hangs
// on SIGTERM.
function cleanupAndExit(signal) {
  console.error(`[electron] received ${signal}, cleaning up sidecar and exiting`);
  sseAbort = true;
  killSidecar();
  process.exit(0);
}

process.on("SIGTERM", () => cleanupAndExit("SIGTERM"));
process.on("SIGHUP", () => cleanupAndExit("SIGHUP"));
process.on("SIGINT", () => cleanupAndExit("SIGINT"));

module.exports = {};
