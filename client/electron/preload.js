// Preload script: rebuilds `window.__TAURI__` with the exact shape
// `frontend/scripts/*.js` expects, so those files need zero changes when
// running under Electron instead of Tauri.
//
// Contract this mirrors (see frontend/scripts/shell.js, stream-control.js,
// settings.js, view.js):
//   - `window.__TAURI__.core.invoke(name, args)` — always used as
//     `await invoke(...)` inside try/catch or with a bare await; real Tauri
//     `invoke()` returns a Promise that resolves with the command's return
//     value or rejects with the command's error string. We mirror that with
//     `ipcRenderer.invoke('tauri-invoke', name, args)`, which is itself
//     promise-based, and the main process throws on non-200 responses so the
//     IPC promise rejects the same way.
//   - `window.__TAURI__.event.listen(name, callback)` — called
//     fire-and-forget in shell.js (`listen("room-connected", () => ...)`,
//     no `await`, no stored/used unlisten function). Real Tauri `listen()`
//     returns `Promise<UnlistenFn>`; we return the same shape (a Promise
//     resolving to an unlisten function) for compatibility even though
//     nothing in the current frontend awaits or calls it.
//   - The callback's argument has a `.payload` field
//     (`event.payload`, `event.payload.overshootKbps`, etc. in shell.js) —
//     we wrap whatever the main process forwards as `{ payload: data }`
//     before invoking the frontend's callback.

"use strict";

const { contextBridge, ipcRenderer } = require("electron");

const EVENT_NAMES = [
  "room-connected",
  "room-connect-failed",
  "room-disconnected",
  "bandwidth-overshoot",
  "presence-updated",
  "destination-state-changed",
];

function listen(eventName, callback) {
  const handler = (_ipcEvent, payload) => {
    callback({ payload });
  };
  ipcRenderer.on(eventName, handler);

  const unlisten = () => {
    ipcRenderer.removeListener(eventName, handler);
  };
  return Promise.resolve(unlisten);
}

contextBridge.exposeInMainWorld("__TAURI__", {
  core: {
    invoke: (cmd, args) => ipcRenderer.invoke("tauri-invoke", cmd, args || {}),
  },
  event: {
    listen,
  },
});

// Not strictly required by the frontend today, but harmless and documents
// which events the bridge forwards.
void EVENT_NAMES;

// ---------------------------------------------------------------------
// window.__ELECTRON__ — separate bridge for Electron-native window
// management (fullscreen/borderless stream viewer window). Distinct from
// __TAURI__ above: __TAURI__ is a narrow Tauri-invoke/event compatibility
// shim for talking to the Rust sidecar; this is Electron-specific window
// control that has no Tauri equivalent, so it gets its own namespace rather
// than being bolted onto __TAURI__.
//
// This same preload script also runs inside the viewer window (main.js
// passes the identical `preload` path when constructing it), so both
// window.__ELECTRON__ and window.__TAURI__ are available there too — a
// later unit's viewer.js can call back into __TAURI__ for stream data with
// no new backend contract needed.
contextBridge.exposeInMainWorld("__ELECTRON__", {
  // Promise<Array<{id, bounds, workArea, isPrimary}>>
  listDisplays: () => ipcRenderer.invoke("list-displays"),

  // opts: { mode: 'fullscreen' | 'borderless', displayId: number | null, streamKeys: string[] }
  // displayId: null (or omitted) => display nearest the current cursor position.
  openViewer: (opts) => ipcRenderer.invoke("open-viewer-window", opts),

  // Toggles fullscreen on the currently-open viewer window. No-op if none is open.
  setFullscreen: (fullscreen) => ipcRenderer.invoke("viewer-set-fullscreen", fullscreen),

  // Closes the currently-open viewer window, if any.
  closeViewer: () => ipcRenderer.invoke("viewer-close"),

  // Fires when the viewer window exits fullscreen via an OS-level gesture
  // (e.g. an Escape/WM shortcut) rather than through setFullscreen(false),
  // so caller UI state (e.g. a "fullscreen" toggle) can stay in sync.
  // Mirrors the __TAURI__.event.listen shape: returns Promise<UnlistenFn>.
  onViewerLeftFullscreen: (callback) => {
    const handler = () => callback();
    ipcRenderer.on("viewer-left-fullscreen", handler);
    return Promise.resolve(() => {
      ipcRenderer.removeListener("viewer-left-fullscreen", handler);
    });
  },
});
