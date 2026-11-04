// Task I10: a headless Chromium in the same room as the native clients of the headless rig (client/scripts/hs-rig.sh), for the webcam and the
// webcam in picture in both directions. No audio anywhere (the camera is `audio: false`, the screen is the harness's silent pattern).
//
//   1. Start the rig with a fake camera on client 1 and no sound:
//        CLIENT_ENV_1="SSS_FAKE_CAMERA=1 SSS_SOUND_SINK=fakesink" CLIENT_ENV_2="SSS_SOUND_SINK=fakesink" client/scripts/hs-rig.sh up   (prints the join link)
//      On client 1: share the screen (`hs-drive.sh share; setup 60 h264 quality; golive`) and start the webcam (`hs-drive.sh uclick 1 ctl.webcam`).
//   2. node docs/dev/e2e/native-webcam.mjs '<join link>' [--hold 120]
//      Phase A: the browser watches the native screen and the native webcam, switches Webcam in picture on, checks the PIP is inside the native
//      screen's tile and that both decode, screenshots to $E2E_OUT. Phase B: the browser shares a silent screen and its own webcam into the room
//      and holds them for --hold seconds ("BROWSER SHARING" on stdout): on client 2 watch both (`hs-drive.sh uclick 2 watch.0` ...) and look for
//      the browser's webcam as a tile or, with Layout > Webcam in picture, in the browser's screen.
import { mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const args = process.argv.slice(2);
const link = args.find((a) => !a.startsWith("--"));
const hold = Number(args.includes("--hold") ? args[args.indexOf("--hold") + 1] : 120);
if (!link) throw new Error("usage: native-webcam.mjs '<join link>' [--hold SECONDS]");
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
mkdirSync(OUT, { recursive: true });
const sleep = L.sleep;
let failed = 0;
const check = (c, what, extra = "") => {
  console.log(`${c ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!c) failed++;
};

const site = await L.serveSite(0);
const b = await L.openApp(site.url, { name: "browser", flags: ["--use-fake-device-for-media-stream"] });
await b.conn.send("Browser.grantPermissions", { permissions: ["videoCapture"], origin: new URL(site.url).origin }).catch(() => {});
const P = b.page;
try {
  await L.addRoomByLink(P, link, "rig");
  await L.waitJoined(P);
  await P.waitFor("document.querySelectorAll('.st-tile.idle').length >= 2", { timeout: 30000, what: "the native screen and webcam as idle tiles" });
  check(await P.eval("[...document.querySelectorAll('.st-tile.idle')].some((t) => /\\/video\\/Webcam$/.test(t.dataset.id))"), "a native webcam (label `Webcam`) is an idle tile in the browser");
  for (let i = 0; i < 8 && (await P.eval("__sss.S.rooms[0].watches.size")) < 2; i++) {
    await P.click({ js: "document.querySelector('.st-tile.idle [data-watch]')" });
    await sleep(900);
  }
  await P.waitFor("[...__sss.S.rooms[0].watches.values()].length === 2 && [...__sss.S.rooms[0].watches.values()].every((w) => w.firstFrame)", { timeout: 40000, what: "first frames of the native screen and webcam" });
  const stats = () => P.eval("[...__sss.S.rooms[0].watches.values()].map((w) => ({ label: w.label, decoded: __sss.S.rooms[0].client.watchStats(w.key).decoded, audio: !!w.handle.audioKey }))");
  const s1 = await stats();
  await sleep(2000);
  const s2 = await stats();
  check(s2.every((x, i) => x.decoded > s1[i].decoded && !x.audio), "both native streams decode in the browser and the webcam watch plays no audio", JSON.stringify(s2));
  await P.screenshot(OUT + "native_cam_tiles.png");
  await P.click("#btn-layout");
  await P.waitFor("document.querySelector('.menu.m-layout')", { what: "layout menu" });
  await P.click({ js: "[...document.querySelectorAll('.menu .cbx')].find((e) => /Webcam in picture/.test(e.innerText))" });
  await P.key("Escape");
  await sleep(500);
  const pip = await P.eval("(() => { const e = document.querySelector('.arena > .st-tile > .pip'); if (!e) return null; const r = e.getBoundingClientRect(), t = e.parentElement.getBoundingClientRect(); return { parent: e.parentElement.dataset.id, id: e.dataset.id, inside: r.x >= t.x - 1 && r.y >= t.y - 1 && r.right <= t.right + 1 && r.bottom <= t.bottom + 1 }; })()");
  check(pip && /\/video\/Screen/.test(pip.parent) && /\/video\/Webcam$/.test(pip.id) && pip.inside, "the native webcam sits in the native screen's picture (webcam in picture)", JSON.stringify(pip));
  await P.screenshot(OUT + "native_cam_pip.png");

  // Phase B: the browser's own screen and webcam into the room
  await L.stubCapture(P, { width: 1280, height: 720, fps: 30, audio: false });
  await L.goLive(P, { codec: "h264", fps: 30 });
  await P.click("#btn-webcam");
  await P.waitFor("__sss.S.captures.some((c) => c.kind === 'webcam' && c.state === 'live')", { timeout: 20000, what: "the browser's webcam live" });
  await sleep(1500);
  check(await P.eval("!!document.querySelector('.st-tile.mine > .pip, .st-tile.mine.cam')"), "the browser's own webcam is on its stage (in its own screen's picture or as a tile)");
  await P.screenshot(OUT + "browser_cam_own.png");
  console.log(`BROWSER SHARING (screen + webcam), holding ${hold} s`);
  await sleep(hold * 1000);
  check(b.consoleErrors.length === 0, "no console errors in the browser", b.consoleErrors.join(" | "));
} catch (e) {
  failed++;
  console.log("FAIL", e.stack || e.message);
  await P.screenshot(OUT + "native_cam_fail.png").catch(() => {});
} finally {
  await b.close().catch(() => {});
  site.close();
}
console.log(failed ? `${failed} FAILED` : "ALL PASSED");
process.exit(failed ? 1 : 0);
