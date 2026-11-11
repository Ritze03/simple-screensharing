// Row 178 through the REAL site UI: Shift + left-drag pans the picture inside its cell in the Crop layout (parity with native row 177).
// Alice shares a 16:9 synthetic screen (video only, no audio stream anywhere); Bob watches it in a tall narrow window, so Crop cuts the sides.
// Real pointer input through CDP Input.dispatchMouseEvent with the Shift modifier (modifiers: 8). Checks: the pan follows the pointer 1:1, clamps
// so the picture covers the cell, the axis without overflow does not move, a plain drag does not pan, Split ignores the offset, "Reset sizes and
// positions" recentres, the grab cursor shows while Shift is held.
//
//   usage: node docs/dev/e2e/crop-pan.mjs <relay host:port> <certhash>   (relay: docs/dev/relay.sh start ...)
//   env:   E2E_OUT = where the screenshots go (default ~/tmp/sss-e2e)
import { mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const [RELAY, HASH] = process.argv.slice(2).filter((a) => !a.startsWith("--"));
if (!RELAY || !HASH) throw new Error("usage: crop-pan.mjs <relay host:port> <certhash>");
const OUT = (process.env.E2E_OUT || process.env.HOME + "/tmp/sss-e2e") + "/";
mkdirSync(OUT, { recursive: true });
let failed = 0;
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!cond) failed++;
};
const near = (a, b, eps, what) => check(Math.abs(a - b) <= eps, what, `${(+a).toFixed(3)} vs ${(+b).toFixed(3)} (+-${eps})`);

const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Alice" });
const B = await L.openApp(site.url, { name: "Bob" });
const P = B.page;
const state = () => P.eval(`(() => {
  const ar = document.querySelector('.arena'); const t = ar.querySelector('.st-tile'); const v = t.querySelector('.vid');
  const tr = t.getBoundingClientRect(), vr = v.getBoundingClientRect();
  const pan = __sss.vg(ar.dataset.view).pan[t.dataset.id] || null;
  return { crop: ar.classList.contains('crop'), tile: { x: tr.x, y: tr.y, w: tr.width, h: tr.height }, vid: { x: vr.x, y: vr.y, w: vr.width, h: vr.height },
    px: parseFloat(t.style.getPropertyValue('--px')), py: parseFloat(t.style.getPropertyValue('--py')), pan: pan && { ...pan },
    cursor: getComputedStyle(t).cursor, panning: ar.classList.contains('panning') };
})()`);
const mouse = (type, x, y, shift) => P.send("Input.dispatchMouseEvent", { type, x, y, button: type === "mouseMoved" ? "none" : "left", buttons: type === "mouseReleased" ? 0 : 1, clickCount: 1, modifiers: shift ? 8 : 0 });
/** Press at the tile centre (+ offset), move by (dx, dy) in steps, release. `hold` runs between the last move and the release. */
async function drag(dx, dy, { shift = true, hold = null, from = null } = {}) {
  const s = await state();
  const x0 = from ? from.x : s.tile.x + s.tile.w / 2, y0 = from ? from.y : s.tile.y + s.tile.h / 2;
  await P.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: x0, y: y0, modifiers: shift ? 8 : 0 });
  await mouse("mousePressed", x0, y0, shift);
  for (let i = 1; i <= 8; i++) await mouse("mouseMoved", x0 + (dx * i) / 8, y0 + (dy * i) / 8, shift);
  if (hold) await hold();
  await mouse("mouseReleased", x0 + dx, y0 + dy, shift);
  await L.sleep(120);
}
/** A sweep across the whole cell, from edge to edge (more than the overflow), always inside the window. */
const sweep = async (dir) => {
  const s = await state();
  const a = s.tile.x + 10, b = s.tile.x + s.tile.w - 10, y = s.tile.y + s.tile.h / 2;
  await drag(dir < 0 ? a - b : b - a, 0, { from: { x: dir < 0 ? b : a, y } });
};
const menuItem = async (re) => {
  await P.click("#btn-layout");
  await P.waitFor("document.querySelector('.menu.m-layout')", { what: "layout menu" });
  await P.click({ js: `[...document.querySelectorAll('.menu .mi, .menu .cbx')].find((e) => ${re}.test(e.innerText))` });
  await L.sleep(250);
};

