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

const { app, BrowserWindow, ipcMain, screen } = require("electron");
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

let apiToken = null;
let apiPort = null;
let mainWindow = null;
let viewerWindow = null;
let sidecarProcess = null;
let sseAbort = false;

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
}

// ---------------------------------------------------------------------
// Window creation
// ---------------------------------------------------------------------

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 800,
    height: 600,
    title: "Simple Screensharing",
    webPreferences: {
      preload: PRELOAD,
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  mainWindow.loadFile(FRONTEND_INDEX);
}

// ---------------------------------------------------------------------
// Viewer window: dedicated fullscreen/borderless stream-viewing window.
//
// Only one viewer window at a time for this pass — opening a new one
// closes/replaces any existing one first (see `open-viewer-window` below).
// ---------------------------------------------------------------------

function mapDisplay(d) {
  return {
    id: d.id,
    bounds: d.bounds,
    workArea: d.workArea,
    isPrimary: d.id === screen.getPrimaryDisplay().id,
  };
}

function resolveTargetDisplay(displayId) {
  if (displayId !== null && displayId !== undefined) {
    const found = screen.getAllDisplays().find((d) => d.id === displayId);
    if (found) return found;
  }
  // Default: the display nearest the current cursor position (matches this
  // environment's own window-spawn convention).
  return screen.getDisplayNearestPoint(screen.getCursorScreenPoint());
}

// streamKeys is handed to the viewer window via a query string on the
// loaded file URL (?streamKeys=<json-encoded-array>), read by the later
// unit's viewer.js on load via `new URLSearchParams(location.search)`.
// Chosen over an IPC message sent after `did-finish-load` because it's
// available synchronously to the viewer's own startup code with no race
// against when the renderer attaches its listener.
function loadViewerContents(win, streamKeys) {
  const query = `streamKeys=${encodeURIComponent(JSON.stringify(streamKeys || []))}`;
  win.loadFile(VIEWER_HTML, { search: query });
}

// mode: 'fullscreen' | 'borderless'
// displayId: number | null — null means "nearest the cursor" (see resolveTargetDisplay)
// streamKeys: string[]
function openViewerWindow({ mode, displayId, streamKeys }) {
  if (viewerWindow && !viewerWindow.isDestroyed()) {
    viewerWindow.close();
    viewerWindow = null;
  }

  const target = resolveTargetDisplay(displayId != null ? displayId : null);
  const { x, y, width, height } = target.bounds;

  const borderless = mode === "borderless";

  // Critical ordering (known Linux WM footgun): position the window on the
  // target display's bounds FIRST — passed at construction here — and only
  // call setFullScreen(true) AFTER the window is positioned and shown.
  // Calling setFullScreen before the window is on the correct
  // display/visible is unreliable on Linux WMs (can fullscreen on the
  // wrong/primary display instead of the requested one).
  viewerWindow = new BrowserWindow({
    x,
    y,
    width,
    height,
    frame: !borderless,
    resizable: true,
    show: false,
    webPreferences: {
      preload: PRELOAD,
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  loadViewerContents(viewerWindow, streamKeys);

  viewerWindow.once("ready-to-show", () => {
    if (!viewerWindow || viewerWindow.isDestroyed()) return;
    // Re-assert bounds before showing: some Linux WMs can reposition an
    // unmapped window on first map, so pin it again right before show().
    viewerWindow.setBounds(target.bounds);
    viewerWindow.show();

    if (mode === "fullscreen") {
      // Standard fullscreen (EWMH _NET_WM_STATE_FULLSCREEN on X11 / xdg-shell
      // fullscreen on Wayland) — NOT kiosk mode, which is more aggressive
      // and less predictable across Linux window managers.
      viewerWindow.setFullScreen(true);
    } else {
      // Borderless: full display bounds (not workArea — maximize() would
      // snap to workArea and exclude panel/taskbar space, which is wrong
      // here; the user wants true maximum size). Stays a normal,
      // WM-managed, alt-tabbable window otherwise: resizable, not
      // always-on-top, not skip-taskbar — chrome-free is the only
      // difference from a regular window.
      viewerWindow.setBounds(target.bounds);
    }
  });

  viewerWindow.on("leave-full-screen", () => {
    // Forwards an OS-level fullscreen exit (e.g. an Escape/WM gesture, not
    // triggered through our own setFullscreen(false) IPC call) to the
    // renderer so its UI state can stay in sync.
    if (viewerWindow && !viewerWindow.isDestroyed()) {
      viewerWindow.webContents.send("viewer-left-fullscreen");
    }
  });

  viewerWindow.on("closed", () => {
    viewerWindow = null;
  });
}

ipcMain.handle("list-displays", () => {
  return screen.getAllDisplays().map(mapDisplay);
});

ipcMain.handle("open-viewer-window", (_event, opts) => {
  openViewerWindow(opts || {});
});

ipcMain.handle("viewer-set-fullscreen", (_event, fullscreen) => {
  if (viewerWindow && !viewerWindow.isDestroyed()) {
    viewerWindow.setFullScreen(!!fullscreen);
  }
});

ipcMain.handle("viewer-close", () => {
  if (viewerWindow && !viewerWindow.isDestroyed()) {
    viewerWindow.close();
  }
});

// ---------------------------------------------------------------------
// App lifecycle
// ---------------------------------------------------------------------

app.whenReady().then(async () => {
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
