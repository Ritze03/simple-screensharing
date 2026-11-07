// Task 145 through the REAL site UI: the website on a phone. Headless Chromium with phone emulation (CDP Emulation.setDeviceMetricsOverride mobile:true,
// setTouchEmulationEnabled) and REAL touch input (Input.dispatchTouchEvent, no mouse), against a real relay and a real stream: Alice (desktop viewport)
// shares a synthetic screen, the phone watches it (really decoded) at 390x844 portrait, 844x390 landscape and 360x640. The phone page has no
// getDisplayMedia and no Document Picture-in-Picture, like Android Chrome / iOS Safari. Video only, no audio is ever created.
//
//   usage: node docs/dev/e2e/phone.mjs <relay host:port> <certhash>      (relay: docs/dev/relay.sh start ...)
//   env:   E2E_OUT = where the screenshots go (default /tmp/sss-e2e), SHOT_PREFIX (default "ph_")
import { mkdirSync } from "node:fs";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const [RELAY, HASH] = process.argv.slice(2).filter((a) => !a.startsWith("--"));
if (!RELAY || !HASH) throw new Error("usage: phone.mjs <relay host:port> <certhash>");
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
const PFX = process.env.SHOT_PREFIX || "ph_";
mkdirSync(OUT, { recursive: true });
const sleep = L.sleep;

let failed = 0;
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  if (!cond) failed++;
};
const section = (s) => console.log(`\n== ${s}`);

const site = await L.serveSite(0);
const A = await L.openApp(site.url, { name: "Alice" });
const PH = await L.openApp(site.url, { name: "Pia", flags: ["--use-fake-device-for-media-stream"], viewport: [390, 844] });
const P = PH.page;
let sizeName = "";
const shot = (n) => P.screenshot(`${OUT}${PFX}${n}_${sizeName}.png`);

/* ---------- phone emulation + touch ---------- */
const SIZES = [["390x844", 390, 844], ["844x390", 844, 390], ["360x640", 360, 640]];
async function phoneSize(w, h) {
  await P.send("Emulation.setDeviceMetricsOverride", { width: w, height: h, deviceScaleFactor: 2, mobile: true, screenWidth: w, screenHeight: h });
  await P.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 });
  await sleep(400);
}
const touch = (type, x, y) => P.send("Input.dispatchTouchEvent", { type, touchPoints: type === "touchEnd" ? [] : [{ x, y, id: 1 }] });
async function tapAt(x, y) {
  await touch("touchStart", x, y);
  await sleep(60);
  await touch("touchEnd", x, y);
  await sleep(250);
}
async function pressAt(x, y, ms) {
  await touch("touchStart", x, y);
  await sleep(ms);
  await touch("touchEnd", x, y);
  await sleep(300);
}
const centre = (target) => P.eval(`(() => { const e = ${target.js ? target.js : `document.querySelector(${JSON.stringify(target)})`}; if (!e) return null; const r = e.getBoundingClientRect(); return r.width && r.height ? { x: r.x + r.width / 2, y: r.y + r.height / 2 } : null; })()`);
async function tap(target) {
  let c = null;
  for (let i = 0; i < 30 && !(c = await centre(target)); i++) await sleep(100);
  if (!c) throw new Error("tap: no visible element for " + JSON.stringify(target));
  await tapAt(c.x, c.y);
}
const noOverflow = async (what) => {
  const m = await P.eval("({ sw: document.documentElement.scrollWidth, iw: innerWidth, bw: document.body.scrollWidth, app: document.querySelector('#app').scrollWidth })");
  check(m.sw <= m.iw && m.bw <= m.iw, `${what}: no horizontal overflow`, JSON.stringify(m));
  // nothing sticks out of the screen (an element may be off-screen on purpose: the closed drawer, the off-screen harness canvas)
  const out = await P.eval(`[...document.querySelectorAll('#app *')].filter((e) => { if (e.closest('.p-side') && !document.querySelector('.drawer-open')) return false; const r = e.getBoundingClientRect(); const cs = getComputedStyle(e);
    if (!r.width || !r.height || cs.visibility === 'hidden' || cs.display === 'none' || e.closest('.arena, .menu, .utip, .vpop, .toasts') && false) return false;
    if (e.closest('.strip') || e.closest('.st-tile') || e.closest('.p-side-scroll') || e.closest('.modal-body')) return false; // these scroll or clip
    return r.right > innerWidth + 1 || r.left < -1; }).slice(0, 5).map((e) => e.className + ':' + Math.round(e.getBoundingClientRect().right))`);
  check(out.length === 0, `${what}: no control sticks out of the screen`, out.join(" "));
};
const inView = async (sel, what) => {
  const r = await P.eval(`(() => { const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return null; const r = e.getBoundingClientRect(); return { l: r.left, t: r.top, r: r.right, b: r.bottom, iw: innerWidth, ih: innerHeight }; })()`);
  check(r && r.l >= -0.5 && r.t >= -0.5 && r.r <= r.iw + 0.5 && r.b <= r.ih + 0.5, `${what} fits the viewport`, JSON.stringify(r));
};
const targets = (what, sel, min = 40) =>
  P.eval(`[...document.querySelectorAll(${JSON.stringify(sel)})].filter((e) => { const r = e.getBoundingClientRect(); return r.width && r.height && getComputedStyle(e).visibility !== 'hidden' && (r.height < ${min} - 0.5 || r.width < ${min - 12}); }).map((e) => (e.id || e.className) + ':' + Math.round(e.getBoundingClientRect().width) + 'x' + Math.round(e.getBoundingClientRect().height))`)
    .then((bad) => check(bad.length === 0, `${what}: tap targets >= ${min} px high`, bad.join(" ")));
