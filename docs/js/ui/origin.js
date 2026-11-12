// Which window a modal or toast belongs to (pop-out parity with native rows 182/183): the one the user last pressed a key or button in.
// Menus already know their window (menus.js hostIdOf); modals and toasts are raised from many places that have no element at hand, so
// bindWindow (stage.js) records every pointerdown/keydown here and the stamp is taken when the modal/toast is created.
// Pure: no DOM, no clock of its own (tests pass `now`).

export const ORIGIN_WINDOW_MS = 2000; // a toast that follows a click by a promise (clipboard) still counts; a relay event minutes later does not
let last = { host: "main", at: -Infinity };
/** Remember that the user just acted in window `host` ("main" | "pop"). */
export const noteInput = (host, now) => { last = { host, at: now }; };
/** The window a thing raised now belongs to: "pop" only when the stage window is open and the last input was in it, recently. */
export const originHost = (now, popOpen) => (popOpen && last.host === "pop" && now - last.at < ORIGIN_WINDOW_MS ? "pop" : "main");
/** Where a stamped thing is drawn: its own window while that is open, else the main one (the pop-out closed under it). */
export const shownIn = (hostId, popOpen) => (hostId === "pop" && popOpen ? "pop" : "main");
