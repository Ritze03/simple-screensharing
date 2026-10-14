// Browser viewer via the real UI. usage: node b-viewer.mjs <hash> [headed]
// Creates room "E2E", writes the link to viewer.link, clicks Watch on the first stream that shows up, then serves commands:
//   touch viewer.measure[.<label>]  -> 8 s measurement appended to viewer.results (JSON lines)
//   touch viewer.shot.<name>        -> screenshot <name>.png
//   touch viewer.stop               -> exit
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
import { mkdirSync, writeFileSync, readFileSync, existsSync, readdirSync, rmSync, appendFileSync } from "node:fs";
const [HASH, headed] = process.argv.slice(2);
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
mkdirSync(OUT, { recursive: true });
const site = await L.serveSite(0);
const V = await L.openApp(site.url, { name: "BrowserViewer", headed: !!headed, flags: headed ? ["--enable-features=VaapiVideoDecoder,VaapiIgnoreDriverChecks,AcceleratedVideoDecodeLinuxGL", "--ignore-gpu-blocklist"] : [] });
for (const f of readdirSync(OUT).filter((x) => x.startsWith("viewer."))) rmSync(OUT + f);
process.on("SIGTERM", () => { writeFileSync(OUT + "viewer.stop", "1"); });
try {
  const link = await L.createRoom(V.page, { name: "E2E", server: "127.0.0.1:14433", hash: HASH });
  await L.waitJoined(V.page);
  writeFileSync(OUT + "viewer.link", link);
  console.log("ROOM", link);
  let watching = false;
  for (;;) {
    await L.sleep(500);
    watching = await V.page.eval("__sss.S.rooms.some((r) => r.watches.size)");
    if (!watching) {
      const has = await V.page.eval("[...document.querySelectorAll('button')].some((b) => /(^|\\n)Watch$/.test(b.innerText.trim()))");
      if (has) {
        await L.sleep(800);
        await L.watchFirst(V.page);
        watching = true;
        console.log("WATCHING");
      }
    }
    for (const f of readdirSync(OUT).filter((x) => x.startsWith("viewer."))) {
      if (f === "viewer.stop") throw new Error("stop");
      if (f.startsWith("viewer.measure")) {
        rmSync(OUT + f);
        const m = await L.measureWatch(V.page, 8000);
        m.label = f;
        appendFileSync(OUT + "viewer.results", JSON.stringify(m) + "\n");
        console.log("MEASURE", JSON.stringify(m));
      } else if (f.startsWith("viewer.shot.")) {
        rmSync(OUT + f);
        await V.page.screenshot(OUT + f.slice(12) + ".png");
      } else if (f.startsWith("viewer.js.")) {
        const src = readFileSync(OUT + f, "utf8");
        rmSync(OUT + f);
        let out;
        try { out = JSON.stringify(await V.page.eval(src)); } catch (e) { out = "ERR " + e.message; }
        writeFileSync(OUT + "out." + f.slice(10), out);
      }
    }
  }
} catch (e) {
  if (e.message !== "stop") console.log("FAIL", e.message);
} finally {
  console.log("errors", V.consoleErrors, V.badRequests);
  await V.close();
  site.close();
}
process.exit(0);
