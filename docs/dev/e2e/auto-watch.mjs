// Row 176 through the REAL site UI: Settings > Playback > "Automatically watch new streams" (on by default).
// Alice shares (synthetic capture, video only). Bob (default settings) is in the room BEFORE she goes live and must start watching by himself;
// Zoe has the option off (stored setting) and must only list the stream; Dora joins AFTER the stream runs and must not watch (snapshot is a baseline).
// Then Zoe flips the option on through the Settings UI (persisted) and Alice restarts the share: Zoe now auto-watches. A stopped watch is not
// re-watched (stableKey resume is covered by ui-reconnect.mjs).
// Row 181: Alice also turns on her (fake, video only) webcam; Eve joins late with default settings and watches the webcam by herself but only lists
// the running screen share; Fay joins late with the option off and watches nothing.
//
//   usage: node docs/dev/e2e/auto-watch.mjs <relay host:port> <certhash>   (relay: docs/dev/relay.sh start ...)
//   env:   E2E_OUT = where the screenshots go (default ~/tmp/sss-e2e)
import { mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const [RELAY, HASH] = process.argv.slice(2).filter((a) => !a.startsWith("--"));
if (!RELAY || !HASH) throw new Error("usage: auto-watch.mjs <relay host:port> <certhash>");
const OUT = (process.env.E2E_OUT || process.env.HOME + "/tmp/sss-e2e") + "/";
mkdirSync(OUT, { recursive: true });
let failed = 0;
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!cond) failed++;
};
const watching = (p) => p.page.eval(`__sss.S.rooms[0].watches.size`);
const listed = (p) => p.page.eval(`__sss.S.rooms[0].streams.filter((s) => !s.self && s.kind === "video").length`);