try {
  await L.createRoom(A.page, { name: "croppan", server: RELAY, hash: HASH });
  await L.waitJoined(A.page);
  const link = await L.inviteLink(A.page);
  await L.addRoomByLink(P, link, "croppan");
  await L.waitJoined(P);
  await L.stubCapture(A.page, { audio: false, fps: 30, width: 1920, height: 1080 });
  await L.goLive(A.page, { codec: "h264", fps: 30 });
  await P.waitFor(`__sss.S.rooms[0].watches.size === 1`, { timeout: 25000, what: "Bob auto-watches" });
  await P.waitFor(`document.querySelector('.arena .st-tile .vid canvas')`, { what: "canvas" });
  await B.page.setViewport(700, 900);
  await L.sleep(600);

  await P.click("#btn-layout");
  await P.waitFor("document.querySelector('.menu.m-layout')", { what: "layout menu" });
  const sub = await P.eval(`[...document.querySelectorAll('.menu .mi')].find((e) => /Crop/.test(e.innerText)).innerText`);
  check(/Shift\+drag to pan/.test(sub), "the Crop menu item hints at Shift+drag", JSON.stringify(sub));
  await P.click({ js: `[...document.querySelectorAll('.menu .mi')].find((e) => /Crop/.test(e.innerText))` });
  await L.sleep(400);

  let s = await state();
  check(s.crop && s.vid.w > s.tile.w + 20 && s.vid.h <= s.tile.h + 1, "Crop crops the sides in this window", `tile ${s.tile.w.toFixed(0)}x${s.tile.h.toFixed(0)}, picture ${s.vid.w.toFixed(0)}x${s.vid.h.toFixed(0)}`);
  near(s.px, 0.5, 1e-6, "starts centred (x)");
  near(s.vid.x - s.tile.x, -(s.vid.w - s.tile.w) / 2, 1, "the picture is centred in its cell");
  const over = s.vid.w - s.tile.w;
  await P.screenshot(OUT + "croppan_0_centred.png");

  // plain drag (no Shift) does not pan
  await drag(-60, 0, { shift: false });
  s = await state();
  check(s.pan === null && Math.abs(s.px - 0.5) < 1e-6, "a plain drag does not pan");

  // Shift + drag 1:1
  const x0 = (await state()).vid.x;
  await drag(-60, 25, { hold: async () => {
    const m = await state();
    check(m.panning, "while panning the arena has .panning");
    check(m.cursor === "grabbing" || m.cursor === "grab", "the tile cursor is grab/grabbing while panning", m.cursor);
  } });
  s = await state();
  near(s.vid.x - x0, -60, 1.5, "Shift+drag left moves the picture 60 px left, 1:1");
  near(s.px, 0.5 + 60 / over, 0.01, "stored x position = 0.5 + 60/overflow");
  near(s.py, 0.5, 1e-6, "no vertical overflow: the y axis does not move");
  check(s.pan && s.pan.y === 0.5, "pan state keeps y centred");
  await P.screenshot(OUT + "croppan_1_panned.png");

  // continue from the stored position, drag back right
  await drag(30, 0);
  s = await state();
  near(s.px, 0.5 + 30 / over, 0.01, "a second drag continues from the stored position");

  // clamp both ways
  await sweep(-1);
  s = await state();
  check(s.px === 1 && s.vid.x + s.vid.w >= s.tile.x + s.tile.w - 0.5 && s.vid.x <= s.tile.x + 0.5, "far left: clamped at 1, the picture still covers the cell", `px ${s.px}`);
  near(s.vid.x + s.vid.w, s.tile.x + s.tile.w, 1, "its right edge sits on the cell's right edge");
  await sweep(1);
  s = await state();
  check(s.px === 0 && s.vid.x >= s.tile.x - 0.5, "far right: clamped at 0, its left edge on the cell's left edge", `px ${s.px}`);
  near(s.vid.x, s.tile.x, 1, "left edge flush");

  // survives a window resize (percentage based): pan 1 then widen
  await sweep(-1);
  await B.page.setViewport(820, 900);
  await L.sleep(700);
  s = await state();
  check(s.px === 1 && s.vid.x + s.vid.w >= s.tile.x + s.tile.w - 0.5, "a resize keeps the offset and the picture still covers the cell", `px ${s.px}`);
  await B.page.setViewport(700, 900);
  await L.sleep(500);

  // the grab cursor while Shift is held (a bare key event, no drag)
  await P.send("Input.dispatchKeyEvent", { type: "rawKeyDown", key: "Shift", code: "ShiftLeft", windowsVirtualKeyCode: 16, modifiers: 8 });
  s = await state();
  const t0 = await P.eval(`document.documentElement.classList.contains('shift-held')`);
  check(t0 && s.cursor === "grab", "Shift held over a cropped tile: grab cursor", s.cursor);
  await P.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Shift", code: "ShiftLeft", windowsVirtualKeyCode: 16, modifiers: 0 });
  check(!(await P.eval(`document.documentElement.classList.contains('shift-held')`)), "Shift released: the class goes");

  // Split ignores the offset
  await menuItem(/Split/);
  s = await state();
  check(!s.crop && Math.abs(s.vid.x + s.vid.w / 2 - (s.tile.x + s.tile.w / 2)) < 1 && s.pan && s.pan.x === 1, "Split shows the picture centred; the offset is kept for Crop");
  const before = (await state()).pan;
  await drag(-50, 0);
  check(JSON.stringify((await state()).pan) === JSON.stringify(before), "Shift+drag in Split does not pan");
  await menuItem(/Crop/);
  s = await state();
  check(s.px === 1, "back in Crop the offset is applied again");

  // Reset recentres
  await menuItem(/Reset sizes/);
  s = await state();
  check(s.pan === null && Math.abs(s.px - 0.5) < 1e-6 && Math.abs(s.vid.x - s.tile.x + over / 2) < 1.5, "Reset sizes and positions recentres the picture", `px ${s.px}`);
  await P.screenshot(OUT + "croppan_2_reset.png");
} catch (e) {
  failed++;
  console.log("FAIL", e.message);
  await P.screenshot(OUT + "croppan_fail.png").catch(() => {});
} finally {
  await A.close();
  await B.close();
  site.close();
}
console.log(failed ? `${failed} FAILED` : "ALL PASSED");
process.exit(failed ? 1 : 0);