const reset = () => P.eval("(() => { __sss.S.modal = null; __sss.S.menu = null; __sss.S.card = null; __sss.S.drawer = false; __sss.render(); })()").then(() => sleep(200));
const openDrawer = async () => {
  await tap(await P.eval("['#btn-sidebar', '#btn-drawer'].find((s) => { const e = document.querySelector(s); return e && e.getBoundingClientRect().width; })"));
  await P.waitFor("document.querySelector('.drawer-open')", { what: "drawer open" });
};

try {
  /* ============================== the phone is a phone ============================== */
  // no screen capture and no Document PiP, like Android Chrome / iOS Safari
  await P.addInitScript("try { delete MediaDevices.prototype.getDisplayMedia; } catch {} try { Object.defineProperty(window, 'documentPictureInPicture', { value: undefined }); } catch {}");
  // Wake Lock spy around the REAL navigator.wakeLock (headless Chromium grants it): counts requests and live sentinels, can make a request reject.
  await P.addInitScript(`(() => { if (!navigator.wakeLock) return; const real = navigator.wakeLock.request.bind(navigator.wakeLock);
    const wl = window.__wl = { calls: 0, live: 0, reject: false, sents: [] };
    navigator.wakeLock.request = async (t) => { wl.calls++; if (wl.reject) throw new DOMException("no gesture", "NotAllowedError"); const s = await real(t); wl.sents.push(s); wl.live++; s.addEventListener("release", () => wl.live--); return s; }; })()`);
  await phoneSize(390, 844);
  await P.goto(site.url);
  await P.waitFor("window.__sss && document.querySelector('#app').children.length > 0", { what: "reload" });
  section("environment");
  check(await P.eval("matchMedia('(pointer: coarse)').matches && matchMedia('(hover: none)').matches"), "emulation is a touch phone (pointer coarse, hover none)");
  check(await P.eval("!navigator.mediaDevices.getDisplayMedia && !window.documentPictureInPicture"), "no getDisplayMedia, no Document PiP");

  /* ============================== home, before any room ============================== */
  for (const [n, w, h] of SIZES) {
    sizeName = n;
    section(`home ${n}`);
    await phoneSize(w, h);
    await reset();
    await noOverflow("home");
    await shot("01_home");
    const land = h < 500;
    check(await centre(land ? "#btn-home-add" : "#btn-drawer") !== null, land ? "the home page has its Create/join button in view" : "a way to open the rooms list is always visible (menu button)");
    await openDrawer();
    await shot("02_drawer_empty");
    await noOverflow("drawer");
    await inView(".p-side", "the drawer");
    await tapAt(w - 10, 20); // the scrim
    check(await P.eval("!document.querySelector('.drawer-open')"), "a tap on the scrim closes the drawer");
  }

  /* ============================== a room, with real touch ============================== */
  section("setup: Alice shares, the phone joins by touch");
  await L.createRoom(A.page, { name: "phone room", server: RELAY, hash: HASH });
  await L.waitJoined(A.page);
  const link = await L.inviteLink(A.page);
  await L.stubCapture(A.page, { width: 960, height: 540, fps: 15, audio: false });
  await L.goLive(A.page, { codec: "h264", fps: 15 });

  sizeName = "390x844";
  await phoneSize(390, 844);
  await openDrawer();
  await tap("#btn-create-join");
  await P.waitFor("document.querySelector('#add-link')", { what: "join modal" });
  check(await P.eval("!document.querySelector('.drawer-open')"), "opening the dialog closes the drawer");
  await inView(".modal", "the join dialog");
  await noOverflow("join dialog");
  await shot("03_join_modal");
  await tap("#add-link");
  await P.eval("document.querySelector('#add-link').value = ''");
  await P.send("Input.insertText", { text: link });
  await sleep(150);
  await tap("#btn-join");
  await L.waitJoined(P);
  await P.waitFor("document.querySelector('.st-tile.idle [data-watch]')", { timeout: 30000, what: "Alice's idle tile" });
  await shot("04_room_idle");
  check(true, "joined a room through the dialog by touch");

  /* ---------- screen wake lock (real lock in headless Chromium, requests spied) ---------- */
  section("wake lock: joined on a phone, not watching");
  const WL = (e) => P.eval("(() => { const w = __wl; return " + e + "; })()");
  await P.waitFor("__wl.live === 1", { what: "wake lock held after joining on a phone" });
  await P.eval("for (let i = 0; i < 5; i++) __sss.render()");
  await sleep(300);
  check(await WL("w.calls === 1 && w.live === 1"), "joined on a phone holds ONE wake lock; repeated renders issue no duplicate requests", JSON.stringify(await WL("({ calls: w.calls, live: w.live })")));
  await P.eval("__wl.sents[__wl.sents.length - 1].release()"); // what the browser does when the page is hidden
  await P.eval("document.dispatchEvent(new Event('visibilitychange'))");
  await P.waitFor("__wl.live === 1 && __wl.calls === 2", { what: "wake lock re-acquired when visible again" });
  check(true, "a dropped lock is re-acquired on visibilitychange (visible)");
  await P.eval("__wl.reject = true; __wl.sents[__wl.sents.length - 1].release(); document.dispatchEvent(new Event('visibilitychange'))");
  await sleep(300);
  check(await WL("w.live === 0 && w.calls === 3"), "a rejected request leaves no lock and raises nothing", JSON.stringify(await WL("({ calls: w.calls, live: w.live })")));
  await P.eval("__wl.reject = false");
  await tapAt(5, 5);
  await P.waitFor("__wl.live === 1", { what: "wake lock acquired by the next tap" });
  check(true, "after a rejected request the next tap retries and gets the lock");

  // the sidebar room row: long-press opens the Room settings menu
  section("long-press on a room");
  await openDrawer();
  const rowC = await centre({ js: "document.querySelector('.p-side .p-row[title]:not(:first-child)') || document.querySelectorAll('.p-side .p-row')[1]" });
  await pressAt(rowC.x, rowC.y, 800);
  await P.waitFor("document.querySelector('.menu#menu')", { what: "room menu after a long press" });
  check(await P.eval("__sss.S.menu && __sss.S.menu.kind === 'room'"), "a long press on a room opens its Room settings menu");
  check(await P.eval("!!document.querySelector('.drawer-open')"), "...and the press did not pick the room (drawer still open)");
  await shot("05_room_menu_longpress");
  await inView(".menu#menu", "the room menu");
  await tapAt(5, 5);
  await sleep(300);
  check(await P.eval("!document.querySelector('.menu#menu')"), "a tap outside closes the menu");
  await pressAt(rowC.x, rowC.y, 150); // a short tap: picks the room
  check(await P.eval("!document.querySelector('.drawer-open') && !document.querySelector('.menu#menu')"), "a plain tap picks the room and closes the drawer");

  // watch the stream
  section("watching");
  await tap("[data-watch]");
  await P.waitFor("__sss.S.rooms[0].watches.size === 1 && [...__sss.S.rooms[0].watches.values()][0].firstFrame", { timeout: 40000, what: "first frame" });
  await sleep(1500);
  const st = await P.eval("(() => { const r = __sss.S.rooms[0]; const w = [...r.watches.values()][0]; const s = r.client.watchStats(w.key); return { decoded: s.decoded, w: s.width, h: s.height, cw: w.canvas.width, ch: w.canvas.height }; })()");
  check(st.decoded > 5 && st.w === 960 && st.h === 540, "the phone decodes the stream (real WebCodecs decode)", JSON.stringify(st));
  check(await P.eval("!!document.querySelector('#btn-noscreen') && !document.querySelector('#btn-share') && !document.querySelector('#btn-audio')"), "no getDisplayMedia: Share screen / Share audio are replaced by one dimmed button");
  check(await P.eval("!!document.querySelector('#btn-webcam') && !document.querySelector('#btn-popstage')"), "Webcam stays, pop-out is hidden without Document PiP");

  // A decoder failure, through the real recovery path (the next decode() throws what WebKit's error says): the last picture stays.
  section("decoder failure");
  await P.eval(`(() => { const r = __sss.S.rooms[0]; const w = [...r.watches.values()][0]; window.__tiles = 0; window.__watch = w;
    window.__spy = setInterval(() => { if (document.querySelector(".st-tile .wait")) window.__tiles++; }, 20);
    const d = r.client.subs.get(w.key).impl.decoder; d.decode = () => { throw Object.assign(new Error("Decoder failure"), { name: "EncodingError" }); }; })()`);
  await P.waitFor("[...__sss.S.rooms[0].watches.values()][0].stats?.decoderErrors >= 1", { timeout: 10000, what: "the decoder failed" });
  await P.waitFor("(() => { const r = __sss.S.rooms[0]; const s = r.client.watchStats([...r.watches.keys()][0]); return !s.needKeyframe && s.decodedFps > 1; })()", { timeout: 20000, what: "decoding again" });
  const df = await P.eval(`(() => { clearInterval(window.__spy); const r = __sss.S.rooms[0]; const w = [...r.watches.values()][0];
    return { tiles: window.__tiles, same: w === window.__watch, first: w.firstFrame, line: __sss.S.dropLog.map((e) => e.text).find((t) => t.includes("decoder-error")) }; })()`);
  check(df.tiles === 0 && df.same && df.first, "no waiting tile, the same watch: the last picture stays", JSON.stringify(df));
  check(/decoder-error #1 \(EncodingError: Decoder failure\) \S+ 960x540 hw=no-preference q=\d+ key=\d+ms\/\d+f break=none out=\d+ gaps=0 chain=\w+ ptsback=0/.test(df.line || ""), "the Connection log has the whole story in one line");

  for (const [n, w, h] of SIZES) {
    sizeName = n;
    section(`stage ${n}`);
    await phoneSize(w, h);
    await reset();
    await sleep(500);
    await noOverflow("stage");
    await shot("06_stage");
    const tl = await P.eval("(() => { const t = document.querySelector('.arena > .st-tile'); const a = document.querySelector('.arena').getBoundingClientRect(); const r = t.getBoundingClientRect(); return { aw: a.width, ah: a.height, tw: r.width, th: r.height }; })()");
    check(tl.tw > 150 && tl.th > 80, "the stream tile has a usable size", JSON.stringify(tl));
    await targets("the bar", ".ctlrow .cb:not(.caret), .ctlrow .caret", 40);
    await targets("the tile buttons", ".st-tile .round-btn", 40);
    await targets("the header", ".p-head .icon-btn", 40);
    check(await P.eval("!!document.querySelector('.st-tile .tools') && getComputedStyle(document.querySelector('.st-tile .tools')).opacity === '1'"), "the tile's buttons and name are visible without hover");
    check(await P.eval("!document.querySelector('.arena > .gut') || getComputedStyle(document.querySelector('.arena > .gut')).display === 'none'"), "no resize bars on a phone");

    // tile menu by tap on the always-visible ⋯
    await tap('.st-tile [data-mk^="tile:"]');
    await P.waitFor("document.querySelector('.menu#menu')", { what: "tile menu" });
    await inView(".menu#menu", "the stream menu");
    await shot("07_tile_menu");
    await tapAt(3, 3);
    await sleep(250);

    // layout menu: no Floating
    await tap("#btn-layout");
    await P.waitFor("document.querySelector('.menu.m-layout')", { what: "layout menu" });
    check(await P.eval("![...document.querySelectorAll('.menu .mi')].some((e) => /Floating/.test(e.innerText))"), "the Layout menu offers no Floating on a phone");
    await inView(".menu.m-layout", "the layout menu");
    await shot("08_layout_menu");
    await tapAt(3, 3);
    await sleep(250);

    // people strip: a tap shows the hover card
    if (await centre(".strip .pv")) {
      await tap(".strip .pv");
      await P.waitFor("document.querySelector('.utip')", { what: "people card by tap" });
      await inView(".utip", "the people card");
      check(true, "a tap on a person opens their card");
      await shot("09_people_card");
      await tapAt(3, 3);
      await sleep(250);
      check(await P.eval("!document.querySelector('.utip')"), "a tap elsewhere closes it");
    }

    // the no-screen button explains itself
    await tap("#btn-noscreen");
    check(await P.eval("[...document.querySelectorAll('.toast')].some((e) => /can't share a screen/.test(e.innerText))"), "the dimmed share button says why on tap");
    await shot("10_noscreen_toast");
    await sleep(300);

    // room settings menu from the bar
    await tap("#btn-room");
    await P.waitFor("document.querySelector('.menu#menu')", { what: "room menu from the bar" });
    await inView(".menu#menu", "the bar's room menu");
    await shot("11_bar_room_menu");
    await tapAt(3, 3);
    await sleep(250);

    // settings modal: fits and scrolls
    await openDrawer();
    await shot("12_drawer_room");
    await targets("the drawer rows", ".p-side .p-row, .p-side .p-sub", 40);
    await tap("#btn-settings");
    await P.waitFor("document.querySelector('.modal')", { what: "settings" });
    await inView(".modal", "the settings dialog");
    await noOverflow("settings");
    const scr = await P.eval("(() => { const b = document.querySelector('.modal-body'); return { sh: b.scrollHeight, ch: b.clientHeight, ov: getComputedStyle(b).overflowY }; })()");
    check(scr.ov === "auto" && scr.sh >= scr.ch, "the dialog body scrolls when it is taller than the screen", JSON.stringify(scr));
    await shot("13_settings");
    await tap('.modal-head .icon-btn');
    await sleep(250);

    // fullscreen of the stage (the real Fullscreen API where there is one, else the stage-only layout) and the chrome by tap
    await tap("#btn-stagefs");
    await P.waitFor("__sss.S.stage.fullscreen", { what: "fullscreen" });
    await sleep(3600); // the chrome fades after 3 s
    check(await P.eval("!!document.querySelector('.chrome-off')"), "in fullscreen the chrome hides by itself");
    await shot("14_fullscreen_chrome_off");
    await tapAt(w / 2, h / 2);
    await sleep(400);
    check(await P.eval("!document.querySelector('.chrome-off')"), "a tap on the stage brings the chrome back");
    await shot("15_fullscreen_chrome_on");
    await noOverflow("fullscreen");
    await tap("#btn-stagefs");
    await P.waitFor("!__sss.S.stage.fullscreen", { what: "fullscreen left" });
    await sleep(300);

    // a tile's fullscreen where the browser has no element fullscreen (iPhone: API missing) or refuses it (rejects): never an error, a fixed
    // full-viewport stage with a visible exit, which survives render(). SIMULATED: the API is removed / made to reject by overriding
    // Element.prototype.requestFullscreen; real WebKit must confirm.
    for (const mode of ["missing", "rejects"]) {
      await P.eval(`(() => { window.__rf = window.__rf || Element.prototype.requestFullscreen;
        Element.prototype.requestFullscreen = ${mode === "missing" ? "undefined" : "() => Promise.reject(new TypeError('Type error'))"}; })()`);
      await tap('.st-tile .tools button[aria-label="Fullscreen"]');
      await P.waitFor("document.querySelector('.pip-stage.css-fs')", { what: `fallback fullscreen (${mode})` });
      check(await P.eval("!document.querySelector('.toast.err')"), `tile fullscreen, API ${mode}: no error toast`);
      await P.eval("__sss.render()");
      check(await P.eval("!!document.querySelector('.pip-stage.css-fs canvas')"), `API ${mode}: the fullscreen layer survives a render()`);
      await inView(".pip-stage.css-fs", "the fallback fullscreen layer");
      await inView(".pip-stage.css-fs .round-btn", "its exit button");
      await shot(`16_tile_fullscreen_${mode}`);
      await tap(".pip-stage.css-fs .round-btn");
      await P.waitFor("!document.querySelector('.pip-stage')", { what: "fallback fullscreen left" });
      check(await P.eval("!document.querySelector('.toast.err') && !!document.querySelector('.st-tile canvas')"), `API ${mode}: exit brings the tile back`);
    }
    // the stage's own fullscreen, refused: the stage-only layout stays (it used to snap back silently)
    await P.eval("Element.prototype.requestFullscreen = () => Promise.reject(new TypeError('Type error'))");
    await tap("#btn-stagefs");
    await sleep(500);
    check(await P.eval("__sss.S.stage.fullscreen"), "stage fullscreen refused by the browser: the stage-only layout stays");
    await tap("#btn-stagefs");
    await P.waitFor("!__sss.S.stage.fullscreen", { what: "stage-only layout left" });
    await P.eval("Element.prototype.requestFullscreen = window.__rf");
    await sleep(300);
  }

  /* ============================== wake lock ends with the room ============================== */
  section("wake lock: leaving releases it");
  await P.eval("__sss.S.modal = { kind: 'leave', room: __sss.S.rooms[0].id }; __sss.render()");
  await tap(".modal .btn-danger-solid");
  await P.waitFor("__wl.live === 0", { what: "wake lock released after leaving the room" });
  check(await P.eval("__sss.S.rooms[0].state === 'disconnected' && __wl.live === 0"), "leaving the room releases the wake lock (watching ended with it)");
  await sleep(300);
  check(await WL("w.live === 0"), "...and it stays released");

  section("wake lock: no navigator.wakeLock");
  const NW = await L.openApp(site.url, { name: "NoWL" });
  await NW.page.addInitScript("try { Object.defineProperty(navigator, 'wakeLock', { value: undefined }); } catch {}");
  await NW.page.goto(site.url);
  await NW.page.waitFor("window.__sss && document.querySelector('#app').children.length > 0", { what: "boot without wakeLock" });
  await L.createRoom(NW.page, { name: "nowl room", server: RELAY, hash: HASH });
  await L.waitJoined(NW.page);
  await NW.page.send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 2, mobile: true }); // now a phone: a lock would be wanted
  await sleep(500);
  check(await NW.page.eval("navigator.wakeLock === undefined") && NW.consoleErrors.length === 0, "a browser without navigator.wakeLock joins a room and raises nothing", NW.consoleErrors.slice(0, 2).join(" | "));
  await NW.close();

  /* ============================== a state with no WebTransport is not a blank page ============================== */
  section("unsupported browser message");
  const Z = await L.openApp(site.url, { name: "Old" });
  await Z.page.addInitScript("try { delete window.WebTransport; } catch {}");
  await Z.page.goto(site.url);
  await Z.page.waitFor("document.querySelector('#app').children.length > 0", { what: "boot without WebTransport" });
  check(await Z.page.eval("/WebTransport/.test(document.body.innerText) && /Missing in this browser/.test(document.body.innerText)"), "a browser without WebTransport gets a clear message naming what is missing");
  await Z.page.screenshot(`${OUT}${PFX}16_unsupported.png`);
  await Z.close();
} catch (e) {
  console.log("FAIL exception: " + (e.stack || e));
  failed++;
  try { await shot("zz_failure"); } catch {}
}
check(PH.consoleErrors.length === 0, "no console errors on the phone", PH.consoleErrors.slice(0, 3).join(" | "));
await PH.close();
await A.close();
site.close();
console.log(failed ? `\n${failed} FAILED` : "\nALL PASSED");
process.exit(failed ? 1 : 0);
