// Scenario 6 (browser side). usage: node t-links.mjs <hash> <phase>
//  phase A: browser creates the room (with cert hash) + shares; waits for the native to join it (links.A written)
//  phase B: browser joins the two links the native wrote (links.native1 = minted by Create, links.native2 = "Copy invite link")
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
import { mkdirSync, writeFileSync, existsSync, readFileSync } from "node:fs";
const [HASH, phase] = process.argv.slice(2);
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
mkdirSync(OUT, { recursive: true });
const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "BrowserA" });
try {
  if (phase === "A") {
    const link = await L.createRoom(A.page, { name: "FromBrowser", server: "127.0.0.1:14433", hash: HASH });
    await L.waitJoined(A.page);
    await L.stubCapture(A.page, { fps: 30 });
    await L.goLive(A.page, { codec: "h264", fps: 30 });
    writeFileSync(OUT + "links.A", link);
    console.log("A link", link);
    const t0 = Date.now();
    while (!existsSync(OUT + "links.stop") && Date.now() - t0 < 240000) {
      const names = await A.page.eval("__sss.S.rooms[0].members.map((m) => m.name)");
      const viewers = await A.page.eval("(() => { const c = __sss.S.captures[0]; const d = c && [...c.dests.values()][0]; return d && d.share ? d.share.subscribers : -1; })()");
      writeFileSync(OUT + "links.A.members", JSON.stringify({ names, viewers }));
      await L.sleep(1000);
    }
  } else {
    while (!existsSync(OUT + "links.native1")) await L.sleep(500);
    const l1 = readFileSync(OUT + "links.native1", "utf8").trim();
    console.log("native minted link   :", l1);
    await L.addRoomByLink(A.page, l1, "NativeMinted");
    await A.page.waitFor("__sss.S.rooms[0] && __sss.S.rooms[0].state !== 'connecting' && (__sss.S.rooms[0].state === 'joined' || __sss.S.rooms[0].error)", { timeout: 25000, what: "result of joining the minted link" });
    console.log("  -> state:", await A.page.eval("__sss.S.rooms[0].state"), "| error:", await A.page.eval("__sss.S.rooms[0].error"));
    await A.page.screenshot(OUT + "s6-browser-minted-link.png");
    while (!existsSync(OUT + "links.native2")) await L.sleep(500);
    const l2 = readFileSync(OUT + "links.native2", "utf8").trim();
    console.log("native 'Copy invite link':", l2);
    const B = await L.openApp(site.url, { name: "BrowserB" });
    try {
      await L.addRoomByLink(B.page, l2, "NativeInvite");
      await L.waitJoined(B.page, 0, 25000);
      await L.sleep(1500);
      console.log("  -> joined; members:", JSON.stringify(await B.page.eval("__sss.S.rooms[0].members.map((m) => m.name)")), "streams:", JSON.stringify(await B.page.eval("__sss.S.rooms[0].streams.map((s) => s.ownerName + ':' + s.kind)")));
      await B.page.screenshot(OUT + "s6-browser-invite-link.png");
      writeFileSync(OUT + "links.done", "1");
    } finally {
      await B.close();
    }
  }
  console.log("errors", A.consoleErrors, A.badRequests);
} catch (e) {
  console.log("FAIL", e.message);
  await A.page.screenshot(OUT + "s6-fail.png");
} finally {
  await A.close();
  site.close();
}
process.exit(0);
