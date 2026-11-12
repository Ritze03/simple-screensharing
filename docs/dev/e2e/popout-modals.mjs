// Row 184: modals, menus and toasts raised from the popped-out stage window open IN that window, not in the main page.
// usage: node popout-modals.mjs <relay cert hash> [port=14433]   (a relay must run: dev/relay.sh; E2E_OUT = screenshot dir)
// One sharing page (stubbed screen, video only, no audio). The stage pops out (Document PiP; its target gets its own CDP session and real
// mouse/key input), then End call, the Layout / Room settings / invite menus, the Share modal and a "copied" toast are driven INSIDE the pop-out.
import { createRoom, openApp, serveSite, waitJoined, stubCapture, goLiveAuto, sleep } from "../e2e-lib.mjs";
import { Page } from "../cdp.mjs";
const [HASH, PORT = "14433"] = process.argv.slice(2);
const OUT = (process.env.E2E_OUT || ".tl-scratch") + "/";
const site = await serveSite(0);
let fails = 0;
const check = (ok, what, extra = "") => { console.log(ok ? "PASS" : "FAIL", what, extra); if (!ok) fails++; };
let A;
try {
  A = await openApp(site.url, { name: "Alice" });
  const P = A.page;
  await createRoom(P, { name: "Pop Room", server: `127.0.0.1:${PORT}`, hash: HASH });
  await waitJoined(P);
  await stubCapture(P);
  await goLiveAuto(P);
  await P.eval(`window.__copied = []; navigator.clipboard.writeText = async (x) => { window.__copied.push(x); }; true`);
  if (!(await P.eval("!!window.documentPictureInPicture"))) throw new Error("no Document Picture-in-Picture in this Chromium");
  for (let i = 0; i < 4 && !(await P.eval("!!__sss.stageWindow()")); i++) { await P.click("#btn-popstage"); await sleep(600); }
  await P.waitFor("!!__sss.stageWindow()", { what: "stage window" });
  await sleep(500);
  // the pop-out's own CDP target (headless opens it at ~450x270: size it like stage.mjs does)
  const mainId = (await A.conn.send("Target.getTargetInfo", {}, P.sid)).targetInfo.targetId;
  const { targetInfos } = await A.conn.send("Target.getTargets");
  const pipT = targetInfos.find((t) => t.type === "page" && t.targetId !== mainId);
  const sid = (await A.conn.send("Target.attachToTarget", { targetId: pipT.targetId, flatten: true })).sessionId;
  await A.conn.send("Emulation.setDeviceMetricsOverride", { width: 1100, height: 720, deviceScaleFactor: 1, mobile: false }, sid);
  const Q = new Page(A.conn, sid);
  for (const d of ["Page", "Runtime"]) await Q.send(`${d}.enable`);
  await sleep(800);
  // counts in both documents
  const where = (sel) => P.eval(`({ main: document.querySelectorAll(${JSON.stringify(sel)}).length, pop: __sss.stageWindow() ? __sss.stageWindow().win.document.querySelectorAll(${JSON.stringify(sel)}).length : -1 })`);
  const shot = async (n) => { await P.screenshot(OUT + "popmodal-" + n + "-main.png"); await Q.screenshot(OUT + "popmodal-" + n + "-pop.png"); };

  // 1) End call in the pop-out
  await Q.click("#btn-leave");
  await P.waitFor("__sss.S.modal && __sss.S.modal.kind === 'leave'", { what: "leave modal" });
  await sleep(300);
  let w = await where(".scrim .modal");
  check(w.pop === 1 && w.main === 0, "End call pressed in the pop-out: the confirmation is in the pop-out, not in the main page", JSON.stringify(w));
  await shot("1-leave");
  await Q.key("Escape");
  await sleep(300);
  check((await P.eval("__sss.S.modal")) === null, "Escape pressed in the pop-out closes it");
  // 2) menus
  await Q.click("#btn-layout");
  await sleep(300);
  w = await where(".menu.m-layout");
  check(w.pop === 1 && w.main === 0, "Layout menu opens in the pop-out", JSON.stringify(w));
  await shot("2-layout");
  await Q.key("Escape");
  await sleep(200);
  await Q.click("#btn-room");
  await sleep(300);
  w = await where(".menu.m-room");
  check(w.pop === 1 && w.main === 0, "Room settings menu opens in the pop-out", JSON.stringify(w));
  await shot("3-room");
  // a menu item that opens a modal: Rename room -> modal in the pop-out with its field focused
  await Q.click({ js: "[...document.querySelectorAll('.menu.m-room .mi')].find((e) => /^edit/.test(e.innerText.trim()))" });
  await sleep(400);
  w = await where(".scrim .modal");
  check(w.pop === 1 && w.main === 0, "Rename (from the Room settings menu in the pop-out) opens in the pop-out", JSON.stringify(w));
  await Q.type("#" + (await Q.eval("document.querySelector('.modal input.field').id || (document.querySelector('.modal input.field').id='rn', 'rn')")), "X");
  await sleep(200);
  check(await P.eval(`__sss.stageWindow().win.document.activeElement.matches('.modal input.field')`), "typing keeps the field focused across renders");
  await shot("4-rename");
  await Q.key("Escape");
  await sleep(200);
  // 3) toast from a copy action in the pop-out
  await Q.click("#btn-invite");
  await sleep(500);
  w = await where(".toast");
  check(w.pop === 1 && w.main === 0, "'Copied' toast raised from the pop-out shows in the pop-out", JSON.stringify(w));
  await shot("5-toast");
  // 4) invite right-click menu
  const r = await Q.eval(`(() => { const b = document.querySelector('#btn-invite').getBoundingClientRect(); return { x: b.left + 10, y: b.top + 10 }; })()`);
  await Q.send("Input.dispatchMouseEvent", { type: "mousePressed", x: r.x, y: r.y, button: "right", clickCount: 1 });
  await Q.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: r.x, y: r.y, button: "right", clickCount: 1 });
  await sleep(300);
  w = await where(".menu.m-invite");
  check(w.pop === 1 && w.main === 0, "invite right-click menu opens in the pop-out", JSON.stringify(w));
  await Q.key("Escape");
  await sleep(200);
  // 5) Share modal (a live share exists: the button opens the stream settings modal)
  await Q.click("#btn-share");
  await sleep(400);
  w = await where(".scrim .modal");
  check(w.pop === 1 && w.main === 0, "Stream settings modal (Share button, pop-out) opens in the pop-out", JSON.stringify(w));
  await shot("6-stream");
  // 6) the pop-out closes with the modal open: it moves to main
  await P.eval(`__sss.stageWindow().win.document.querySelector('#btn-popstage').click(); true`);
  await P.waitFor("!__sss.stageWindow()", { what: "stage back" });
  await sleep(400);
  check((await P.eval("document.querySelectorAll('.scrim .modal').length")) === 1, "pop-out closed under an open modal: it shows in the main page");
  await P.screenshot(OUT + "popmodal-7-moved-main.png");
  await P.key("Escape");
  await sleep(200);
  // 7) End call on the main page
  await P.click("#btn-leave");
  await sleep(300);
  check((await P.eval("document.querySelectorAll('.scrim .modal').length")) === 1 && (await P.eval("__sss.S.modal.hostId")) === "main", "End call on the main page: modal on main");
  await P.screenshot(OUT + "popmodal-8-main-leave.png");
  await P.eval("__sss.S.modal = null; __sss.render(); true");
} catch (e) {
  console.log("ERROR", e.stack || e);
  fails++;
} finally {
  if (A) await A.close?.();
  site.close?.();
}
console.log(fails ? `${fails} FAILED` : "ALL PASS");
process.exit(fails ? 1 : 0);
