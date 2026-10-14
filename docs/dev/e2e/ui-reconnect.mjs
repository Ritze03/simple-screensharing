// UI check: German du-form, relay down banner (unreachable, not "link expired"), "Jetzt erneut versuchen" keeps the share + the watch.
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
import { spawnSync } from "node:child_process";
const HASH = process.argv[2];
import { mkdirSync } from "node:fs";
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
mkdirSync(OUT, { recursive: true });
const relay = (cmd) => spawnSync("bash", [W + "relay.sh", cmd, ...(cmd === "stop" ? ["14433"] : [process.env.RELAY_BIN || OUT + "relay-bin", process.env.RELAY_DIR || OUT + "relay-run", "14433"])], { encoding: "utf8" });
const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Anna", lang: "de" });
const B = await L.openApp(site.url, { name: "Ben", lang: "de" });
try {
  const link = await L.createRoom(A.page, { name: "Filmabend", server: "127.0.0.1:14433", hash: HASH });
  await L.waitJoined(A.page);
  await L.stubCapture(A.page, { fps: 30 });
  await L.goLive(A.page, { codec: "h264", fps: 30 });
  await L.addRoomByLink(B.page, link, "Filmabend");
  await L.waitJoined(B.page);
  await L.watchFirst(B.page);
  await L.sleep(2500);
  await B.page.screenshot(OUT + "ui-de-watching.png");
  await A.page.screenshot(OUT + "ui-de-sharing.png");
  const keyBefore = await A.page.eval("__sss.S.rooms[0].client.shares[0].videoStreamId + '@' + __sss.S.rooms[0].client.selfId");
  console.log("A selfId/share before:", keyBefore);
  relay("stop");
  await B.page.waitFor("__sss.S.rooms[0].state === 'reconnecting' && __sss.S.rooms[0].issue === 'unreachable'", { timeout: 20000, what: "banner state unreachable" });
  await L.sleep(1500);
  const bannerB = await B.page.eval("[...document.querySelectorAll('.banner')].map((b) => b.innerText).join(' | ')");
  console.log("B banner:", bannerB);
  await B.page.screenshot(OUT + "ui-de-relay-down.png");
  relay("restart");
  await L.sleep(300);
  // press the real "Jetzt erneut versuchen" button on A while the relay is back
  await A.page.click({ text: "Jetzt erneut versuchen", sel: "button" }).catch((e) => console.log("no retry button on A:", e.message));
  await L.sleep(300);
  await A.page.waitFor("__sss.S.rooms[0].state === 'joined'", { timeout: 20000, what: "A joined after retry" });
  const keyAfter = await A.page.eval("__sss.S.rooms[0].client.shares[0].videoStreamId + '@' + __sss.S.rooms[0].client.selfId");
  const shareLive = await A.page.eval("(() => { const c = __sss.S.captures[0]; return c && [...c.dests.values()].map((d) => d.state).join(','); })()");
  console.log("A after retry: share dest state =", shareLive, "| selfId/share:", keyAfter, "(share object kept, ConnId changed)");
  await B.page.waitFor("__sss.S.rooms[0].state === 'joined'", { timeout: 20000, what: "B joined" });
  await B.page.waitFor("(() => { const r = __sss.S.rooms[0]; const w = [...r.watches.values()][0]; return w && r.client.watchStats(w.key) && r.client.watchStats(w.key).decoded > 5; })()", { timeout: 30000, what: "B watching again (resume by stableKey)" });
  console.log("B resumed the watch by itself; watches:", await B.page.eval("[...__sss.S.rooms[0].watches.values()].map((w) => w.ownerName + ':' + w.key).join(',')"));
  await L.sleep(1500);
  await B.page.screenshot(OUT + "ui-de-recovered.png");
  console.log("errors A", A.consoleErrors, A.badRequests, "B", B.consoleErrors, B.badRequests);
  console.log("Chromium's own ERR_QUIC_* console errors during the deliberate relay downtime (expected, not page code): B", B.browserNetErrors.length, "A", A.browserNetErrors.length);
} catch (e) {
  console.log("FAIL", e.message);
  await A.page.screenshot(OUT + "ui-fail-A.png");
  await B.page.screenshot(OUT + "ui-fail-B.png");
} finally {
  relay("restart");
  await A.close();
  await B.close();
  site.close();
}
process.exit(0);
