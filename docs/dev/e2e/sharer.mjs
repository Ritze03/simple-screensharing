// Browser sharer via the real UI. usage: node b-sharer.mjs <hash> <codec> <fps> [lingerSeconds] [headed]
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
import { mkdirSync, writeFileSync, existsSync, rmSync } from "node:fs";
import { spawnSync } from "node:child_process";
const [HASH, codec = "h264", fps = "60", linger = "300"] = process.argv.slice(2);
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
mkdirSync(OUT, { recursive: true });
const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Browser" });
try {
  const link = await L.createRoom(A.page, { name: "E2E", server: "127.0.0.1:14433", hash: HASH });
  await L.waitJoined(A.page);
  await L.stubCapture(A.page, { fps: Number(fps) });
  await L.goLive(A.page, { codec, fps: Number(fps) });
  writeFileSync(OUT + "sharer.link", link);
  console.log("LIVE", codec, fps, link);
  await A.page.screenshot(OUT + `sharer-${codec}-${fps}.png`);
  const t0 = Date.now();
  const timeline = [];
  let last = "";
  const poll = setInterval(async () => {
    try {
      const st = await A.page.eval(`(() => { const r = __sss.S.rooms[0]; const c = __sss.S.captures[0]; const d = c && [...c.dests.values()][0]; return r.state + "|" + r.attempt + "|" + (r.issue || "") + "|share:" + (d ? d.state : "none") + "|viewers:" + (d && d.share ? d.share.subscribers : "-"); })()`);
      if (st !== last) { last = st; timeline.push(`${((Date.now() - t0) / 1000).toFixed(2)}s ${st}`); }
    } catch {}
  }, 100);
  while (Date.now() - t0 < Number(linger) * 1000 && !existsSync(OUT + "sharer.stop")) {
    if (existsSync(OUT + "sharer.restart")) {
      rmSync(OUT + "sharer.restart");
      timeline.push(`${((Date.now() - t0) / 1000).toFixed(2)}s RELAY RESTART requested (graceful q, certificate kept)`);
      const r = spawnSync("bash", [W + "relay.sh", "restart", process.env.RELAY_BIN || OUT + "relay-bin", process.env.RELAY_DIR || OUT + "relay-run", "14433"], { encoding: "utf8" });
      timeline.push(`${((Date.now() - t0) / 1000).toFixed(2)}s relay.sh returned: ${r.stdout.trim()}`);
    }
    await L.sleep(250);
  }
  clearInterval(poll);
  console.log("TIMELINE\n" + timeline.join("\n"));
  const st = await A.page.eval(`(() => { const r = __sss.S.rooms[0]; const c = __sss.S.captures[0]; const d = c && [...c.dests.values()][0]; return d && d.stats; })()`);
  console.log("SHARER STATS", JSON.stringify(st));
  console.log("errors", A.consoleErrors, A.badRequests);
} catch (e) {
  console.log("FAIL", e.message);
} finally {
  await A.close();
  site.close();
}
process.exit(0);
