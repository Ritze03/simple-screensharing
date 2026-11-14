// Task 197: "pick up" through the REAL site UI (twin of the native pick-up). Every connect starts out of the call: the room shows the people and who is sharing
// (idle tiles without Watch buttons), the bar has only the green Pick up, the red Disconnect and the Room menu, nothing is watched, nothing can be shared.
// Alice shares (synthetic capture, video only). Bob connects (not in the call), picks up (auto-watch on: watches the running stream), hangs up (confirm, stays
// connected, sharer still sees him), disconnects with the red button (no confirm), connects again by hand (starts out of the call again, D2).
// Zoe has "Automatically watch new streams" off: pick-up lists the stream with a Watch button and watches nothing. Finally Alice hangs up: her share stops.
// Sounds are proven with a spy on SoundOut.play (nothing reaches the speakers), no audio stream anywhere.
//
// With --relay-bin <bin> --relay-dir <dir> (absolute) the relay is restarted once: Zoe (in the call) comes back in it and resumes her watch, Bob (not in the call, connected
// by hand) comes back still out of it and watches nothing (D2: a reconnect is not a new connect).
//
//   usage: node docs/dev/e2e/pickup.mjs <relay host:port> <certhash> [--relay-bin <bin> --relay-dir <dir>]   (relay: docs/dev/relay.sh start ...)
//   env:   E2E_OUT = where the screenshots go (default ~/tmp/sss-e2e), SHOT_PREFIX (default "web-")
import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const opt = (n) => (process.argv.includes(n) ? process.argv[process.argv.indexOf(n) + 1] : null);
const RELAY_BIN = opt("--relay-bin");
const RELAY_DIR = opt("--relay-dir");
const [RELAY, HASH] = process.argv.slice(2).filter((a, i, all) => !a.startsWith("--") && !(i && all[i - 1].startsWith("--")));
if (!RELAY || !HASH) throw new Error("usage: pickup.mjs <relay host:port> <certhash>");
const OUT = (process.env.E2E_OUT || process.env.HOME + "/tmp/sss-e2e") + "/";
const PFX = process.env.SHOT_PREFIX || "web-";
mkdirSync(OUT, { recursive: true });
let failed = 0;
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!cond) failed++;
};
const q = (p, js) => p.page.eval(js);
const room = (p, expr) => q(p, `(() => { const r = __sss.S.rooms[0]; return ${expr}; })()`);
const has = (p, sel) => q(p, `!!document.querySelector(${JSON.stringify(sel)})`);
const count = (p, sel) => q(p, `document.querySelectorAll(${JSON.stringify(sel)}).length`);
const icon = (p, sel) => q(p, `(document.querySelector(${JSON.stringify(sel)}) || {}).innerText?.trim()`);
const strip = (p) => q(p, `[...document.querySelectorAll(".pv")].map((e) => e.dataset.user).join()`);
const spy = (p) => q(p, `(() => { window.__played = []; __sss.SoundOut.play = async (n) => { window.__played.push(n); }; })()`);
const played = (p) => q(p, `window.__played.join()`);

