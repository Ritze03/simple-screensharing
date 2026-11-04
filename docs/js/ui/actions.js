// The hooks the stage modules (stage.js, bar.js, menus.js) call back into app.js with, set once by app.js at boot. A plain object
// instead of imports because app.js imports the stage modules (a cycle otherwise): render, the modals it owns, and the few
// helpers that need the render's `live`/`meters` registries.

export const A = {
  render() {},
  /** Text patched in place every stats tick (a re-render never interrupts a drag or a hover). */
  liveText: (el) => el,
  liveMeter: () => document.createElement("span"),
  openStreamModal() {},
  /** Pick a screen for a room (user gesture) and open the stream modal. */
  startStage() {},
  copyLink() {},
  /** Leave a room: confirm modal when something would be lost, else at once. */
  leaveRoom() {},
  openModal() {},
  doPopOut() {},
  doFullscreen() {},
  watchAndPop() {},
};
