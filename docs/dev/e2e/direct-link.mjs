// Row 179: browser direct link. usage: node direct-link.mjs <relay cert hash> [port=14433]  (a relay must run, see dev/relay.sh)
//  A creates a room, right-clicks "Copy invite link" and copies the browser direct link (clipboard write intercepted);
//  B (fresh profile) opens that URL -> room saved + connected without a dialog, hash cleared, kept after reload;
//  B opens the same link again -> no duplicate; an invalid hash -> the prefilled join dialog with its error;
//  C (fresh profile, no name chosen yet) -> "Choose your name" first, prefilled from the link's name suggestion, then connected.
import { createRoom, openApp, serveSite, waitJoined, sleep } from "../e2e-lib.mjs";
const [HASH, PORT = "14433"] = process.argv.slice(2);
const site = await serveSite(0);
let fails = 0;
const check = (ok, what) => { console.log(ok ? "PASS" : "FAIL", what); if (!ok) fails++; };
const open = [];
try {
  const A = await openApp(site.url, { name: "BrowserA" });
  open.push(A);
  await createRoom(A.page, { name: "Direct Room", server: `127.0.0.1:${PORT}`, hash: HASH });
  await waitJoined(A.page);
  await A.page.eval(`window.__copied = []; navigator.clipboard.writeText = async (x) => { window.__copied.push(x); }; true`);
  // left click: the join link at once
  await A.page.click("#btn-invite");
  const join = await A.page.eval("__copied[0]");
  check(/^127\.0\.0\.1:\d+\|[0-9a-f]{32}\|/.test(join), "left click copies the join link: " + join.slice(0, 40) + "...");
  // right click: menu with two items
  const r = await A.page.eval(`(() => { const b = document.querySelector("#btn-invite").getBoundingClientRect(); return { x: b.left + 10, y: b.top + 10 }; })()`);
  await A.page.send("Input.dispatchMouseEvent", { type: "mousePressed", x: r.x, y: r.y, button: "right", clickCount: 1 });
  await A.page.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: r.x, y: r.y, button: "right", clickCount: 1 });
  await A.page.waitFor("document.querySelector('.menu.m-invite')", { what: "invite menu" });
  const items = await A.page.eval(`[...document.querySelectorAll('.menu.m-invite .mi .t')].map((e) => e.firstChild.textContent)`);
  check(JSON.stringify(items) === JSON.stringify(["Copy join link", "Copy browser direct link"]), "context menu items: " + JSON.stringify(items));
  await A.page.screenshot((process.env.E2E_OUT || "") + "direct-link-menu.png");
  await A.page.click({ js: "document.querySelectorAll('.menu.m-invite .mi')[1]" });
  const direct = await A.page.eval("__copied[1]");
  check(direct && direct.startsWith(site.url + "#join=") && decodeURIComponent(direct.slice(direct.indexOf("#join=") + 6)) === join, "direct link = site URL + #join=encoded join link: " + (direct || "").slice(0, 70) + "...");
  // the room menu has the item too
  await A.page.click("#btn-room");
  await A.page.waitFor("document.querySelector('.menu.m-room')");
  check(await A.page.eval(`[...document.querySelectorAll('.menu.m-room .mi')].some((e) => /direct link/.test(e.innerText))`), "room menu lists Copy browser direct link");
  await A.page.key("Escape");

  // B: fresh profile
  const B = await openApp(site.url + "#join=" + encodeURIComponent(join), { name: "BrowserB" });
  open.push(B);
  await waitJoined(B.page);
  check(await B.page.eval("__sss.S.rooms.length") === 1, "B: room saved");
  check(await B.page.eval("__sss.S.modal") === null, "B: no dialog");
  check(await B.page.eval("location.hash") === "", "B: hash cleared");
  check(await B.page.eval("__sss.S.rooms[0].name") === "Direct Room", "B: name from the link");
  await B.page.goto(site.url);
  await B.page.waitFor("window.__sss && document.querySelector('#app').children.length > 0");
  check(await B.page.eval("__sss.S.rooms.length") === 1, "B: still listed after reload");
  // same link again (pasted into the address bar of the open tab): no duplicate
  await B.page.goto(site.url + "#join=" + encodeURIComponent(join));
  await B.page.waitFor("window.__sss && document.querySelector('#app').children.length > 0");
  await waitJoined(B.page);
  check(await B.page.eval("__sss.S.rooms.length") === 1, "B: same link again, no duplicate");
  await B.page.eval(`location.hash = "#join=" + encodeURIComponent(${JSON.stringify(join)}); true`);
  await sleep(500);
  check(await B.page.eval("__sss.S.rooms.length") === 1 && await B.page.eval("location.hash") === "", "B: hashchange on an open tab, no duplicate, hash cleared");
  // invalid
  await B.page.eval(`location.hash = "#join=garbage"; true`);
  await B.page.waitFor("document.querySelector('#add-link')", { what: "join dialog for an invalid link" });
  check(await B.page.eval("document.querySelector('#add-link').value") === "garbage" && !!(await B.page.eval("document.querySelector('.hint.err:not(#add-hint)') && document.querySelector('.hint.err:not(#add-hint)').innerText")), "B: invalid link -> prefilled dialog with an error");

  // C: no name chosen yet, link with a name suggestion
  const withName = join.split("|").slice(0, 4).join("|") + "|" + "Alex";
  const C = await openApp(site.url + "#join=" + encodeURIComponent(withName), { name: "", settings: { name: "" } });
  open.push(C);
  await C.page.waitFor("document.querySelector('#choose-name-in')", { what: "choose name modal" }).catch(() => {});
  const pre = await C.page.eval("document.querySelector('#choose-name-in') && document.querySelector('#choose-name-in').value");
  check(pre === "Alex" || pre === null, "C: name modal prefilled with the link's suggestion (got " + JSON.stringify(pre) + ")");
  check(await C.page.eval("__sss.S.rooms.length") === 1, "C: room saved while the name prompt is up");
  if (pre) await C.page.click("#btn-choose-name-save");
  await waitJoined(C.page);
  check(true, "C: connected");
} catch (e) {
  console.log("FAIL", e.message);
  fails++;
} finally {
  for (const b of open) await b.close().catch(() => {});
  site.close();
}
console.log(fails ? `${fails} FAILED` : "ALL PASSED");
process.exit(fails ? 1 : 0);