const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Alice" });
const B = await L.openApp(site.url, { name: "Bob" });
const Z = await L.openApp(site.url, { name: "Zoe", settings: { autoWatch: false } });
try {
  await L.createRoom(A.page, { name: "pickup", server: RELAY, hash: HASH });
  await L.waitJoined(A.page); // picks up
  const link = await L.inviteLink(A.page);
  await L.stubCapture(A.page, { audio: false, fps: 30 });
  await L.goLive(A.page, { codec: "h264", fps: 30 });
  check(await room(A, `r.inCall`), "Alice picked up (her own pick-up works)");

  // ---- Bob connects: not in the call ----
  await spy(B);
  await L.addRoomByLink(B.page, link, "pickup");
  await L.waitJoined(B.page, 0, 20000, { pickUp: false });
  await B.page.waitFor(`__sss.S.rooms[0].streams.some((s) => !s.self && s.kind === "video")`, { what: "Bob lists Alice's stream" });
  await B.page.waitFor(`!!document.querySelector(".st-tile.idle")`, { what: "idle tile" });
  await L.sleep(1500);
  check(!(await room(B, `r.inCall`)), "a fresh connect starts out of the call");
  check((await strip(B)).includes("Alice"), "people strip shows the sharer", await strip(B));
  check((await count(B, ".st-tile.idle")) === 1 && !(await has(B, "[data-watch]")) && !(await has(B, "[data-listen]")), "the stream is an idle tile without a Watch button");
  check(!(await has(B, "#btn-share")) && !(await has(B, "#btn-webcam")) && !(await has(B, "#btn-audio")), "no share, webcam or audio button");
  check((await has(B, "#btn-pickup")) && (await icon(B, "#btn-pickup")) === "call", "green pick-up button (call icon)");
  check((await has(B, "#btn-leave")) && (await icon(B, "#btn-leave")) === "logout", "red button is the Disconnect one (logout icon)");
  check((await has(B, "#btn-room")) && !(await has(B, "#btn-layout")) && !(await has(B, "#btn-people")), "Room menu there, layout and people buttons not");
  check((await room(B, `r.watches.size`)) === 0, "nothing watched, nothing decoding");
  check((await room(B, `r.state`)) === "joined" && (await strip(A)).includes("Bob"), "connected: Alice sees Bob in her strip", await strip(A));
  // a hidden people strip is shown anyway while not in the call (the setting is kept)
  await q(B, `(() => { __sss.S.stage.stripHidden = true; })()`);
  await q(B, `import("${L.BASE}js/ui/store.js").then((m) => m.invalidate())`);
  await L.sleep(300);
  check((await count(B, ".pv")) >= 2, "people strip shown although hidden in the settings");
  await q(B, `(() => { __sss.S.stage.stripHidden = false; })()`);
  await B.page.screenshot(OUT + PFX + "not-in-call.png");

  // ---- pick up: watches the running stream, no sound burst ----
  await B.page.click("#btn-pickup");
  await B.page.waitFor(`__sss.S.rooms[0].inCall && __sss.S.rooms[0].watches.size === 1 && [...__sss.S.rooms[0].watches.values()].every((w) => w.firstFrame)`, { timeout: 25000, what: "Bob watches after pick-up" });
  check(true, "pick-up with auto-watch on: the running stream is watched and frames arrive");
  const st = (await L.watchStats(B.page))[0];
  check(st && st.st && st.st.decoded > 0, "frames decode", JSON.stringify(st && st.st && { decoded: st.st.decoded }));
  check((await played(B)) === "", "no notification sound at pick-up", await played(B));
  check(!(await has(B, "#btn-pickup")) && (await icon(B, "#btn-leave")) === "call_end" && (await has(B, "#btn-share")), "in the call: red hang-up button, share button back, no pick-up");
  check((await strip(A)).includes("Bob"), "Alice still sees Bob");
  await B.page.screenshot(OUT + PFX + "in-call.png");

  // ---- hang up: confirm, stays connected ----
  await B.page.click("#btn-leave");
  await B.page.waitFor(`!!document.querySelector("#btn-confirm")`, { what: "hang-up confirm" });
  await B.page.screenshot(OUT + PFX + "hangup-confirm.png");
  await B.page.click("#btn-confirm");
  await B.page.waitFor(`!__sss.S.rooms[0].inCall`, { what: "hung up" });
  await L.sleep(500);
  check((await room(B, `r.state`)) === "joined" && (await room(B, `r.watches.size`)) === 0, "hang up: still connected, nothing watched");
  check((await has(B, "#btn-pickup")) && (await count(B, ".st-tile.idle")) === 1 && !(await has(B, "[data-watch]")), "back to look-only: pick-up button, idle tile, no Watch button");
  check((await strip(A)).includes("Bob"), "Alice still sees Bob after he hung up", await strip(A));
  // picking up again watches again (a hang-up leaves no stale "resume")
  await B.page.click("#btn-pickup");
  await B.page.waitFor(`__sss.S.rooms[0].inCall && __sss.S.rooms[0].watches.size === 1`, { timeout: 25000, what: "re-pick-up watches" });
  check(true, "picking up again watches again");
  await B.page.click("#btn-leave");
  await B.page.click("#btn-confirm");
  await B.page.waitFor(`!__sss.S.rooms[0].inCall`, { what: "hung up 2" });

  // ---- red again (not in the call): Disconnect, no confirm ----
  await B.page.click("#btn-leave");
  await B.page.waitFor(`__sss.S.rooms[0].state === "disconnected"`, { what: "disconnected" });
  check(!(await has(B, "#btn-confirm")), "Disconnect asks nothing");
  await A.page.waitFor(`![...document.querySelectorAll(".pv")].some((e) => e.dataset.user === "Bob")`, { timeout: 15000, what: "Bob gone from Alice's strip" });
  check(true, "Bob disconnected: Alice's strip no longer lists him");

  // ---- D2: a manual Connect starts out of the call too ----
  await B.page.click("#btn-connect");
  await L.waitJoined(B.page, 0, 20000, { pickUp: false });
  await L.sleep(800);
  check(!(await room(B, `r.inCall`)) && (await room(B, `r.watches.size`)) === 0 && (await has(B, "#btn-pickup")), "manual Connect: out of the call, nothing watched");

  // ---- option off: pick-up lists, watches nothing ----
  await L.addRoomByLink(Z.page, link, "pickup");
  await L.waitJoined(Z.page, 0, 20000, { pickUp: false });
  await Z.page.waitFor(`!!document.querySelector(".st-tile.idle")`, { what: "Zoe idle tile" });
  await L.pickUpRoom(Z.page);
  await L.sleep(3000);
  check((await room(Z, `r.inCall && r.watches.size === 0`)) && (await has(Z, "[data-watch]")), "option off: pick-up lists the stream with a Watch button, watches nothing");
  await Z.page.click("[data-watch]");
  await Z.page.waitFor(`__sss.S.rooms[0].watches.size === 1`, { what: "Zoe watches by button" });
  check(true, "and the Watch button then watches");

  // ---- D2: a reconnect keeps the call state (relay restart) ----
  if (RELAY_BIN && RELAY_DIR) {
    spawnSync("bash", [W + "relay.sh", "restart", RELAY_BIN, RELAY_DIR, RELAY.split(":").pop()], { stdio: "inherit" });
    for (const P of [A, B, Z]) await P.page.waitFor(`__sss.S.rooms[0].state === "joined"`, { timeout: 60000, what: "back after the restart" });
    await Z.page.waitFor(`__sss.S.rooms[0].watches.size === 1 && [...__sss.S.rooms[0].watches.values()].every((w) => w.firstFrame)`, { timeout: 60000, what: "Zoe resumes her watch" });
    await L.sleep(2500);
    check((await room(Z, `r.inCall`)) && (await room(Z, `r.watches.size`)) === 1, "reconnect in the call: still in the call, the watch resumed");
    check(!(await room(B, `r.inCall`)) && (await room(B, `r.watches.size`)) === 0 && (await has(B, "#btn-pickup")), "reconnect out of the call: still out of it, nothing watched");
    check(await room(A, `r.inCall`), "Alice (in the call) is still in it after the reconnect");
  } else console.log("(no --relay-bin/--relay-dir: the relay-restart check is skipped)");

  // ---- Alice hangs up: her share into the room stops, she stays connected ----
  await A.page.click("#btn-leave");
  await A.page.waitFor(`!!document.querySelector("#btn-confirm")`, { what: "Alice's hang-up confirm" });
  await A.page.click("#btn-confirm");
  await A.page.waitFor(`!__sss.S.rooms[0].inCall && __sss.S.captures.length === 0`, { timeout: 15000, what: "Alice's share stopped" });
  check((await room(A, `r.state`)) === "joined", "Alice hung up: her share stopped, room still connected");
  await Z.page.waitFor(`!__sss.S.rooms[0].streams.some((s) => !s.self && s.kind === "video")`, { timeout: 15000, what: "stream gone for Zoe" });
  check(true, "Zoe sees the stream end");
} catch (e) {
  failed++;
  console.log("FAIL", e.message);
  await A.page.screenshot(OUT + PFX + "fail-alice.png").catch(() => {});
  await B.page.screenshot(OUT + PFX + "fail-bob.png").catch(() => {});
} finally {
  await A.close();
  await B.close();
  await Z.close().catch(() => {});
  site.close();
}
console.log(failed ? `${failed} FAILED` : "ALL PASSED");
process.exit(failed ? 1 : 0);
