// Task #77 through the REAL site UI: (a) double-click a watched tile -> fullscreen stage, double-click again -> back;
// (b) avatar gradients are a pure function of the display name (same hex stops as the native client, see dev/tests.js
// vectors; task #83 brought the 140deg fade back).
// Page "client-1" shares the synthetic pattern, page "client-2" watches it; the names are the ones the native rig uses, so
// the screenshots can be compared pixel for pixel with `hs-drive.sh shot`.
//
//   usage: node docs/dev/e2e/avatar-fullscreen.mjs <relay host:port> <certhash>   (relay: docs/dev/relay.sh start ...)
//   env:   E2E_OUT = where the screenshots go (default /tmp/sss-e2e), SHOT_PREFIX (default "t77_")
import { mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const [RELAY, HASH] = process.argv.slice(2);
if (!RELAY || !HASH) throw new Error("usage: avatar-fullscreen.mjs <relay host:port> <certhash>");
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
const PFX = process.env.SHOT_PREFIX || "t77_";
mkdirSync(OUT, { recursive: true });

let failed = 0;
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!cond) failed++;
};
// "linear-gradient(140deg, rgb(37, 147, 81), rgb(24, 98, 42))" -> "#259351 #18622a"
const hex = (img) => (img.match(/rgb\(\d+, \d+, \d+\)/g) || []).map((c) => "#" + c.match(/\d+/g).map((n) => Number(n).toString(16).padStart(2, "0")).join("")).join(" ");

const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "client-1" });
const B = await L.openApp(site.url, { name: "client-2" });
try {
  await L.createRoom(A.page, { name: "avatars", server: RELAY, hash: HASH });
  await L.waitJoined(A.page);
  await L.stubCapture(A.page, { audio: false });
  await L.goLive(A.page, { codec: "h264", fps: 30 });
  await L.addRoomByLink(B.page, await L.inviteLink(A.page), "avatars");
  await L.waitJoined(B.page);
  await L.watchFirst(B.page);
  await B.page.waitFor("__sss.S.rooms.some((r) => [...r.watches.values()].some((w) => w.firstFrame))", { timeout: 30000, what: "first frame" });

  // (b) avatar gradients: user band + People rows + share card, in both pages
  const C1 = "#259351 #18622a", C2 = "#93256c #621854";
  const colours = (page) => page.eval(`(() => ({
    band: getComputedStyle(document.querySelector(".p-user .avatar")).backgroundImage,
    people: [...document.querySelectorAll(".p-right .avatar")].map((a) => [a.textContent, getComputedStyle(a).backgroundImage]),
    all: [...document.querySelectorAll(".avatar")].map((a) => { const r = a.getBoundingClientRect(); return [a.firstChild && a.firstChild.textContent, getComputedStyle(a).backgroundImage, r.x, r.y, r.width]; }),
  }))()`);
  const cb = await colours(B.page);
  check(cb.band.startsWith("linear-gradient(140deg") && hex(cb.band) === C2, "viewer user band avatar = client-2 gradient", cb.band);
  check(cb.people.length === 2, "two People avatars", JSON.stringify(cb.people.map(([n, c]) => [n, hex(c)])));
  const hexes = cb.people.map(([, c]) => hex(c)).sort();
  check(JSON.stringify(hexes) === JSON.stringify([C1, C2]), `People avatars = client-1 ${C1} and client-2 ${C2}`, hexes.join(" | "));
  check(cb.all.every(([, img]) => img.startsWith("linear-gradient(140deg")), "every avatar on the page is a gradient", JSON.stringify(cb.all));
  console.log("AVATARS " + JSON.stringify(cb.all));
  const ca = await colours(A.page);
  check(hex(ca.band) === C1, "sharer user band avatar = client-1 gradient (same person, same colours in every viewer)", hex(ca.band));
  await B.page.screenshot(OUT + PFX + "browser_viewer_watching.png");

  // (a) double-click the tile -> fullscreen stage
  const fsInfo = () => B.page.eval(`(() => { const e = document.fullscreenElement; return e ? { cls: e.className, hasCanvas: !!e.querySelector("canvas"), w: e.getBoundingClientRect().width, h: e.getBoundingClientRect().height } : null; })()`);
  check((await fsInfo()) === null, "not fullscreen before");
  await B.page.dblclick(".tile");
  await B.page.waitFor("document.fullscreenElement", { what: "fullscreenElement after dblclick" });
  const f1 = await fsInfo();
  check(f1 && f1.cls.includes("pip-stage") && f1.hasCanvas, "double-click: the stage with the canvas is fullscreen", JSON.stringify(f1));
  check(f1 && f1.w >= 1000, "stage fills the viewport", `${f1 && f1.w}x${f1 && f1.h}`);
  await B.page.screenshot(OUT + PFX + "browser_fullscreen.png");
  // a re-render while fullscreen must not drop it (the whole tree is rebuilt on each state change)
  await B.page.eval(`__sss.render()`);
  await L.sleep(300);
  check((await fsInfo()) !== null, "fullscreen survives a re-render");
  // double-click on the stage -> back
  await B.page.dblclick(".pip-stage canvas");
  await B.page.waitFor("!document.fullscreenElement", { what: "fullscreen left after second dblclick" });
  await B.page.waitFor("document.querySelector('.tile canvas')", { what: "canvas back in its tile" });
  check(true, "second double-click: fullscreen left, canvas back in the tile");
  // fullscreen -> exitFullscreen() (what Esc does natively) -> back
  await B.page.dblclick(".tile");
  await B.page.waitFor("document.fullscreenElement", { what: "fullscreen again" });
  await B.page.eval(`document.exitFullscreen()`);
  await B.page.waitFor("!document.fullscreenElement && document.querySelector('.tile canvas')", { what: "exitFullscreen (Esc) returns the canvas" });
  check(true, "exitFullscreen() (= Esc) returns the canvas to its tile");
  const errs = [...A.consoleErrors, ...B.consoleErrors];
  check(errs.length === 0, "no console errors", errs.join(" | "));
} catch (e) {
  failed++;
  console.log("FAIL", e.message);
  await B.page.screenshot(OUT + PFX + "fail.png").catch(() => {});
} finally {
  await A.close();
  await B.close();
  site.close();
}
console.log(failed ? `${failed} FAILED` : "ALL PASSED");
process.exit(failed ? 1 : 0);
