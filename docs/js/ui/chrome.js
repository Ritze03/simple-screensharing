// Fullscreen auto-hide of the stage's chrome (people strip, control bar, tile overlays), the pure rule: a port of the native
// `client/src/ui/stage/chrome.rs` (the replica's chromeSync / chromePointer), tested by dev/tests.js "fullscreen chrome".
//
// In fullscreen the chrome lies over the stage and is shown only while the pointer is over the stage and moved in the last
// CHROME_IDLE_MS. An open menu, modal or drag HOLDS it up wherever the pointer is and stops the timer; when the hold ends the
// chrome gets a FULL idle period again, so closing a menu never hides the bar at once. Entering fullscreen starts the same
// grace. Outside fullscreen the chrome always shows and this is not consulted.

export const CHROME_IDLE_MS = 3000;

/** The state on entering fullscreen at `now` (ms). */
export const chromeStart = (now) => ({ lastActive: now, grace: true, held: false });

/** ms until the chrome would go, for scheduling a timer (null when it is held or already gone). */
export function chromeRemaining(s, now) {
  return !s.held && now - s.lastActive < CHROME_IDLE_MS ? CHROME_IDLE_MS - (now - s.lastActive) : null;
}

/** One step. `inside`: the pointer is over the stage; `active`: the pointer moved or pressed, the wheel turned or a key was
 *  pressed since the last step; `held`: a menu, modal or drag is open. Returns `[nextState, shown]`. */
export function chromeStep(s, now, inside, active, held) {
  if (held) return [{ lastActive: now, grace: true, held: true }, true];
  if (s.held) {
    // The hold just ended: a full period from now, wherever the pointer is.
    s = { lastActive: now, grace: true, held: false };
  } else if (active && inside) {
    s = { lastActive: now, grace: false, held: false };
  } else if (active) {
    // A key press with the pointer off the stage brings the bar back for one period too.
    s = { ...s, lastActive: now, grace: true };
  }
  const shown = now - s.lastActive < CHROME_IDLE_MS && (inside || s.grace);
  return [s, shown];
}
