// Task #84 through the REAL site UI: a relay address without a port is invalid input. The create/join dialog shows an
// inline hint and disables Create/Join until it is fixed; a saved room with a portless server shows a useful connect
// error. No relay needed (nothing here reaches the network). Headless Chromium, fresh profile.
//
//   usage: node docs/dev/e2e/server-port.mjs
//   env:   E2E_OUT = where the screenshots go (default /tmp/sss-e2e), SHOT_PREFIX (default "t84_")
import { mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
const PFX = process.env.SHOT_PREFIX || "t84_";
mkdirSync(OUT, { recursive: true });

let failed = 0;
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!cond) failed++;
};

const ID = "c35293cd888511563e94239abb299633";
const KEY = "jX26ZBd5v3CZnwcIBwrH22-JBT9GV9PjAYrxRzCvBM4";
const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Zed" });
const { page } = A;
const hint = () => page.eval(`(() => { const e = document.querySelector("#add-hint"); return e && !e.hidden ? e.textContent : null; })()`);
const disabled = (sel) => page.eval(`document.querySelector(${JSON.stringify(sel)}).disabled`);
try {
  // 1. create tab: `example.org` -> hint, Create disabled (even with a room name); `:4433` -> hint gone, enabled
  await page.click("#btn-create-join");
  await page.waitFor("document.querySelector('.seg')", { what: "add modal" });
  await page.click({ js: "document.querySelectorAll('.seg button')[1]" });
  await page.waitFor("document.querySelector('#add-server')", { what: "create tab" });
  await page.type("#add-cname", "Test");
  check((await hint()) === null && !(await disabled("#btn-create")), "empty server: no hint");
  await page.type("#add-server", "example.org");
  check((await hint()) === "Add the relay's port, e.g. example.org:4433", "portless server shows the hint", String(await hint()));
  check(await disabled("#btn-create"), "Create disabled while the port is missing");
  await page.screenshot(OUT + PFX + "browser_noport.png");
  await page.type("#add-server", "example.org:0");
  check((await hint()) && /1 to 65535/.test(await hint()), "port 0 gets the bad-port hint", String(await hint()));
  await page.type("#add-server", "example.org:4433");
  check((await hint()) === null && !(await disabled("#btn-create")), "with a port: hint gone, Create enabled");
  await page.screenshot(OUT + PFX + "browser_port.png");

  // 2. join tab: the server part of a pasted link gets the same check (once the first `|` is there)
  await page.click({ js: "document.querySelectorAll('.seg button')[0]" });
  await page.waitFor("document.querySelector('#add-link')", { what: "join tab" });
  await page.type("#add-link", "vogelnest.any64.de");
  check((await hint()) === null, "no hint before the first |");
  await page.type("#add-link", `vogelnest.any64.de|${ID}|${KEY}`);
  check((await hint()) !== null && (await disabled("#btn-join")), "join link without a port: hint, Join disabled");
  await page.screenshot(OUT + PFX + "browser_joinlink_noport.png");
  await page.type("#add-link", `vogelnest.any64.de:41000|${ID}|${KEY}`);
  check((await hint()) === null && !(await disabled("#btn-join")), "join link with a port: hint gone, Join enabled");

  // 3. a saved room with a portless server (made before this check existed): the connect error says what is wrong
  await page.addInitScript(`try { localStorage.setItem("sss.rooms.v1", JSON.stringify([{ id: "r1", name: "portless", link: "vogelnest.any64.de|${ID}|${KEY}", auto: true }])); } catch {}`);
  await page.goto(site.url);
  await page.waitFor("window.__sss && __sss.S.rooms[0] && __sss.S.rooms[0].error", { what: "connect error" });
  const err = await page.eval("__sss.S.rooms[0].error");
  check(err === "The server address needs a port, e.g. host:4433.", "saved portless room: connect error names the port", err);
  await page.screenshot(OUT + PFX + "browser_connect_error.png");
} catch (e) {
  console.log("FAIL exception: " + e.stack);
  failed++;
} finally {
  await A.close?.();
  site.close?.();
}
console.log(failed ? `${failed} FAILED` : "all passed");
process.exit(failed ? 1 : 0);
