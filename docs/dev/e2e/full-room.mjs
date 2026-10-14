// Scenario 3: room full -> the real UI shows the proper error. 8 core clients fill a room inside page A; page B (real UI) is the 9th.
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const HASH = process.argv[2];
import { mkdirSync } from "node:fs";
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
mkdirSync(OUT, { recursive: true });
const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Filler" });
const B = await L.openApp(site.url, { name: "Ninth" });
try {
  const link = await L.createRoom(A.page, { name: "Full", server: "127.0.0.1:14433", hash: HASH });
  await L.waitJoined(A.page);
  // 7 more members through the core (the UI page is member 1)
  await A.page.eval(`(async () => { const core = await import("${L.BASE}js/core/index.js"); window.fill = [];
    for (let i = 0; i < 7; i++) { const c = new core.RoomClient({ displayName: "f" + i }); await c.connect(${JSON.stringify(link)}); window.fill.push(c); } return window.fill.length; })()`);
  const n = await A.page.eval("__sss.S.rooms[0].client.members.size");
  console.log("members in room:", n);
  await L.addRoomByLink(B.page, link, "Full");
  await B.page.waitFor("document.body.innerText.includes('full')", { timeout: 15000, what: "room-full text" });
  const text = await B.page.eval("document.body.innerText");
  console.log("UI text (excerpt):", text.split("\n").filter((l) => /full|Connect|connect/i.test(l)).join(" | "));
  await B.page.screenshot(OUT + "s3-room-full-modal.png");
  // let go of a seat and retry
  await A.page.eval("window.fill[0].disconnect()");
  await L.sleep(500);
  await B.page.eval("document.querySelector('#btn-join') ? 0 : 0");
  console.log("B state:", await B.page.eval("__sss.S.rooms[0] ? __sss.S.rooms[0].state + ' | ' + __sss.S.rooms[0].error : 'no room entry (modal still open)'"));
  console.log("errors A", A.consoleErrors, A.badRequests, "B", B.consoleErrors, B.badRequests);
} catch (e) {
  console.log("FAIL", e.message);
  await B.page.screenshot(OUT + "s3-fail.png");
} finally {
  await A.close();
  await B.close();
  site.close();
}
process.exit(0);