const site = await L.serveSite(0);
const fakeCam = { flags: ["--use-fake-device-for-media-stream"] };
const A = await L.openApp(site.url, { name: "Alice", ...fakeCam });
await A.conn.send("Browser.grantPermissions", { permissions: ["videoCapture"], origin: new URL(site.url).origin }).catch(() => {});
const B = await L.openApp(site.url, { name: "Bob" });
const Z = await L.openApp(site.url, { name: "Zoe", settings: { autoWatch: false } });
let D = null;
let E = null;
let F = null;
try {
  check((await B.page.eval(`__sss.S.settings.autoWatch`)) === true, "default: autoWatch on (no stored value)");
  check((await Z.page.eval(`__sss.S.settings.autoWatch`)) === false, "stored autoWatch:false loads as off");
  await L.createRoom(A.page, { name: "autowatch", server: RELAY, hash: HASH });
  await L.waitJoined(A.page);
  const link = await L.inviteLink(A.page);
  for (const P of [B, Z]) {
    await L.addRoomByLink(P.page, link, "autowatch");
    await L.waitJoined(P.page);
  }
  await L.stubCapture(A.page, { audio: false, fps: 30 });
  await L.goLive(A.page, { codec: "h264", fps: 30 });
  await B.page.waitFor(`__sss.S.rooms[0].watches.size === 1`, { timeout: 20000, what: "Bob auto-watches" });
  check(true, "option on: Bob watches the new stream without a click");
  await Z.page.waitFor(`__sss.S.rooms[0].streams.some((s) => !s.self && s.kind === "video")`, { what: "Zoe lists it" });
  await L.sleep(3000);
  check((await watching(Z)) === 0 && (await listed(Z)) === 1, "option off: Zoe lists the stream, does not watch it");
  await Z.page.screenshot(OUT + "autowatch_off_idle.png");
  await B.page.screenshot(OUT + "autowatch_on_watching.png");

  D = await L.openApp(site.url, { name: "Dora" });
  await L.addRoomByLink(D.page, link, "autowatch");
  await L.waitJoined(D.page);
  await L.sleep(3000);
  check((await watching(D)) === 0 && (await listed(D)) === 1, "joining a room with a running stream: listed, not watched (snapshot = baseline)");

  // Zoe flips the option on in Settings > Playback (real clicks) and it persists
  await Z.page.click("#btn-settings");
  await Z.page.click({ text: "Playback", sel: ".seg button" });
  await Z.page.waitFor(`document.querySelector("#set-autowatch")`, { what: "checkbox" });
  check((await Z.page.eval(`document.querySelector("#set-autowatch").getAttribute("aria-checked")`)) === "false", "settings checkbox shows off");
  await Z.page.screenshot(OUT + "autowatch_settings_off.png");
  await Z.page.click("#set-autowatch");
  await Z.page.waitFor(`document.querySelector("#set-autowatch")?.getAttribute("aria-checked") === "true"`, { what: "checkbox on" });
  check((await Z.page.eval(`JSON.parse(localStorage.getItem("sss.settings.v1")).autoWatch`)) === true, "toggle persisted in localStorage");
  await Z.page.screenshot(OUT + "autowatch_settings_on.png");
  await Z.page.click("#btn-settings-done");

  // Alice stops and goes live again: a NEW stream; Zoe (now on), Bob and Dora (default) all watch it. A stopped watch on Bob stays stopped
  // while the same stream keeps running (only a NEW key is auto-watched).
  await B.page.eval(`import(new URL("js/ui/rooms.js", location.href).href).then((m) => { const r = __sss.S.rooms[0]; m.unwatchStream(r, [...r.watches.keys()][0]); })`);
  await L.sleep(2000);
  check((await watching(B)) === 0, "Bob stopped watching: not re-watched while the same stream runs");
  await A.page.eval(`import(new URL("js/ui/capture.js", location.href).href).then((m) => m.stopAll())`);
  await A.page.waitFor(`!__sss.S.captures.some((c) => c.state === 'live')`, { timeout: 15000, what: "Alice stopped" });
  await L.stubCapture(A.page, { audio: false, fps: 30 });
  await L.goLive(A.page, { codec: "h264", fps: 30 });
  for (const [n, P] of [["Zoe (switched on)", Z], ["Bob", B], ["Dora", D]]) {
    await P.page.waitFor(`__sss.S.rooms[0].watches.size === 1`, { timeout: 25000, what: n + " auto-watches the restarted share" });
    check(true, n + " auto-watches the restarted share");
  }

  // Row 181: late joiners with a screen share AND a webcam already running
  await A.page.waitFor("!document.querySelector('#btn-webcam').disabled", { what: "Alice's webcam button" });
  await A.page.click("#btn-webcam");
  await A.page.waitFor("__sss.S.captures.some((c) => c.kind === 'webcam' && c.state === 'live' && [...c.dests.values()].some((d) => d.state === 'live'))", { timeout: 20000, what: "Alice's webcam live" });
  await Z.page.waitFor(`__sss.S.rooms[0].streams.filter((s) => !s.self && s.kind === "video").length === 2`, { what: "both streams listed" });
  E = await L.openApp(site.url, { name: "Eve" });
  F = await L.openApp(site.url, { name: "Fay", settings: { autoWatch: false } });
  for (const P of [E, F]) {
    await L.addRoomByLink(P.page, link, "autowatch");
    await L.waitJoined(P.page);
  }
  await E.page.waitFor(`__sss.S.rooms[0].watches.size === 1`, { timeout: 25000, what: "Eve auto-watches the webcam" });
  await L.sleep(3000);
  check((await E.page.eval(`[...__sss.S.rooms[0].watches.values()].map((w) => w.label).join()`)) === "Webcam" && (await listed(E)) === 2, "joining with a screen share and a webcam running: Eve watches the webcam, only lists the screen");
  check((await watching(F)) === 0 && (await listed(F)) === 2, "option off: Fay joining late watches nothing");
  await E.page.screenshot(OUT + "autowatch_join_webcam.png");
} catch (e) {
  failed++;
  console.log("FAIL", e.message);
  await A.page.screenshot(OUT + "autowatch_fail.png").catch(() => {});
} finally {
  await A.close();
  await B.close();
  await Z.close().catch(() => {});
  if (D) await D.close().catch(() => {});
  if (E) await E.close().catch(() => {});
  if (F) await F.close().catch(() => {});
  site.close();
}
console.log(failed ? `${failed} FAILED` : "ALL PASSED");
process.exit(failed ? 1 : 0);
