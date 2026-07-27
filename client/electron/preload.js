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
