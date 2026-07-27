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

const { app, BrowserWindow, ipcMain } = require("electron");
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

let apiToken = null;
let apiPort = null;
let mainWindow = null;
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
      preload: path.join(__dirname, "preload.js"),
      contextIsolation: true,
      nodeIntegration: false,
    },
  });

  mainWindow.loadFile(FRONTEND_INDEX);
}

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

app.on("window-all-closed", () => {
  sseAbort = true;
  if (sidecarProcess) sidecarProcess.kill();
  if (process.platform !== "darwin") app.quit();
});

app.on("before-quit", () => {
  sseAbort = true;
  if (sidecarProcess) sidecarProcess.kill();
});

module.exports = {};
