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
// management (the pop-out stream viewer window). Distinct from __TAURI__
// above: __TAURI__ is a narrow Tauri-invoke/event compatibility shim for
// talking to the Rust sidecar; this is Electron-specific window control
// that has no Tauri equivalent, so it gets its own namespace rather than
// being bolted onto __TAURI__.
//
// This same preload script also runs inside every pop-out window (main.js
// passes the identical `preload` path when constructing one), so both
// window.__ELECTRON__ and window.__TAURI__ are available there too —
// viewer.js calls back into __TAURI__ for stream data with no new backend
// contract needed.
//
// 2026-07-28 consolidation: this used to back a *singleton* fullscreen/
// borderless viewer window (`openViewer({mode, displayId, streamKeys})`).
// That's gone — see `superdoc/features/viewer-window.md`'s matching dated
// entry. `openPopout` below opens one independent window per stream
// instead; `setFullscreen`/`closeViewer`/`onViewerLeftFullscreen` are
// unchanged in shape but now implicitly scoped to whichever pop-out window
// the calling renderer lives in (`electron/main.js` resolves that via
// `BrowserWindow.fromWebContents(event.sender)`, not a module-level
// singleton reference), so the same three calls work correctly no matter
// how many pop-out windows are open at once.
contextBridge.exposeInMainWorld("__ELECTRON__", {
  // opts: { streamKey: string } — `${room_handle}:${owner}:${stream_id}`,
  // the same tile-id scheme `view.js` already uses. Opens a new lean
  // pop-out window for that stream, or focuses the existing one if this
  // stream is already popped out (mirrors the old Tauri `pop_out_stream`
  // command's per-stream-label reuse behavior).
  openPopout: (opts) => ipcRenderer.invoke("open-popout-window", opts),

  // Toggles real OS fullscreen on the pop-out window this was called from.
  setFullscreen: (fullscreen) => ipcRenderer.invoke("viewer-set-fullscreen", fullscreen),

  // Closes the pop-out window this was called from.
  closeViewer: () => ipcRenderer.invoke("viewer-close"),

  // Fires when the pop-out window this is called from exits fullscreen via
  // an OS-level gesture (e.g. an Escape/WM shortcut) rather than through
  // setFullscreen(false), so caller UI state (e.g. an F11 toggle) can stay
  // in sync. Mirrors the __TAURI__.event.listen shape: returns
  // Promise<UnlistenFn>.
  onViewerLeftFullscreen: (callback) => {
    const handler = () => callback();
    ipcRenderer.on("viewer-left-fullscreen", handler);
    return Promise.resolve(() => {
      ipcRenderer.removeListener("viewer-left-fullscreen", handler);
    });
  },

  // Fires in the MAIN window's renderer (unlike the other three calls above,
  // which are scoped to whichever pop-out window called them) whenever ANY
  // pop-out window closes, with that window's streamKey — main.js's
  // `openPopoutWindow` sends this from the `BrowserWindow`'s own `'closed'`
  // event. `view.js` uses it to clear the closed stream's held-slot entry
  // from `poppedOutIds` and re-render, so the main View tab's tile resumes
  // normal rendering automatically instead of staying stuck as a held
  // placeholder forever. Mirrors the `Promise<UnlistenFn>` shape the other
  // listen-style calls here use, even though nothing currently unlistens.
  onPopoutClosed: (callback) => {
    const handler = (_ipcEvent, streamKey) => callback(streamKey);
    ipcRenderer.on("popout-closed", handler);
    return Promise.resolve(() => {
      ipcRenderer.removeListener("popout-closed", handler);
    });
  },
});
