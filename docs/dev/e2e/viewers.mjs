// Task #116 through the REAL site UI: the sharer's own card shows an eye chip with the number of viewers, and hovering it
// opens a dropdown listing each viewer (avatar + name). Three browsers: Alice shares (synthetic capture), Bob and Zoë watch.
// The relay must be a #116 one (viewer lists: protocol-spec.md 9.1); against an older relay the chip must NOT be drawn
// (second half: pass --old to assert that instead).
//
//   usage: node docs/dev/e2e/viewers.mjs <relay host:port> <certhash> [--old]   (relay: docs/dev/relay.sh start ..., or the rig's)
//   env:   E2E_OUT = where the screenshots go (default /tmp/sss-e2e), SHOT_PREFIX (default "t116_browser_")
import { mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const [RELAY, HASH] = process.argv.slice(2).filter((a) => !a.startsWith("--"));
const OLD = process.argv.includes("--old");
if (!RELAY || !HASH) throw new Error("usage: viewers.mjs <relay host:port> <certhash> [--old]");
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
const PFX = process.env.SHOT_PREFIX || "t116_browser_";
mkdirSync(OUT, { recursive: true });

let failed = 0;
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!cond) failed++;
};

const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Alice" });
const B = await L.openApp(site.url, { name: "Bob" });
const C = await L.openApp(site.url, { name: "Zoë Bauer" });
const chip = () => A.page.eval(`(() => { const c = document.querySelector(".chip-dark.viewers"); return c ? { text: c.innerText.trim(), title: c.title } : null; })()`);
const pop = () => A.page.eval(`(() => { const p = document.querySelector(".vpop"); return p ? { rows: [...p.querySelectorAll(".vrow")].map((r) => r.innerText.trim().replace(/\\s+/g, " ")), head: p.querySelector(".ph").innerText.trim(), avatars: p.querySelectorAll(".avatar").length } : null; })()`);
const hover = async (x, y) => A.page.send("Input.dispatchMouseEvent", { type: "mouseMoved", x, y });
const chipCenter = () => A.page.eval(`(() => { const r = document.querySelector(".chip-dark.viewers").getBoundingClientRect(); return { x: r.left + r.width / 2, y: r.top + r.height / 2 }; })()`);
try {
  await L.createRoom(A.page, { name: "viewers", server: RELAY, hash: HASH });
  await L.waitJoined(A.page);
  await L.stubCapture(A.page, { audio: false, fps: 30 });
  await L.goLive(A.page, { codec: "h264", fps: 30 });
  const link = await L.inviteLink(A.page);
  for (const P of [B, C]) {
    await L.addRoomByLink(P.page, link, "viewers");
    await L.waitJoined(P.page);
  }
  await L.sleep(2500); // hello / ack round trip
  if (OLD) {
    check((await chip()) === null, "older relay: no viewer chip at all (a 0 would be a lie)");
  } else {
    let c = await chip();
    check(c && c.text.endsWith("0"), "supported relay, nobody watching: chip shows 0", JSON.stringify(c));
    await A.page.screenshot(OUT + PFX + "zero.png");

    await L.watchFirst(B.page);
    await A.page.waitFor(`document.querySelector(".chip-dark.viewers")?.innerText.trim().split(/\\s+/).pop() === "1"`, { what: "chip 1" });
    await L.watchFirst(C.page);
    await A.page.waitFor(`document.querySelector(".chip-dark.viewers")?.innerText.trim().split(/\\s+/).pop() === "2"`, { what: "chip 2" });
    check(true, "two viewers: chip shows 2");

    // hover the chip -> dropdown with both names, avatars, sorted
    let p = chipCenter ? await chipCenter() : null;
    await hover(p.x, p.y);
    await A.page.waitFor(`document.querySelector(".vpop")`, { what: "dropdown opens on hover" });
    let d = await pop();
    check(d && d.rows.length === 2 && d.rows[0].endsWith("Bob") && d.rows[1].endsWith("Zoë Bauer") && d.avatars === 2, "dropdown lists Bob and Zoë Bauer with avatars", JSON.stringify(d));
    check(d && /2$/.test(d.head), "heading carries the count", d && d.head);
    await A.page.screenshot(OUT + PFX + "hover_two.png");

    // pointer travels into the dropdown: it stays open
    const box = await A.page.eval(`(() => { const r = document.querySelector(".vpop").getBoundingClientRect(); return { x: r.left + 40, y: r.top + r.height - 12 }; })()`);
    await hover(p.x, p.y + 12);
    await hover(box.x, box.y);
    await L.sleep(400);
    check(!!(await pop()), "pointer inside the dropdown: still open");

    // a viewer stops watching while the dropdown is open: the list follows live
    await B.page.eval(`(() => { const r = __sss.S.rooms[0]; r.client.unwatch([...r.watches.values()][0].key); })()`);
    await A.page.waitFor(`document.querySelector(".vpop") && document.querySelectorAll(".vpop .vrow").length === 1`, { what: "list shrinks to 1 while open" });
    d = await pop();
    check(d && d.rows.length === 1 && d.rows[0].endsWith("Zoë Bauer"), "Bob stopped watching: only Zoë Bauer remains", JSON.stringify(d));
    await A.page.screenshot(OUT + PFX + "hover_one.png");

    // pointer leaves: it closes
    await hover(5, 5);
    await L.sleep(400);
    check(!(await pop()), "pointer away: dropdown closed");

    // last viewer leaves the room entirely (graceful disconnect): count back to 0, no dropdown
    await C.page.eval(`__sss.S.rooms[0].client.disconnect()`);
    await A.page.waitFor(`document.querySelector(".chip-dark.viewers")?.innerText.trim().split(/\\s+/).pop() === "0"`, { timeout: 15000, what: "chip back to 0" });
    check(true, "viewer disconnected: chip back to 0");
    p = await chipCenter();
    await hover(p.x, p.y);
    await L.sleep(400);
    check(!(await pop()), "0 viewers: no dropdown");
    await A.page.screenshot(OUT + PFX + "zero_again.png");
  }
  check(A.consoleErrors.length === 0, "no console errors on the sharer", A.consoleErrors.join(" | "));
} catch (e) {
  failed++;
  console.log("FAIL", e.message);
  await A.page.screenshot(OUT + PFX + "fail.png").catch(() => {});
} finally {
  await A.close();
  await B.close();
  await C.close().catch(() => {});
  site.close();
}
console.log(failed ? `${failed} FAILED` : "ALL PASSED");
process.exit(failed ? 1 : 0);
