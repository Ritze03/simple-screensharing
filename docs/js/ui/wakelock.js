// Screen Wake Lock: the viewer draws into a <canvas>, so a phone sees no playing <video> and lets the screen sleep.
// `syncWakeLock()` (called by render()) holds the lock while we watch or share anything, or on a phone while joined to any room.
import { S, PHONE } from "./store.js";
import { allDests } from "./capture.js";

let lock = null, pending = false;
const wanted = () => S.rooms.some((r) => r.watches.size) || allDests().length > 0 || (PHONE.matches && S.rooms.some((r) => r.state === "joined"));

export function syncWakeLock() {
  if (!navigator.wakeLock) return;
  if (!wanted()) { if (lock) { lock.release().catch(() => {}); lock = null; } return; }
  if (lock && !lock.released) return;
  if (pending) return;
  pending = true;
  navigator.wakeLock.request("screen").then((s) => {
    pending = false;
    if (!wanted()) return void s.release().catch(() => {}); // not needed any more by the time it was granted
    lock = s;
  }, () => { pending = false; }); // refused (no gesture, low battery): the next tap or render retries, silently
}
document.addEventListener("visibilitychange", () => { if (document.visibilityState === "visible") syncWakeLock(); }); // the browser dropped it while hidden
document.addEventListener("pointerdown", syncWakeLock, true); // Safari wants a user gesture
