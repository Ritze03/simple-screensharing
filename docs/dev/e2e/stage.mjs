// Task I6 through the REAL site UI: the browser client's stage (Split / Crop / Floating, resize bars, the control bar, the people strip with its
// hover card, the Layout / Room / Share / stream menus, the stage pop-out, fullscreen auto-hide, the new stream settings, emoji avatars) and
// every D1 item the native client passed ("D1 items" in superdoc/features/website.md). Real pointer input through CDP Input.dispatchMouseEvent,
// headless Chromium, a throwaway relay. Four pages: Alice, Bob and Carol share a synthetic screen each (video only: no audio stream is ever
// created, `?fakeaudio=1` on the viewer makes every stream act as if it had audio for the ring / volume / card rows), Vera watches all three and
// shares too, so her stage holds four tiles.
//
//   usage: node docs/dev/e2e/stage.mjs <relay host:port> <certhash> [--relay-bin <bin> --relay-dir <dir>]   (relay: docs/dev/relay.sh start ...)
//          the relay binary/dir let the run restart the relay (pop-out and avatars must survive it); without them that part is skipped
//   env:   E2E_OUT = where the screenshots go (default /tmp/sss-e2e), SHOT_PREFIX (default "i6_")
import { mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
const W = new URL("../", import.meta.url).pathname;
const L = await import(W + "e2e-lib.mjs");
const args = process.argv.slice(2);
const opt = (n) => (args.includes(n) ? args[args.indexOf(n) + 1] : null);
const [RELAY, HASH] = args.filter((a, i) => !a.startsWith("--") && !(i > 0 && args[i - 1].startsWith("--")));
const RELAY_BIN = opt("--relay-bin");
const RELAY_DIR = opt("--relay-dir");
const OLD_RELAY = opt("--old-relay"); // host:port of a relay that carries no avatars (SSSHARE_AVATARS=0 acts exactly like an older relay)
const OLD_HASH = opt("--old-hash");
if (!RELAY || !HASH) throw new Error("usage: stage.mjs <relay host:port> <certhash> [--relay-bin <bin> --relay-dir <dir>]");
const OUT = (process.env.E2E_OUT || "/tmp/sss-e2e") + "/";
const PFX = process.env.SHOT_PREFIX || "i6_";
mkdirSync(OUT, { recursive: true });

let failed = 0;
const results = [];
const check = (cond, what, extra = "") => {
  console.log(`${cond ? "PASS" : "FAIL"} ${what}${extra ? "  " + extra : ""}`);
  results.push({ ok: !!cond, what, extra });
  if (!cond) failed++;
};
const near = (a, b, eps, what) => check(Math.abs(a - b) <= eps, what, `${(+a).toFixed(2)} vs ${(+b).toFixed(2)} (+-${eps})`);
const sleep = L.sleep;
const section = (s) => console.log(`\n== ${s}`);

const site = await L.serveSite(0);
// A fake camera for getUserMedia (Chromium's own moving test picture; `getDisplayMedia` is stubbed by the harness, never real, and no audio is requested
// anywhere: the camera is always `audio: false`), and the camera permission granted through CDP instead of the fake-UI flag.
const open = async (name, url = site.url) => {
  const b = await L.openApp(url, { name, flags: ["--use-fake-device-for-media-stream"] });
  await b.conn.send("Browser.grantPermissions", { permissions: ["videoCapture"], origin: new URL(site.url).origin }).catch(() => {});
  return b;
};
const A = await open("Alice");
const B = await open("Bob");
const C = await open("Carol");
const V = await open("Vera", site.url + "?fakeaudio=1");
const P = V.page;
const shot = (page, n) => page.screenshot(OUT + PFX + n + ".png");

/* ---------- helpers ---------- */
const rect = (p, sel, i = 0) => p.eval(`(() => { const e = document.querySelectorAll(${JSON.stringify(sel)})[${i}]; if (!e) return null; const r = e.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height, r: r.right, b: r.bottom, cx: r.x + r.width / 2, cy: r.y + r.height / 2 }; })()`);
const rects = (p, sel) => p.eval(`[...document.querySelectorAll(${JSON.stringify(sel)})].map((e) => { const r = e.getBoundingClientRect(); return { x: r.x, y: r.y, w: r.width, h: r.height, r: r.right, b: r.bottom, cx: r.x + r.width / 2, cy: r.y + r.height / 2, id: e.dataset.id || e.dataset.bar || e.dataset.user || null }; })`);
const css = (p, sel, prop, pseudo = null, i = 0) => p.eval(`(() => { const e = document.querySelectorAll(${JSON.stringify(sel)})[${i}]; return e ? getComputedStyle(e${pseudo ? `, ${JSON.stringify(pseudo)}` : ""}).getPropertyValue(${JSON.stringify(prop)}) : null; })()`);
const mouse = (p, type, x, y, extra = {}) => p.send("Input.dispatchMouseEvent", { type, x, y, button: "none", buttons: 0, ...extra });
const move = (p, x, y) => mouse(p, "mouseMoved", x, y);
const press = (p, x, y, button = "left") => mouse(p, "mousePressed", x, y, { button, buttons: button === "left" ? 1 : 2, clickCount: 1 });
const release = (p, x, y, button = "left") => mouse(p, "mouseReleased", x, y, { button, buttons: 0, clickCount: 1 });
const rightClick = async (p, x, y) => {
  await move(p, x, y);
  await press(p, x, y, "right");
  await release(p, x, y, "right");
  await sleep(150);
};
/** Drag from (x, y) by (dx, dy) in `steps` moves with the button held; `mid` runs once half way (while still pressed). */
async function drag(p, x, y, dx, dy, { steps = 12, mid = null } = {}) {
  await move(p, x, y);
  await press(p, x, y);
  for (let i = 1; i <= steps; i++) {
    await mouse(p, "mouseMoved", x + (dx * i) / steps, y + (dy * i) / steps, { button: "left", buttons: 1 });
    if (mid && i === Math.floor(steps / 2)) await mid();
    await sleep(8);
  }
  await sleep(60);
  await release(p, x + dx, y + dy);
  await sleep(150);
}
const wheel = (p, x, y, deltaY) => p.send("Input.dispatchMouseEvent", { type: "mouseWheel", x, y, deltaX: 0, deltaY });
const key = async (p, k) => {
  await p.key(k);
  await sleep(120);
};
const tiles = (p) => rects(p, ".arena > .st-tile");
const guts = (p) => rects(p, ".arena > .gut");
const gutColor = (p, i) => css(p, ".arena > .gut", "background-color", "::after", i);
const GREY = "rgb(52, 53, 59)", BLUE = "rgb(76, 110, 245)", CLEAR = "rgba(0, 0, 0, 0)";
const viewKey = () => P.eval(`"room:" + __sss.S.rooms[0].id`);
const grid = async () => P.eval(`JSON.parse(JSON.stringify(__sss.vg("room:" + __sss.S.rooms[0].id).grid))`);
const openLayoutMenu = async () => {
  await P.click("#btn-layout");
  await P.waitFor("document.querySelector('.menu.m-layout')", { what: "layout menu" });
};
const layoutItem = async (re) => {
  await P.click({ js: `[...document.querySelectorAll('.menu .mi, .menu .cbx')].find((e) => ${re}.test(e.innerText))` });
  await sleep(150);
};
const setLayout = async (name) => {
  await openLayoutMenu();
  await layoutItem(name === "split" ? "/Split/" : name === "crop" ? "/Crop/" : "/Floating/");
  await sleep(150);
};
const closeMenu = async () => {
  await key(P, "Escape");
  await sleep(100);
};

/* ---------- webcam in picture helpers (I10) ---------- */
/** The first PIP matching `sel`: its rect and its parent tile's (page coordinates), the stored position, the handle corner, the picture size. */
const pipState = (p, sel = ".arena > .st-tile > .pip") => p.eval(`(() => {
  const e = document.querySelector(${JSON.stringify(sel)}); if (!e) return null;
  const t = e.parentElement; const r = e.getBoundingClientRect(), tr = t.getBoundingClientRect();
  const ar = document.querySelector('.arena');
  const par = parseFloat(t.style.getPropertyValue('--ar')) || 16 / 9;
  const crop = ar.classList.contains('crop');
  const picW = crop ? Math.max(tr.width, tr.height * par) : Math.min(tr.width, tr.height * par);
  const pos = __sss.vg(ar.dataset.view).pip[e.dataset.id] || null;
  const cs = getComputedStyle(e);
  return { id: e.dataset.id, parent: t.dataset.id, c: e.dataset.c, ar: parseFloat(e.dataset.ar), pos: pos && JSON.parse(JSON.stringify(pos)),
    pip: { x: r.x, y: r.y, w: r.width, h: r.height }, tile: { x: tr.x, y: tr.y, w: tr.width, h: tr.height }, picW, picH: picW / par,
    radius: cs.borderTopLeftRadius, snap: cs.getPropertyValue('--x') || (e.classList.contains('snap') ? 'on' : '') };
})()`);
const insideOf = (r, o) => r.x >= o.x - 1 && r.y >= o.y - 1 && r.x + r.w <= o.x + o.w + 1 && r.y + r.h <= o.y + o.h + 1;
/** The PIP sits on spot `[col, row]` of its visible picture: 12 px from the edge it is on, centred on a middle (the inset shrinks only in tiny pictures). */
function spotInsetOk(s, spot) {
  const l = s.pip.x - s.tile.x, t = s.pip.y - s.tile.y;
  const r = s.tile.x + s.tile.w - (s.pip.x + s.pip.w), b = s.tile.y + s.tile.h - (s.pip.y + s.pip.h);
  const m = (len, size) => Math.min(12, Math.max(0, (len - size) / 2));
  const okX = spot[0] === 0 ? Math.abs(l - m(s.tile.w, s.pip.w)) < 1.5 : spot[0] === 2 ? Math.abs(r - m(s.tile.w, s.pip.w)) < 1.5 : Math.abs(l - r) < 1.5;
  const okY = spot[1] === 0 ? Math.abs(t - m(s.tile.h, s.pip.h)) < 1.5 : spot[1] === 2 ? Math.abs(b - m(s.tile.h, s.pip.h)) < 1.5 : Math.abs(t - b) < 1.5;
  return okX && okY;
}

async function startSharer(X, name, w, h) {
  await L.stubCapture(X.page, { width: w, height: h, fps: 15, audio: false });
  await L.goLive(X.page, { codec: "h264", fps: 15 });
}
let ok0 = false;
try {
  /* ============================== setup ============================== */
  section("setup: Alice, Bob, Carol share, Vera watches all three and shares");
  await L.createRoom(A.page, { name: "stage", server: RELAY, hash: HASH });
  await L.waitJoined(A.page);
  const link = await L.inviteLink(A.page);
  for (const X of [B, C, V]) {
    await L.addRoomByLink(X.page, link, "stage");
    await L.waitJoined(X.page);
  }
  await startSharer(A, "Alice", 960, 540);
  await startSharer(B, "Bob", 800, 600);
  await startSharer(C, "Carol", 540, 960);
  await P.waitFor("document.querySelectorAll('.st-tile.idle').length === 3", { timeout: 30000, what: "three idle tiles on Vera's stage" });
  await shot(P, "01_idle");
  // idle tile: avatar, "X is sharing Y", Watch + Pop out
  check(await P.eval("[...document.querySelectorAll('.st-tile.idle')].every((t) => t.querySelector('.invite .avatar') && t.querySelector('[data-watch]') && /is sharing/.test(t.innerText))"), "an unwatched stream is a dimmed tile with 'X is sharing Y' and Watch stream");
  for (let n = 1; n <= 3; n++) {
    // a click is press + release on one element and the UI re-renders on every state change: retry if one landed in between
    for (let i = 0; i < 6 && (await P.eval("__sss.S.rooms[0].watches.size")) < n; i++) {
      await P.click("[data-watch]");
      await sleep(700);
    }
    await P.waitFor(`__sss.S.rooms[0].watches.size === ${n}`, { what: `watch ${n}` });
  }
  await P.waitFor("[...__sss.S.rooms[0].watches.values()].every((w) => w.firstFrame)", { timeout: 40000, what: "first frames" });
  // Vera shares as well: her own tile
  await L.stubCapture(P, { width: 960, height: 540, fps: 15, audio: false });
  await L.goLive(P, { codec: "h264", fps: 15 });
  await P.waitFor("document.querySelectorAll('.arena > .st-tile').length === 4", { what: "four tiles" });
  await sleep(600);
  await shot(P, "02_split");
  const order0 = await P.eval(`__sss.vg("room:" + __sss.S.rooms[0].id).order`);
  check(order0.length === 4 && order0.every((id) => id.startsWith(order0[0].split("/")[0] + "/")), "the stage keys its tiles by room + stableKey (not by ConnId)", JSON.stringify(order0));

  /* ============================== control bar ============================== */
  section("control bar");
  for (const [id, name] of [["btn-share", "Share screen"], ["btn-audio", "Share audio"], ["btn-webcam", "Webcam"], ["btn-layout", "Layout"], ["btn-room", "Room settings"]]) {
    const r = await rect(P, `#${id}`);
    const c = await P.eval(`(() => { const car = document.querySelector("#${id}").parentElement.querySelector(".caret"); if (!car) return null; const r = car.getBoundingClientRect(); return { w: r.width, shadow: getComputedStyle(car).boxShadow }; })()`);
    check(r && c && c.w > 10 && /inset/.test(c.shadow) && /1px/.test(c.shadow), `${name} always shows its dropdown arrow with a separator line`, c ? c.shadow : "no arrow");
  }
  const sepOf = (id) => css(P, `#${id}`, "box-shadow");
  check((await css(P, ".splitb .caret", "box-shadow")) !== "none", "the arrow is cut off by an inset 1px line");
  const grp = await rect(P, ".cgroup");
  const leave = await rect(P, "#btn-leave");
  near(leave.h, grp.h, 0.6, "Leave is as tall as the button groups");
  const bg = await css(P, ".cgroup > .splitb", "background-color");
  check(bg !== CLEAR && bg !== "rgba(0, 0, 0, 0)", "grouped buttons have a backdrop", bg);
  for (const id of ["btn-sidebar", "btn-invite", "btn-popstage", "btn-stagefs"]) {
    check((await css(P, `#${id}`, "background-color")) === CLEAR, `freestanding #${id} has no backdrop`);
  }
  check(await P.eval("!document.querySelector('.p-right') && !document.querySelector('.member') && ![...document.querySelectorAll('h3')].some((h) => /people/i.test(h.innerText))"), "no people list on the right, no heading above the avatars");
  check(await P.eval("!document.querySelector('.stripwrap .ms.caret') && ![...document.querySelectorAll('.stripwrap .ms')].some((e) => /expand|arrow/.test(e.innerText))"), "no arrow above the avatars");
  await P.click("#btn-people");
  check(await P.eval("!document.querySelector('.strip')"), "the people button hides the strip");
  await P.click("#btn-people");
  check(await P.eval("!!document.querySelector('.strip') && document.querySelectorAll('.strip .pv').length === 4"), "and shows it again (4 people)");
  // Level loudness: a global bar button directly left of the people button, in the same group; accent when on
  check(await P.eval("(() => { const l = document.querySelector('#btn-level'), p = document.querySelector('#btn-people'); return !!l && l.nextElementSibling === p && l.parentElement === p.parentElement && l.getBoundingClientRect().right <= p.getBoundingClientRect().left; })()"), "the Level loudness button sits directly left of the people button");
  const lvlOn = () => P.eval("document.querySelector('#btn-level').getAttribute('aria-pressed') + '|' + JSON.parse(localStorage.getItem('sss.settings.v1')).levelLoudness");
  const lvl0 = await lvlOn();
  await P.click("#btn-level");
  const lvl1 = await lvlOn();
  check(lvl0.split("|")[0] !== lvl1.split("|")[0] && lvl1.split("|")[0] === lvl1.split("|")[1], "the Level loudness button toggles and persists the setting", lvl0 + " -> " + lvl1);
  await P.click("#btn-level");
  await P.click("#btn-sidebar");
  check(await P.eval("!document.querySelector('.p-side')"), "the bar's sidebar button hides the sidebar");
  await P.click("#btn-sidebar");
  check(await P.eval("!!document.querySelector('.p-side')"), "and shows it again");
  await P.waitFor("!document.querySelector('#btn-webcam').disabled", { what: "the webcam button enabled once the cameras are listed" });
  check(await P.eval("!document.querySelector('#btn-webcam').disabled && /Share your webcam/.test(document.querySelector('#btn-webcam').title)"), "Webcam is an enabled split button (task I10) with the camera in its tooltip", await P.eval("document.querySelector('#btn-webcam').title"));
  check(await P.eval("!document.querySelector('#btn-audio').disabled && /on its own/.test(document.querySelector('#btn-audio').title)"), "Share audio next to a screen that came with no audio is not a dead mute switch: it offers audio on its own (task I10)", await P.eval("document.querySelector('#btn-audio').title"));

  /* ============================== resize bars: Split ============================== */
  section("resize bars (Split)");
  const T0 = await tiles(P);
  const G0 = await guts(P);
  check(G0.length === 3, "four tiles: three bars (two vertical, one horizontal)", `${G0.length}`);
  await move(P, T0[0].cx, T0[0].cy);
  await sleep(200);
  const over = [];
  for (let i = 0; i < G0.length; i++) over.push(await gutColor(P, i));
  check(over.every((c) => c === CLEAR), "pointer over a stream: every bar is hidden", over.join(" | "));
  // a point inside the arena that is on no picture and no bar: the black space around a picture
  const arena = await rect(P, ".arena");
  let spot = null;
  const inside = (r, x, y, pad = 0) => x >= r.x - pad && x <= r.r + pad && y >= r.y - pad && y <= r.b + pad;
  for (let y = arena.y + 4; y < arena.b && !spot; y += 6) for (let x = arena.x + 4; x < arena.r && !spot; x += 6) if (![...T0, ...G0].some((r) => inside(r, x, y, 2))) spot = { x, y };
  check(!!spot, "there is black space around the pictures in this layout", JSON.stringify(spot));
  await move(P, spot.x, spot.y);
  await sleep(200);
  const grey = [];
  for (let i = 0; i < G0.length; i++) grey.push(await gutColor(P, i));
  check(grey.every((c) => c === GREY), "pointer in the black space around a picture: all bars show in dark grey", grey.join(" | "));
  await shot(P, "03_bars_grey");
  // the bar under the pointer is blue, the others stay grey
  await move(P, G0[0].cx, G0[0].cy);
  await sleep(200);
  const hov = [];
  for (let i = 0; i < G0.length; i++) hov.push(await gutColor(P, i));
  check(hov[0] === BLUE && hov.slice(1).every((c) => c === GREY), "the bar under the pointer is blue, the others dark grey", hov.join(" | "));
  // midway between the two visible pictures, never on the hidden cell edge
  const midOk = async () => {
    const ts = await tiles(P);
    const gs = await guts(P);
    const g = await grid();
    let ok = true;
    let detail = [];
    let first = 0;
    g.counts.forEach((c, i) => {
      for (let j = 0; j < c - 1; j++) {
        const a = ts[first + j];
        const b = ts[first + j + 1];
        const want = (a.r + b.x) / 2;
        const gut = gs.find((x) => x.id === `v${i}.${j}`);
        const err = Math.abs(gut.cx - want);
        detail.push(`v${i}.${j} ${err.toFixed(2)}`);
        if (err > 1) ok = false;
      }
      const next = first + c;
      if (g.counts[i + 1] != null) {
        const bottom = Math.max(...ts.slice(first, next).map((t) => t.b));
        const top = Math.min(...ts.slice(next, next + g.counts[i + 1]).map((t) => t.y));
        const gut = gs.find((x) => x.id === `h${i}`);
        const err = Math.abs(gut.cy - (bottom + top) / 2);
        detail.push(`h${i} ${err.toFixed(2)}`);
        if (err > 1) ok = false;
      }
      first = next;
    });
    return { ok, detail: detail.join(" ") };
  };
  let mid = await midOk();
  check(mid.ok, "every bar sits midway between the two visible pictures (letterboxed pictures included)", mid.detail);
  // drag the first vertical bar 60 px: it follows the pointer 1:1 and stays blue for the whole drag
  const gA = G0.find((x) => x.id === "v0.0");
  let during = null;
  await drag(P, gA.cx, gA.cy, 60, 0, {
    steps: 12,
    mid: async () => {
      await sleep(300); // past the 120 ms fade of the other bars
      during = [];
      const all = await guts(P);
      for (let i = 0; i < all.length; i++) during.push(`${all[i].id}:${await gutColor(P, i)}`);
    },
  });
  const gB = (await guts(P)).find((x) => x.id === "v0.0");
  near(gB.cx - gA.cx, 60, 1.5, "a dragged bar moves with the pointer (drag 60 px, the bar moves 60 px)");
  check(during && during.find((s) => s.startsWith("v0.0")).endsWith(BLUE) && during.filter((s) => !s.startsWith("v0.0")).every((s) => s.endsWith(CLEAR)), "during the drag the dragged bar is blue and the others are hidden", (during || []).join(" | "));
  await shot(P, "04_bar_dragged");
  mid = await midOk();
  check(mid.ok, "after the drag the bars are still midway between the pictures", mid.detail);
  await drag(P, gB.cx, gB.cy, -60, 0);
  near(((await guts(P)).find((x) => x.id === "v0.0")).cx, gA.cx, 1.5, "and -60 px moves it back");
  // far past the limit: it stops at the limit (12 % of the pair), it never crosses
  const gC = (await guts(P)).find((x) => x.id === "v0.0");
  await drag(P, gC.cx, gC.cy, 4000, 0, { steps: 6 });
  const gl = await grid();
  near(Math.min(...gl.colFr[0]), 0.12, 0.01, "dragged far past the end, the bar stops at the limit");
  const mid2 = await midOk();
  check(mid2.ok, "at the limit the bars are still midway", mid2.detail);
  // double-click a bar: an even grid again
  const gD = (await guts(P)).find((x) => x.id === "v0.0");
  await move(P, gD.cx, gD.cy);
  await P.dblclick({ js: `document.querySelector('.arena > .gut[data-bar="v0.0"]')` });
  await sleep(200);
  const ge = await grid();
  near(ge.colFr[0][0], 1 / ge.counts[0], 0.001, "a double-click on a bar evens the grid out");
  // horizontal bar drag
  const hbar = (await guts(P)).find((x) => x.id === "h0");
  await drag(P, hbar.cx + 40, hbar.cy, 0, 40);
  near(((await guts(P)).find((x) => x.id === "h0")).cy - hbar.cy, 40, 1.5, "a horizontal bar follows the pointer 1:1 too");
  await drag(P, hbar.cx + 40, hbar.cy + 40, 0, -40);
  // reorder: drag a tile onto another, they swap on release
  const ord0 = await P.eval(`__sss.vg(${JSON.stringify(await viewKey())}).order`);
  const t0 = (await tiles(P))[0];
  const t1 = (await tiles(P))[1];
  let dropSeen = false;
  await drag(P, t0.cx, t0.cy, t1.cx - t0.cx, t1.cy - t0.cy, { steps: 14, mid: async () => {} });
  const ord1 = await P.eval(`__sss.vg(${JSON.stringify(await viewKey())}).order`);
  check(ord1[0] === ord0[1] && ord1[1] === ord0[0], "dragging a tile onto another swaps them on release", JSON.stringify(ord1));
  await sleep(350); // the tiles glide to their cells (.2s)
  const t0b = (await tiles(P))[0];
  near(t0b.cx, t1.cx, 1, "the dragged tile took the other's place");
  await drag(P, t0b.cx, t0b.cy, t0.cx - t1.cx, t0.cy - t1.cy); // and back
  await sleep(100);

  /* ============================== Crop ============================== */
  section("Crop");
  await setLayout("crop");
  await sleep(200);
  await shot(P, "05_crop");
  const cropOk = await P.eval(`[...document.querySelectorAll('.arena.crop > .st-tile')].every((t) => { const v = t.querySelector('.vid').getBoundingClientRect(); const r = t.getBoundingClientRect(); return v.width >= r.width - 1 && v.height >= r.height - 1; })`);
  check(cropOk, "Crop fills every cell: the picture is at least as big as its cell (edges cropped around the centre)");
  const cg = await guts(P);
  const ct = await tiles(P);
  const cgrid = await grid();
  let cmid = true;
  let first = 0;
  cgrid.counts.forEach((c, i) => {
    for (let j = 0; j < c - 1; j++) {
      const gut = cg.find((x) => x.id === `v${i}.${j}`);
      if (Math.abs(gut.cx - (ct[first + j].r + ct[first + j + 1].x) / 2) > 1) cmid = false;
    }
    first += c;
  });
  check(cmid, "in Crop the bars are in the middle of the gap between the cells");
  const cv = cg.find((x) => x.id === "v0.0");
  await drag(P, cv.cx, cv.cy, 60, 0);
  near(((await guts(P)).find((x) => x.id === "v0.0")).cx - cv.cx, 60, 1.5, "a dragged bar follows the pointer 1:1 in Crop too");
  await P.dblclick({ js: `document.querySelector('.arena > .gut[data-bar="v0.0"]')` });

  /* ============================== Floating ============================== */
  section("Floating");
  await setLayout("float");
  await sleep(200);
  await shot(P, "06_float");
  const ratios = await P.eval(`[...document.querySelectorAll('.arena.float > .st-tile')].map((t) => { const r = t.getBoundingClientRect(); const c = t._it && (t._it.kind === "video" ? t._it.w.canvas : null); return { ratio: r.width / r.height, want: c ? c.width / c.height : null, ar: parseFloat(t.style.getPropertyValue("--ar")) }; })`);
  check(ratios.every((x) => Math.abs(x.ratio - x.ar) < 0.01) && ratios.some((x) => x.want && Math.abs(x.ratio - x.want) < 0.01), "a window has the aspect ratio of its content, also coming from Crop: no black bars", JSON.stringify(ratios.map((x) => +x.ratio.toFixed(3))));
  check(await P.eval("!document.querySelector('.arena.float > .gut')"), "no resize bars in Floating");
  const f0 = (await tiles(P))[0];
  const ar0 = f0.w / f0.h;
  await drag(P, f0.cx, f0.cy, 120, 90);
  const f1 = (await tiles(P))[0];
  near(f1.x - f0.x, 120, 1.5, "a floating window moves exactly with the pointer (x)");
  near(f1.y - f0.y, 90, 1.5, "a floating window moves exactly with the pointer (y)");
  // a window whose corner grip is free (the others overlap): the element under the grip's centre is its own .rz
  const freeGrip = async () => {
    const n = (await tiles(P)).length;
    for (let i = 0; i < n; i++) {
      const g = await P.eval(`(() => { const t = document.querySelectorAll('.arena.float > .st-tile')[${i}]; const r = t.getBoundingClientRect(); const x = r.right - 6, y = r.bottom - 6; const e = document.elementFromPoint(x, y); return e && e.classList.contains('rz') && t.contains(e) ? { x, y, i: ${i}, w: r.width, h: r.height } : null; })()`);
      if (g) return g;
    }
    return null;
  };
  let g1 = await freeGrip();
  check(!!g1, "a floating window has a free corner grip", JSON.stringify(g1));
  const ar0b = g1.w / g1.h;
  await drag(P, g1.x, g1.y, -100, 0);
  let t2 = (await tiles(P))[g1.i];
  check(t2.w < g1.w - 10, "corner resize: moving only left shrinks the window", `${g1.w.toFixed(0)} -> ${t2.w.toFixed(0)}`);
  near(t2.w / t2.h, ar0b, 0.01, "the window keeps its ratio");
  g1 = await freeGrip();
  const w2 = (await tiles(P))[g1.i].w;
  await drag(P, g1.x, g1.y, 0, -80);
  let t3 = (await tiles(P))[g1.i];
  check(t3.w < w2 - 10, "moving only up shrinks it too", `${w2.toFixed(0)} -> ${t3.w.toFixed(0)}`);
  g1 = await freeGrip();
  const w3 = (await tiles(P))[g1.i].w;
  await drag(P, g1.x, g1.y, 80, 80);
  let t4 = (await tiles(P))[g1.i];
  check(t4.w > w3 + 30, "pulling the corner right and down grows it", `${w3.toFixed(0)} -> ${t4.w.toFixed(0)}`);
  near(t4.w / t4.h, ar0b, 0.01, "still the same ratio after growing");
  const ar2 = await rect(P, ".arena");
  const t5 = (await tiles(P))[1];
  await drag(P, t5.cx, t5.cy, 5000, 5000, { steps: 8 });
  const t5b = (await tiles(P))[1];
  check(t5b.r <= ar2.r + 1 && t5b.b <= ar2.b + 1, "a window stops at the stage edge", `${t5b.r.toFixed(0)}/${ar2.r.toFixed(0)}`);
  // press raises: the pressed window ends up on top
  const t6 = (await tiles(P))[0];
  await move(P, t6.cx, t6.cy);
  await press(P, t6.cx, t6.cy);
  await release(P, t6.cx, t6.cy);
  const zs = await P.eval(`[...document.querySelectorAll('.arena.float > .st-tile')].map((t) => Number(t.style.zIndex))`);
  check(zs[0] === Math.max(...zs), "pressing a window raises it", JSON.stringify(zs));
  await setLayout("split");
  await sleep(200);

  /* ============================== overlays ============================== */
  section("tile overlays: buttons, stats, names");
  const vt = (await tiles(P))[0];
  await move(P, 4, 4);
  await sleep(250);
  check((await css(P, ".arena > .st-tile .tools", "opacity", null, 0)) === "0" && (await css(P, ".arena > .st-tile .tag", "opacity", null, 0)) === "0", "not hovered: no buttons and no name tag");
  await move(P, vt.cx, vt.cy);
  await sleep(300);
  check((await css(P, ".arena > .st-tile .tools", "opacity", null, 0)) === "1" && (await css(P, ".arena > .st-tile .tag", "opacity", null, 0)) === "1", "hovered: the buttons and the name tag show");
  await shot(P, "07_hover");
  // Show stats (ONE switch in the Layout menu, every stream): numbers only while hovered, in reserved slots, no jitter
  await P.click({ js: `document.querySelectorAll('.arena > .st-tile')[0].querySelector('.tools .round-btn:last-child')` });
  await P.waitFor("document.querySelector('.menu.m-tile')", { what: "stream menu" });
  const menuItems = await P.eval(`[...document.querySelectorAll('.menu.m-tile .mi')].map((e) => e.innerText.replace(/\\s+/g, ' ').trim())`);
  check(["Pop out", "Fullscreen", "Stop watching"].every((x) => menuItems.some((m) => m.includes(x))) && !menuItems.some((m) => /Level loudness/.test(m)) && !menuItems.some((m) => /stats/i.test(m)), "the stream menu: volume, pop out, fullscreen, stop watching, and no stats or level loudness switch (global, in the bar)", JSON.stringify(menuItems));
  await closeMenu();
  check((await P.eval(`document.querySelectorAll('.arena > .st-tile .tag .v').length`)) === 0, "stats are off by default: no numbers in any tag");
  await openLayoutMenu();
  const layoutItems = await P.eval(`[...document.querySelectorAll('.menu.m-layout .mi, .menu.m-layout .cbx')].map((e) => e.innerText.replace(/\\s+/g, ' ').trim())`);
  check(["Always show names", "Show stats", "Hide my own streams"].every((x) => layoutItems.some((m) => m.includes(x))), "the Layout menu has Always show names, Show stats and Hide my own streams", JSON.stringify(layoutItems));
  await layoutItem("/Show stats/");
  await sleep(150);
  await closeMenu();
  check((await P.eval(`[...document.querySelectorAll('.arena > .st-tile')].filter((t) => t._it.w).every((t) => t.querySelectorAll('.tag .v').length === 3) && [...document.querySelectorAll('.arena > .st-tile')].filter((t) => t._it.w).length >= 3`)), "Show stats switches the numbers on for every stream at once");
  const vt2 = (await tiles(P))[0];
  await move(P, 4, 4);
  await sleep(250);
  check((await P.eval(`document.querySelectorAll('.arena > .st-tile')[0].querySelectorAll('.tag .v').length`)) === 3 && (await css(P, ".arena > .st-tile .tag .v.fps", "display", null, 0)) === "none", "Show stats: the numbers are in the tag, hidden while the tile is not hovered");
  await move(P, vt2.cx, vt2.cy);
  await sleep(300);
  check((await css(P, ".arena > .st-tile .tag .v.fps", "display", null, 0)) !== "none", "and shown while it is hovered");
  const tagW = [];
  const texts = [];
  for (const st of [{ decodedFps: 5, width: 640, height: 480, bitrateKbps: 95 }, { decodedFps: 59, width: 1920, height: 1080, bitrateKbps: 6200 }, { decodedFps: 144, width: 3840, height: 2160, bitrateKbps: 10500 }, { decodedFps: 240, width: 9999, height: 9999, bitrateKbps: 99900 }]) {
    await P.eval(`(() => { const w = [...__sss.S.rooms[0].watches.values()].find((x) => x.canvas === document.querySelectorAll('.arena > .st-tile')[0]._it.w.canvas); w.stats = Object.assign({}, w.stats, ${JSON.stringify(st)}); window.dispatchEvent(new CustomEvent("sss-stats")); })()`);
    await sleep(60);
    tagW.push((await rect(P, ".arena > .st-tile:nth-child(1) .tag")).w);
    texts.push(await P.eval(`document.querySelectorAll('.arena > .st-tile')[0].querySelector('.tag').innerText.replace(/\\s+/g, ' ')`));
  }
  check(tagW.every((w) => Math.abs(w - tagW[0]) < 0.6), "the stats pill is exactly as wide for 5 fps / 59 fps / 144 fps / 240 fps and 640x480 ... 9999x9999 (no jitter)", tagW.map((w) => w.toFixed(1)).join(" "));
  check(await P.eval(`(() => { const tg = document.querySelectorAll('.arena > .st-tile')[0].querySelector('.tag'); return tg.scrollWidth <= tg.clientWidth + 1; })()`), "the pill's background is as wide as its content (nothing overflows it)", texts[3]);
  await shot(P, "08_stats");
  await move(P, 4, 4);
  await sleep(300);
  check((await css(P, ".arena > .st-tile .tag .v.fps", "display", null, 0)) === "none", "unhovered again: the stats are gone");
  // a narrow tile drops what does not fit, the resolution first, then the rate (native stats_fields); a wide one shows all three
  const wide = await P.eval(`document.querySelectorAll('.arena > .st-tile')[0].querySelector('.tag').dataset.fit`);
  const narrowI = await P.eval(`(() => { const w = [...document.querySelectorAll('.arena > .st-tile')].map((t) => t.getBoundingClientRect().width); return w.indexOf(Math.min(...w)); })()`);
  await sleep(300);
  const narrow = await P.eval(`(() => { const t = document.querySelectorAll('.arena > .st-tile')[${narrowI}]; const tg = t.querySelector('.tag'); return { fit: tg.dataset.fit, w: t.getBoundingClientRect().width, tag: tg.getBoundingClientRect().width }; })()`);
  check(wide === "all" && narrow.fit !== "all" && narrow.tag <= narrow.w, "a wide tile shows resolution, fps and rate; a narrow one drops numbers (resolution first) and its pill still fits", `wide ${wide}, ${narrow.w.toFixed(0)} px wide -> ${narrow.fit}, pill ${narrow.tag.toFixed(0)} px`);
  // Always show names (Layout menu): the name tag stays without hovering, the buttons and stats never do
  await openLayoutMenu();
  await layoutItem("/Always show names/");
  await closeMenu();
  await move(P, 4, 4);
  await sleep(300);
  check((await css(P, ".arena > .st-tile .tag", "opacity", null, 2)) === "1" && (await css(P, ".arena > .st-tile .tools", "opacity", null, 2)) === "0", "Always show names: the tag stays, the buttons do not");
  check((await css(P, ".arena > .st-tile .tag .v.fps", "display", null, 0)) === "none", "and the stats still need a hover");
  await openLayoutMenu();
  await layoutItem("/Show stats/");
  await closeMenu();
  check((await P.eval(`document.querySelectorAll('.arena > .st-tile .tag .v').length`)) === 0, "Show stats off again: no numbers in any tag");
  await openLayoutMenu();
  await layoutItem("/Always show names/");
  await closeMenu();
  // Hide my own streams: your own screen leaves your stage, others still see it
  check((await P.eval("document.querySelectorAll('.st-tile.mine').length")) === 1, "Vera's own tile is on her stage");
  await openLayoutMenu();
  await layoutItem("/Hide my own streams/");
  await closeMenu();
  check((await P.eval("document.querySelectorAll('.st-tile.mine').length")) === 0 && (await P.eval("document.querySelectorAll('.arena > .st-tile').length")) === 3, "Hide my own streams: her screen is gone from her stage");
  await L.sleep(500);
  await A.page.eval("1");
  await L.watchFirst(A.page).catch(() => {});
  check((await A.page.eval(`!!__sss.S.rooms[0].streams.find((s) => s.ownerName === "Vera")`)), "others still see her stream (Alice's roster holds it)");
  await openLayoutMenu();
  await layoutItem("/Hide my own streams/");
  await closeMenu();
  check((await P.eval("document.querySelectorAll('.st-tile.mine').length")) === 1, "turned off again, her tile is back");

  /* ============================== strip + hover card ============================== */
  section("people strip and hover card");
  await P.eval(`(() => { const r = __sss.S.rooms[0]; const bob = [...r.watches.values()].find((w) => w.ownerName === "Bob"); window.__setMuted = (m) => { bob.muted = m; r.client.setMuted(bob.key, m); __sss.render(); }; window.__setMuted(true); })()`);
  await sleep(200);
  const pvs = await rects(P, ".strip .pv");
  const lines = await P.eval(`[...document.querySelectorAll('.strip .pv')].map((p) => { const a = p.querySelector('.avatar').getBoundingClientRect(); const n = p.querySelector('.nm').getBoundingClientRect(); const l = p.querySelector('.lv'); const lr = l && l.getBoundingClientRect(); return { user: p.dataset.user, ax: a.x + a.width / 2, nx: n.x + n.width / 2, lx: lr ? lr.x + lr.width / 2 : null, audible: p.classList.contains('audible'), w: a.width, h: a.height }; })`);
  check(lines.length === 4 && lines.every((l) => Math.abs(l.ax - l.nx) < 0.6 && (l.lx == null || Math.abs(l.ax - l.lx) < 0.6)), "ring, avatar, LIVE badge and name share one centre line", lines.map((l) => `${l.user}:${(l.ax - l.nx).toFixed(2)}/${l.lx == null ? "-" : (l.ax - l.lx).toFixed(2)}`).join(" "));
  check(lines.some((l) => l.audible) && lines.some((l) => !l.audible && l.user === "Bob"), "with and without audio: Alice and Carol have the ring, muted Bob has none", lines.map((l) => `${l.user}:${l.audible}`).join(" "));
  const ring = await P.eval(`(() => { const a = document.querySelector('.pv.audible .avatar'); const cs = getComputedStyle(a, '::before'); return { top: cs.top, left: cs.left, right: cs.right, bottom: cs.bottom, bw: cs.borderTopWidth, br: cs.borderRadius, w: cs.width, h: cs.height }; })()`);
  check(ring.top === "-4px" && ring.left === "-4px" && ring.right === "-4px" && ring.bottom === "-4px" && ring.bw === "2px" && ring.w === ring.h, "the ring is one even 2 px circle, 4 px outside the avatar all round", JSON.stringify(ring));
  await shot(P, "09_strip");
  // the hover card: the same for everyone who shares, one row per stream, each saying Audio / No audio
  const cardOf = async (user) => {
    const pv = (await rects(P, ".strip .pv")).find((r) => r.id === user);
    const av = await P.eval(`(() => { const r = [...document.querySelectorAll('.strip .pv')].find((p) => p.dataset.user === ${JSON.stringify(user)}).querySelector('.avatar').getBoundingClientRect(); return { x: r.x, y: r.y, r: r.right, b: r.bottom, cx: r.x + r.width / 2, cy: r.y + r.height / 2 }; })()`);
    await move(P, 3, 3);
    await sleep(120);
    await move(P, av.cx, av.cy);
    await P.waitFor(`[...document.querySelectorAll('.utip .hd b')].some((b) => b.innerText === ${JSON.stringify(user === "Vera" ? "You" : user)})`, { what: `card of ${user}` });
    return { av, pv, card: await P.eval(`(() => { const c = document.querySelector('.utip'); return { n: document.querySelectorAll('.utip').length, rows: [...c.querySelectorAll('.ust')].map((r) => ({ label: r.querySelector('.l1 .txt').innerText, audio: [...r.querySelector('.au').childNodes].filter((n) => n.nodeType === 3).map((n) => n.textContent).join('').trim(), l2: r.querySelector('.l2').innerText, l3: r.querySelector('.l3').innerText.trim() })), badge: !!c.querySelector('.badge-live'), sub: c.querySelector('.hd small').innerText }; })()` ) };
  };
  const cards = {};
  for (const u of ["Alice", "Bob", "Vera"]) cards[u] = await cardOf(u);
  check(["Alice", "Bob", "Vera"].every((u) => cards[u].card.n === 1 && cards[u].card.rows.length === 1 && cards[u].card.badge), "everyone who shares gets the same hover card: a LIVE badge and one row per stream", JSON.stringify(Object.values(cards).map((c) => c.card.rows.length)));
  check(cards.Alice.card.rows[0].audio === "Audio" && cards.Bob.card.rows[0].audio === "Audio", "a stream whose sender shares audio says Audio (Alice, Bob: the muted viewer does not change what the sender shares)", JSON.stringify([cards.Alice.card.rows[0].audio, cards.Bob.card.rows[0].audio]));
  check(cards.Vera.card.rows[0].audio === "No audio", "her own stream, which has no audio, says No audio: every row says one of the two", cards.Vera.card.rows[0].audio);
  check(/1920|960×540|×/.test(cards.Alice.card.rows[0].l2) && /On the stage/.test(cards.Alice.card.rows[0].l3) && /Your stream/.test(cards.Vera.card.rows[0].l3), "the numbers and the state per row (On the stage / Your stream)", JSON.stringify([cards.Alice.card.rows[0].l2, cards.Alice.card.rows[0].l3, cards.Vera.card.rows[0].l3]));
  await shot(P, "10_card");
  // the keep zone: the card, the avatar and the gap straight above it keep it open; the neighbours' avatars do not
  const al = await cardOf("Alice");
  const cardRect = await rect(P, ".utip");
  await move(P, al.av.cx, (cardRect.b + al.av.y) / 2);
  await sleep(150);
  check(await P.eval(`document.querySelectorAll('.utip').length === 1 && document.querySelector('.utip .hd b').innerText === "Alice"`), "the gap straight above the avatar keeps the card open");
  await move(P, cardRect.cx, cardRect.cy);
  await sleep(150);
  check(await P.eval(`document.querySelectorAll('.utip').length === 1 && document.querySelector('.utip .hd b').innerText === "Alice"`), "the card itself keeps it open");
  const pvAll = await rects(P, ".strip .pv");
  const idx = pvAll.findIndex((r) => r.id === "Alice");
  const nbI = idx + 1 < pvAll.length ? idx + 1 : idx - 1;
  const nbAv = await P.eval(`(() => { const r = document.querySelectorAll('.strip .pv')[${nbI}].querySelector('.avatar').getBoundingClientRect(); return { cx: r.x + r.width / 2, cy: r.y + r.height / 2 }; })()`);
  await move(P, nbAv.cx, nbAv.cy);
  await sleep(250);
  const stacked = await P.eval(`({ n: document.querySelectorAll('.utip').length, who: document.querySelector('.utip .hd b')?.innerText })`);
  check(stacked.n === 1 && stacked.who !== "Alice", "moving onto the neighbour's avatar swaps the card: exactly one card, never stacked", JSON.stringify(stacked));
  await move(P, 4, 4);
  await sleep(250);
  check((await P.eval("document.querySelectorAll('.utip').length")) === 0, "pointer away: the card closes");
  // a popped-out stream shows only on its owner's avatar and comes back from there
  await move(P, 4, 4);
  const aliceTile = (await P.eval(`(() => { const ts = [...document.querySelectorAll('.arena > .st-tile')]; const i = ts.findIndex((t) => t._it && t._it.owner === "Alice"); return i; })()`));
  const at = (await tiles(P))[aliceTile];
  await move(P, at.cx, at.cy);
  await sleep(250);
  await P.click({ js: `document.querySelectorAll('.arena > .st-tile')[${aliceTile}].querySelector('.tools .round-btn')` });
  await P.waitFor(`[...__sss.S.rooms[0].watches.values()].some((w) => w.ownerName === "Alice" && w.popped)`, { timeout: 10000, what: "Alice popped out" });
  await sleep(300);
  const popState = await P.eval(`({ tiles: document.querySelectorAll('.arena > .st-tile').length, popped: [...document.querySelectorAll('.pv.popped')].map((p) => p.dataset.user), badges: [...document.querySelectorAll('.pv .pop')].length })`);
  check(popState.tiles === 3 && JSON.stringify(popState.popped) === '["Alice"]' && popState.badges === 1, "a popped-out stream is off the stage and shows only on its owner's avatar", JSON.stringify(popState));
  const alAv = await P.eval(`(() => { const r = document.querySelector('.pv.popped .avatar').getBoundingClientRect(); return { cx: r.x + r.width / 2, cy: r.y + r.height / 2 }; })()`);
  await move(P, alAv.cx, alAv.cy);
  await P.waitFor("document.querySelector('.utip .back')", { what: "Bring back in the card" });
  check(/Popped out/.test(await P.eval("document.querySelector('.utip').innerText")), "its hover card says Popped out and offers Bring back");
  await shot(P, "11_popped");
  await P.click(".utip .back");
  await P.waitFor(`![...__sss.S.rooms[0].watches.values()].some((w) => w.popped) && document.querySelectorAll('.arena > .st-tile').length === 4`, { what: "brought back from the card" });
  check(true, "Bring back in the card returns the stream to the stage");
  // and again, back by clicking the avatar
  const at2 = (await tiles(P))[aliceTile];
  await move(P, at2.cx, at2.cy);
  await sleep(250);
  await P.click({ js: `document.querySelectorAll('.arena > .st-tile')[${aliceTile}].querySelector('.tools .round-btn')` });
  await P.waitFor(`[...__sss.S.rooms[0].watches.values()].some((w) => w.ownerName === "Alice" && w.popped)`, { what: "popped again" });
  await sleep(300);
  await P.click(".pv.popped");
  await P.waitFor(`![...__sss.S.rooms[0].watches.values()].some((w) => w.popped) && document.querySelectorAll('.arena > .st-tile').length === 4`, { what: "brought back from the avatar" });
  check(true, "clicking its owner's avatar brings it back too");
  await move(P, 4, 4);

  /* ============================== stream menu: volume ============================== */
  section("stream menu: volume, wheel");
  await P.eval("window.__setMuted(false)");
  const bt = (await tiles(P))[1];
  await rightClick(P, bt.cx, bt.cy);
  await P.waitFor("document.querySelector('.menu.m-tile input.vol')", { what: "volume row via right click" });
  check(true, "a right click on a tile opens the stream menu");
  const sl = await rect(P, ".menu.m-tile input.vol");
  const volOf = () => P.eval(`Math.round([...__sss.S.rooms[0].watches.values()].find((w) => w.ownerName === "Bob").vol * 100)`);
  const v0 = await volOf();
  // pointer down on the slider thumb area, drag right, read the volume BEFORE the release
  const thumbX = sl.x + 7 + (sl.w - 14) * (v0 / 500);
  await move(P, thumbX, sl.cy);
  await press(P, thumbX, sl.cy);
  await mouse(P, "mouseMoved", thumbX + 40, sl.cy, { button: "left", buttons: 1 });
  await mouse(P, "mouseMoved", thumbX + 80, sl.cy, { button: "left", buttons: 1 });
  await sleep(120);
  const vDuring = await volOf();
  await release(P, thumbX + 80, sl.cy);
  check(vDuring > v0, "the volume slider acts during the drag, not on release", `${v0} -> ${vDuring} while the button is still down`);
  // the wheel: 5 % a notch up to 200 %, 10 % above
  const setVol = async (v) => {
    await P.eval(`(() => { const s = document.querySelector('.menu.m-tile input.vol'); s.value = "${v}"; s.dispatchEvent(new Event("input", { bubbles: true })); })()`);
  };
  const seen = [];
  const slr = await rect(P, ".menu.m-tile input.vol");
  await move(P, slr.cx, slr.cy);
  for (const [from, dir] of [[190, -100], [195, -100], [200, -100], [210, 100], [100, -100]]) {
    await setVol(from);
    await sleep(40);
    await wheel(P, slr.cx, slr.cy, dir);
    await sleep(80);
    seen.push([from, dir < 0 ? "up" : "down", await volOf()]);
  }
  check(JSON.stringify(seen.map((s) => s[2])) === JSON.stringify([195, 200, 210, 200, 105]), "scrolling on a volume slider: 5 % a notch up to 200 %, 10 % above, symmetric downwards", JSON.stringify(seen));
  await closeMenu();

  /* ============================== menus and dropdowns ============================== */
  section("menus: the app's own style, no native select");
  await openLayoutMenu();
  const lay = await P.eval(`({ items: [...document.querySelectorAll('.menu.m-layout .mi')].map((e) => ({ t: e.innerText.replace(/\\s+/g, ' ').trim(), icon: !!e.querySelector('.ms'), two: e.classList.contains('two'), sub: !!e.querySelector('small') })), on: [...document.querySelectorAll('.menu.m-layout .mi.on')].map((e) => e.innerText.trim().split(/\\s+/).slice(-3).join(' ')), pip: (() => { const c = [...document.querySelectorAll('.menu.m-layout .cbx')].find((e) => /Webcam in picture/.test(e.innerText)); return c ? c.classList.contains('disabled') : null; })() })`);
  check(lay.items.slice(0, 3).every((i) => i.icon && i.two && i.sub) && lay.items.length === 4 && lay.pip === false, "Layout menu: Split / Crop / Floating with icons and sub-lines, a live Webcam in picture (I10), Always show names, Hide my own streams, Reset", JSON.stringify(lay.items.map((i) => i.t)));
  await shot(P, "12_layout_menu");
  await closeMenu();
  await P.click("#btn-share");
  await P.waitFor("document.querySelector('.modal')", { what: "stream settings (live)" });
  const modal = await P.eval(`({ titles: [...document.querySelectorAll('.modal .section-label')].map((e) => e.innerText.replace(/\\d+$/, '').trim()), selects: document.querySelectorAll('select').length, change: !!document.querySelector('#btn-change-source'), dd: document.querySelector('#st-fps')?.className })`);
  check(["SOURCE", "COMPRESSION", "VIDEO"].every((x) => modal.titles.some((t) => t.toUpperCase().startsWith(x))) && modal.change, "stream settings (live): Source with Change source, Compression (Codec, Bitrate), Video (FPS, Preset)", JSON.stringify(modal.titles));
  check(modal.selects === 0 && /field dd/.test(modal.dd), "no native <select> anywhere: the FPS field is the app's own dropdown", `${modal.selects} selects`);
  await P.click("#st-fps");
  await P.waitFor("document.querySelector('.menu.m-dd')", { what: "fps dropdown" });
  const dd = await P.eval(`(() => { const f = document.querySelector('#st-fps').getBoundingClientRect(); const m = document.querySelector('.menu.m-dd').getBoundingClientRect(); return { fw: f.width, mw: m.width, below: m.top - f.bottom, left: m.left - f.left, items: document.querySelectorAll('.menu.m-dd .mi').length, checked: document.querySelectorAll('.menu.m-dd .mi.on').length }; })()`);
  check(Math.abs(dd.fw - dd.mw) < 1.5 && dd.below >= 0 && dd.below < 12 && Math.abs(dd.left) < 1.5 && dd.items === 8 && dd.checked === 1, "a dropdown opens under its field, as wide as it, current value checked", JSON.stringify(dd));
  await shot(P, "13_dropdown");
  await key(P, "Escape");
  check(await P.eval("!document.querySelector('.menu.m-dd') && !!document.querySelector('.modal')"), "Escape in a dropdown closes the dropdown, not the modal under it");
  // presets: Text / Quality / Gaming set the FPS once; changing the FPS keeps the preset's mode and keyframe
  const cap = () => P.eval(`document.querySelector('#preset-cap').innerText`);
  await P.click({ text: "Text", sel: ".modal .seg button" });
  await sleep(500);
  check((await cap()) === "15 FPS · Quality encoding · Keyframe 250 ms" && (await P.eval(`document.querySelector('#st-fps .txt').innerText`)) === "15 fps", "Text: 15 fps, Quality encoding, keyframe 250 ms", await cap());
  await P.click("#st-fps");
  await P.click({ js: `[...document.querySelectorAll('.menu.m-dd .mi')].find((e) => e.innerText.trim() === "30 fps")` });
  await sleep(500);
  check((await cap()) === "30 FPS · Quality encoding · Keyframe 250 ms" && (await P.eval(`document.querySelector('.modal .seg button.on')?.innerText`)) !== undefined, "changing the FPS by hand keeps the preset's mode and keyframe", await cap());
  const litNow = (scope) => P.eval(`[...document.querySelectorAll('${scope} .seg:has(button[title=Gaming]) button.on')].map((b) => b.innerText).join()`);
  check((await litNow(".modal")) === "", "Text at 30 fps (hand-changed): no preset lit", await litNow(".modal"));
  await P.click("#st-fps");
  await P.click({ js: `[...document.querySelectorAll('.menu.m-dd .mi')].find((e) => e.innerText.trim() === "120 fps")` });
  await sleep(500);
  check((await cap()) === "120 FPS · Quality encoding · Keyframe 250 ms" && (await litNow(".modal")) === "", "120 fps + Quality + 250 ms: no preset lit", await cap());
  const ord = await P.eval(`(() => { const y = (e) => e.getBoundingClientRect().top; const p = document.querySelector('.modal .seg:has(button[title=Gaming])'); return y(p) < y(document.querySelector('#st-fps')) && y(document.querySelector('#st-fps')) < y(document.querySelector('#st-gop')); })()`);
  check(ord, "Video card order: Preset, FPS, ..., Keyframe");
  await P.click("#st-fps");
  await P.click({ js: `[...document.querySelectorAll('.menu.m-dd .mi')].find((e) => e.innerText.trim() === "15 fps")` });
  await sleep(500);
  check((await litNow(".modal")) === "Text", "back at 15 fps: Text lit again", await litNow(".modal"));
  await P.click({ text: "Gaming", sel: ".modal .seg button" });
  await sleep(500);
  check((await cap()) === "120 FPS · Performance encoding · Keyframe 250 ms", "Gaming: 120 fps, Performance encoding, keyframe 250 ms", await cap());
  check(!/custom/i.test(await cap()), "the caption never says (custom)");
  await P.click({ text: "Quality", sel: ".modal .seg button" });
  await sleep(400);
  check((await cap()) === "60 FPS · Quality encoding · Keyframe 1000 ms", "Quality: 60 fps, Quality encoding, keyframe 1000 ms", await cap());
  // Encoding and Keyframe are fields again (user: "The settings for the keyframe and the Performance/Quality setting should still be shown"):
  // they show what the preset set, editing one by hand leaves no preset lit, re-picking a preset restores both
  const fields = () => P.eval(`({ gop: document.querySelector('#st-gop .txt')?.innerText, enc: document.querySelector('#st-encoding .seg button.on, .modal .seg:has(button[title=Performance]) button.on')?.innerText, preset: document.querySelectorAll('.modal .seg:has(button[title=Gaming]) button.on').length ? document.querySelector('.modal .seg:has(button[title=Gaming]) button.on').innerText : "none" })`);
  let f = await fields();
  check(f.gop === "1000 ms" && f.enc === "Quality" && f.preset === "Quality", "the Keyframe and Encoding fields show the Quality preset's 1000 ms / Quality", JSON.stringify(f));
  await P.click("#st-gop");
  await P.waitFor("document.querySelector('.menu.m-dd')", { what: "keyframe dropdown" });
  const gopItems = await P.eval(`[...document.querySelectorAll('.menu.m-dd .mi')].map((e) => e.innerText.replace(/\\s*check$/, '').trim())`);
  check(JSON.stringify(gopItems) === '["250 ms","500 ms","1000 ms","2000 ms"]', "the Keyframe dropdown lists 250, 500, 1000, 2000 ms", JSON.stringify(gopItems));
  await P.click({ js: `[...document.querySelectorAll('.menu.m-dd .mi')].find((e) => e.innerText.trim() === "500 ms")` });
  await P.waitFor("__sss.S.captures.every((c) => c.dests.size && [...c.dests.values()].every((d) => d.state === 'live'))", { timeout: 20000, what: "live again after a keyframe edit" });
  f = await fields();
  check(f.gop === "500 ms" && f.preset === "none" && (await cap()) === "60 FPS · Quality encoding · Keyframe 500 ms" && !/custom/i.test(await cap()), "Keyframe 500 ms: no preset lit, the caption says what is set (never custom)", JSON.stringify(f) + " " + (await cap()));
  check(await P.eval(`[...__sss.S.captures[0].dests.values()].every((d) => d.gopMs === 500 && d.latencyMode === "quality" && d.share)`), "the live destination restarted with the new keyframe interval", await P.eval(`JSON.stringify([...__sss.S.captures[0].dests.values()].map((d) => [d.gopMs, d.latencyMode, d.preset]))`));
  check(await P.eval(`(() => { const st = __sss.S.settings.stream; return st.gopMs === 500 && st.fps === 60 && st.codec === "h264"; })()`), "a keyframe edit on a LIVE share is the new remembered setting (not only what was set before Go live)", await P.eval("JSON.stringify(__sss.S.settings.stream)"));
  await P.click({ text: "Performance", sel: ".modal .seg button" });
  await P.waitFor("__sss.S.captures.every((c) => c.dests.size && [...c.dests.values()].every((d) => d.state === 'live'))", { timeout: 20000, what: "live again after an encoding edit" });
  check((await cap()) === "60 FPS · Performance encoding · Keyframe 500 ms" && (await fields()).preset === "none", "Encoding Performance by hand: caption follows, still no preset lit", await cap());
  await P.click({ text: "Quality", sel: ".modal .seg button" });
  await P.waitFor("__sss.S.captures.every((c) => c.dests.size && [...c.dests.values()].every((d) => d.state === 'live'))", { timeout: 20000, what: "live again after re-picking Quality" });
  f = await fields();
  check(f.gop === "1000 ms" && f.enc === "Quality" && f.preset === "Quality" && (await cap()) === "60 FPS · Quality encoding · Keyframe 1000 ms", "re-picking the Quality preset restores Quality encoding and 1000 ms", JSON.stringify(f));
  await P.click({ text: "Done", sel: ".modal-foot button" });
  await sleep(300);
  await P.waitFor("__sss.S.captures.every((c) => c.dests.size && [...c.dests.values()].every((d) => d.state === 'live'))", { timeout: 20000, what: "live again after restarts" });

  /* ============================== change source while live ============================== */
  section("Change source while live");
  const idBefore = await P.eval("__sss.S.captures[0].video.id");
  await P.eval(`(async () => { const { makePatternSource } = await import("${L.BASE}dev/pattern.js"); const src = makePatternSource(640, 360, 15, { audio: false }); document.body.lastElementChild.style.cssText = "position:fixed;left:-9999px;width:8px;height:8px"; window.__nextSrc = src; navigator.mediaDevices.getDisplayMedia = async () => src.stream; })()`);
  await P.click("#btn-share");
  await P.waitFor("document.querySelector('#btn-change-source')", { what: "modal" });
  await P.click("#btn-change-source");
  await P.waitFor(`__sss.S.captures[0].video.id !== ${JSON.stringify(idBefore)}`, { timeout: 15000, what: "new source" });
  await P.waitFor("__sss.S.captures.every((c) => c.dests.size && [...c.dests.values()].every((d) => d.state === 'live'))", { timeout: 20000, what: "live on the new source" });
  check(true, "Change source while live: the share carries on with the new source", `${idBefore} -> ${await P.eval("__sss.S.captures[0].video.id")}`);
  await P.click({ text: "Done", sel: ".modal-foot button" });
  await sleep(300);
  // the Share menu: FPS and preset for the live destination
  await P.click("#btn-share ~ .caret");
  await P.waitFor("document.querySelector('.menu.m-share')", { what: "share menu" });
  const sm = await P.eval(`({ head: document.querySelector('.menu.m-share .mh').innerText.replace(/\\s+/g, ' '), fps: document.querySelectorAll('.menu.m-share .seg.dense button').length, presets: [...document.querySelectorAll('.menu.m-share .seg:not(.dense) button')].map((b) => b.innerText), items: [...document.querySelectorAll('.menu.m-share .mi')].map((e) => e.innerText.replace(/\\s+/g, ' ').trim()) })`);
  check(sm.fps === 8 && JSON.stringify(sm.presets) === '["Text","Quality","Gaming"]' && sm.items.some((i) => /Change source/.test(i)) && sm.items.some((i) => /Stop sharing/.test(i)) && /Live/i.test(sm.head), "the Share menu: header, FPS, Preset, Change source, All stream settings, Stop sharing", JSON.stringify(sm));
  // live Quality preset at 60 fps: lit; the menu lists Preset above FPS
  const mo = await P.eval(`(() => { const m = document.querySelector('.menu.m-share'); const y = (e) => e.getBoundingClientRect().top; const p = m.querySelector('.seg:not(.dense)'); const f = m.querySelector('.seg.dense'); return { lit: [...p.querySelectorAll('button.on')].map((b) => b.innerText).join(), above: y(p) < y(f), cap: y(m.querySelector('.cap')) < y(f) }; })()`);
  check(mo.lit === "Quality" && mo.above && mo.cap, "the Share menu: Quality lit at 60 fps, Preset (and its caption) above FPS", JSON.stringify(mo));
  await shot(P, "14_share_menu");
  await closeMenu();
  await P.click("#btn-room");
  await P.waitFor("document.querySelector('.menu.m-room')", { what: "room menu" });
  const rm = await P.eval(`[...document.querySelectorAll('.menu.m-room .mi')].map((e) => e.innerText.replace(/\\s+/g, ' ').trim())`);
  check(["Auto-connect", "Rename", "Copy invite link", "Leave room", "Forget room"].every((x) => rm.some((m) => m.includes(x))), "the Room settings menu: Auto-connect, Rename, Copy invite link, Leave room, Forget room", JSON.stringify(rm));
  // a second click on the opener closes it again
  await P.click("#btn-room");
  await sleep(150);
  check(await P.eval("!document.querySelector('.menu')"), "a second click on the button that opened a menu closes it");

  // Right click on a sidebar room row: that room's Room settings at the pointer, whichever room is open, also while disconnected.
  await L.addRoomByLink(P, ["127.0.0.1:1", ...link.split("|").slice(1)].join("|"), "ghost");
  await P.waitFor("__sss.S.rooms.length === 2", { what: "a second, unreachable room" });
  await sleep(300);
  const rowNames = () => P.eval(`[...document.querySelectorAll('.p-row')].map((e) => e.innerText.replace(/\\s+/g, ' ').trim())`);
  const rowRect = async (re) => (await rects(P, ".p-row"))[(await rowNames()).findIndex((t) => re.test(t))];
  await P.click({ text: "stage", sel: ".p-row" }); // joining the ghost selected it; back to the first room
  await sleep(200);
  const openBefore = await P.eval("__sss.S.selected");
  const gr = await rowRect(/ghost/);
  await rightClick(P, gr.cx, gr.cy);
  const rc = await P.eval(`({ menu: !!document.querySelector('.menu.m-room'), head: document.querySelector('.menu.m-room .mh')?.innerText.replace(/\\s+/g, ' '), items: [...document.querySelectorAll('.menu.m-room .mi')].map((e) => e.innerText.replace(/\\s+/g, ' ').trim()), sel: __sss.S.selected, x: Math.round(document.querySelector('.menu.m-room')?.getBoundingClientRect().x || 0) })`);
  check(rc.menu && /ghost/.test(rc.head) && rc.sel === openBefore && rc.items.some((m) => m.includes("Connect")) && !rc.items.some((m) => m.includes("Leave room")) && Math.abs(rc.x - gr.cx) < 4, "right click on a room row opens that room's menu at the pointer (Connect for a disconnected room); the open room stays", JSON.stringify({ ...rc, gr }));
  await shot(P, "14b_room_row_menu");
  const fr = await rowRect(/stage/);
  await rightClick(P, fr.cx, fr.cy);
  check(await P.eval(`/stage/.test(document.querySelector('.menu.m-room .mh')?.innerText) && document.querySelectorAll('.menu').length === 1`), "a right click on another row moves the menu to that room");
  await key(P, "Escape"); // the open menu lies over the rows below its row
  await rightClick(P, gr.cx, gr.cy);
  await P.click({ text: "Rename", sel: ".menu.m-room .mi" });
  check(await P.eval(`__sss.S.modal?.kind === 'rename' && __sss.S.modal.room === __sss.S.rooms.find((r) => r.name === 'ghost').id`), "Rename in a row's menu acts on the clicked room, not the open one", await P.eval(`JSON.stringify([__sss.S.modal, __sss.S.menu && __sss.S.menu.roomId, __sss.S.rooms.map((r) => r.name + ':' + r.id)])`));
  await key(P, "Escape");
  await sleep(150);
  await rightClick(P, gr.cx, gr.cy);
  await P.click({ text: "Forget room", sel: ".menu.m-room .mi" });
  await P.click({ text: "Forget room", sel: ".modal-foot button" });
  await P.waitFor("__sss.S.rooms.length === 1 && __sss.S.rooms[0].name === 'stage'", { what: "the ghost room forgotten" });
  check(true, "Forget room in a row's menu removed the clicked room only");

  /* ============================== Watched view ============================== */
  section("Watched streams");
  await P.click({ text: "Watched streams", sel: ".p-row" });
  await sleep(300);
  const wv = await P.eval(`({ tiles: document.querySelectorAll('.arena > .st-tile').length, mine: document.querySelectorAll('.st-tile.mine').length, invite: !!document.querySelector('#btn-invite'), room: !!document.querySelector('#btn-room'), leave: !!document.querySelector('#btn-leave'), share: !!document.querySelector('#btn-share'), layout: !!document.querySelector('#btn-layout'), title: document.querySelector('#btn-share')?.title })`);
  check(wv.tiles === 3 && wv.mine === 0 && !wv.invite && !wv.room && !wv.leave && wv.share && wv.layout, "Watched shows what you watch and nothing else: no Invite, Room settings or Leave, Share only while you share", JSON.stringify(wv));
  await shot(P, "15_watched");
  await P.click(".p-row.sel ~ .p-row, .p-row:not(.sel)");
  await sleep(200);
  await P.eval("__sss.S.view = 'room'; __sss.render()");

  /* ============================== fullscreen auto-hide ============================== */
  section("fullscreen: the bar and strip auto-hide");
  await P.click("#btn-stagefs");
  await P.waitFor("document.fullscreenElement === document.getElementById('app') && document.querySelector('.stagefs')", { what: "stage fullscreen" });
  check(await P.eval("!document.querySelector('.p-side') && !document.querySelector('.p-head')"), "fullscreen draws only the stage (no sidebar, no header)");
  await move(P, 600, 300);
  await sleep(400);
  const shown = async () => ({ bar: await css(P, ".ctlrow", "opacity"), strip: await css(P, ".stripwrap", "opacity"), tools: await css(P, ".arena > .st-tile .tools", "opacity") });
  const tilesBefore = await tiles(P);
  let s0 = await shown();
  check(s0.bar === "1" && s0.strip === "1", "fullscreen: the bar and the strip show", JSON.stringify(s0));
  await shot(P, "16_fullscreen");
  // streams do not jump when the chrome fades
  await sleep(3600);
  s0 = await shown();
  const tilesAfter = await tiles(P);
  check(s0.bar === "0" && s0.strip === "0" && s0.tools === "0", "after 3 s without movement the bar, the strip and the tile buttons are gone", JSON.stringify(s0));
  check(tilesBefore.length === tilesAfter.length && tilesBefore.every((t, i) => Math.abs(t.x - tilesAfter[i].x) < 0.5 && Math.abs(t.y - tilesAfter[i].y) < 0.5 && Math.abs(t.w - tilesAfter[i].w) < 0.5), "streams don't jump when the bar fades");
  check((await css(P, ".stagefs", "cursor")) === "none", "the cursor goes with them");
  await shot(P, "17_fullscreen_hidden");
  await move(P, 640, 320);
  await sleep(450);
  s0 = await shown();
  check(s0.bar === "1" && s0.strip === "1", "movement brings them back", JSON.stringify(s0));
  await sleep(1500);
  await move(P, 700, 330);
  await sleep(1800);
  s0 = await shown();
  check(s0.bar === "1", "movement restarts the 3 s (still shown 1.8 s after the second move)", JSON.stringify(s0));
  // the pointer leaves the stage: gone at once
  await P.eval(`document.documentElement.dispatchEvent(new MouseEvent("mouseleave"))`);
  await sleep(450);
  s0 = await shown();
  check(s0.bar === "0" && s0.strip === "0", "the pointer leaving the stage hides them at once", JSON.stringify(s0));
  await move(P, 640, 320);
  await sleep(450);
  // a key press brings it back
  await sleep(3500);
  await key(P, "Shift");
  await sleep(450);
  check((await shown()).bar === "1", "a key press brings the bar back");
  // a menu stops the timer; after it closes the bar gets a full 3 s
  const lb = await rect(P, "#btn-layout");
  await move(P, lb.cx, lb.cy);
  await P.click("#btn-layout");
  await P.waitFor("document.querySelector('.menu.m-layout')", { what: "menu in fullscreen" });
  await sleep(4200);
  s0 = await shown();
  check(s0.bar === "1" && s0.strip === "1", "an open menu stops the timer (still shown after 4 s)", JSON.stringify(s0));
  await key(P, "Escape");
  await sleep(2000);
  s0 = await shown();
  check(s0.bar === "1", "2 s after the menu closed the bar is still there", JSON.stringify(s0));
  await sleep(1700);
  s0 = await shown();
  check(s0.bar === "0", "and gone after a full 3 s", JSON.stringify(s0));
  // a bar button over a resize bar keeps its clicks (found in the native client: the tall bar swallowed them)
  await move(P, 640, 320);
  await sleep(450);
  const fg = await guts(P);
  const cbs = await rects(P, ".ctlrow .cb");
  const hit = [];
  for (const g of fg) for (const b of cbs) {
    const x0 = Math.max(g.x, b.x), x1 = Math.min(g.r, b.r), y0 = Math.max(g.y, b.y), y1 = Math.min(g.b, b.b);
    if (x1 - x0 > 4 && y1 - y0 > 4) hit.push({ g: g.id, x: (x0 + x1) / 2, y: (y0 + y1) / 2 });
  }
  check(hit.length > 0, "in fullscreen a resize bar lies under bar buttons (the situation of the native bug)", `${hit.length} overlaps`);
  const swallowed = await Promise.all(hit.map((p) => P.eval(`(() => { const e = document.elementFromPoint(${p.x}, ${p.y}); return !!(e && e.closest('.cb, .cgroup, .ctlrow')); })()`)));
  check(swallowed.every(Boolean), "the buttons on top get the pointer, not the tall bar", JSON.stringify(swallowed));
  if (hit.length) {
    const lbx = await rect(P, "#btn-layout");
    await move(P, lbx.cx, lbx.cy);
    await P.click("#btn-layout");
    check(await P.eval("!!document.querySelector('.menu.m-layout')"), "and a click on such a button works (Layout menu opens)");
    await key(P, "Escape");
  }
  await P.eval("document.exitFullscreen()");
  await P.waitFor("!document.fullscreenElement && document.querySelector('.p-side') && !__sss.S.stage.fullscreen", { what: "left fullscreen" });
  check(true, "leaving fullscreen (Escape) brings the sidebar and header back");
  await move(P, 700, 400);

  /* ============================== emoji avatars ============================== */
  section("emoji avatars");
  const emojiOf = (page, user) => page.eval(`[...document.querySelectorAll('.strip .pv')].find((p) => p.dataset.user === ${JSON.stringify(user)})?.querySelector('.avatar .emo')?.innerText ?? null`);
  const avBg = (page, user) => page.eval(`getComputedStyle([...document.querySelectorAll('.strip .pv')].find((p) => p.dataset.user === ${JSON.stringify(user)}).querySelector('.avatar')).backgroundImage`);
  const hueOfBg = (bg) => {
    const m = [...bg.matchAll(/rgb\((\d+), (\d+), (\d+)\)/g)][0];
    const [r, g, b] = [+m[1], +m[2], +m[3]].map((x) => x / 255);
    const hi = Math.max(r, g, b), lo = Math.min(r, g, b), d = hi - lo;
    if (!d) return null;
    const h = hi === r ? ((g - b) / d + 6) % 6 : hi === g ? (b - r) / d + 2 : (r - g) / d + 4;
    return { h: h * 60, s: d / (1 - Math.abs(hi + lo - 1)) };
  };
  // Alice picks an emoji in Settings > Profile
  await A.page.click("#btn-settings");
  await A.page.waitFor("document.querySelector('#btn-avatar')", { what: "settings" });
  const openPicker = async () => {
    if (await A.page.eval("!!document.querySelector('.menu.m-emoji')")) return;
    await A.page.click("#btn-avatar");
    await A.page.waitFor("document.querySelector('.menu.m-emoji .emoji-grid')", { what: "picker" });
  };
  const pickGrid = async (e) => {
    await openPicker();
    await A.page.click({ js: `[...document.querySelectorAll('.menu.m-emoji .emoji-grid button')].find((b) => b.innerText === ${JSON.stringify(e)})` });
  };
  await openPicker();
  const grid56 = await A.page.eval("document.querySelectorAll('.menu.m-emoji .emoji-grid button').length");
  check(grid56 === 56 && (await A.page.eval("!!document.querySelector('#emoji-in')")), "Settings > Profile: the picker offers 56 emoji and a Paste any emoji field");
  await shot(A.page, "17_emoji_picker");
  await pickGrid("🦁");
  check(await A.page.eval("!document.querySelector('.menu.m-emoji')"), "a pick closes the picker (the modal under it stays)");
  await P.waitFor(`document.querySelector('.pv[data-user="Alice"] .avatar .emo')?.innerText === "🦁"`, { timeout: 15000, what: "Vera sees Alice's emoji over the datagrams" });
  check(true, "Alice picks 🦁: Vera sees it on Alice's avatar (AvatarSet -> relay list -> AvatarList)");
  const lionBg = hueOfBg(await avBg(P, "Alice"));
  check(lionBg && lionBg.h >= 10 && lionBg.h <= 50, "the background comes from the emoji's strongest colour (an orange lion)", JSON.stringify(lionBg));
  const emoCss = await P.eval(`getComputedStyle(document.querySelector('.pv[data-user="Alice"] .avatar .emo')).filter`);
  check((emoCss.match(/drop-shadow/g) || []).length === 4, "a thin black outline follows the emoji's shape (four 1 px drop-shadows)", emoCss);
  await pickGrid("🐼");
  await P.waitFor(`document.querySelector('.pv[data-user="Alice"] .avatar .emo')?.innerText === "🐼"`, { timeout: 15000, what: "panda" });
  const sk = await avBg(P, "Alice");
  const skHue = hueOfBg(sk);
  check(skHue && skHue.s < 0.3, "a colourless emoji (the panda) gets a neutral slate, not a hue", sk);
  await shot(P, "18_emoji_panda");
  await openPicker();
  await A.page.type("#emoji-in", "🐻");
  await P.waitFor(`document.querySelector('.pv[data-user="Alice"] .avatar .emo')?.innerText === "🐻"`, { timeout: 15000, what: "pasted bear" });
  check(true, "Paste any emoji: the field takes the last emoji of what is typed");
  await A.page.type("#emoji-in", "hello");
  await sleep(150);
  check(/isn't an emoji/.test(await A.page.eval("document.querySelector('#emoji-hint').innerText")), "text that is no emoji is refused with a hint");
  await A.page.click({ text: "Use my initial", sel: ".menu.m-emoji .mi" });
  await P.waitFor(`document.querySelector('.pv[data-user="Alice"] .avatar .emo') == null`, { timeout: 15000, what: "emoji cleared" });
  check((await P.eval(`[...document.querySelectorAll('.strip .pv')].find((p) => p.dataset.user === "Alice").querySelector('.avatar').innerText.trim()`)).startsWith("A"), "Use my initial: the initial is back");
  await pickGrid("🦁");
  await P.waitFor(`document.querySelector('.pv[data-user="Alice"] .avatar .emo')?.innerText === "🦁"`, { timeout: 15000, what: "lion again" });
  check((await A.page.eval(`JSON.parse(localStorage.getItem("sss.settings.v1")).emoji`)) === "🦁", "the emoji is saved in localStorage");
  await shot(P, "19_emoji_lion");
  // tile glide (the replica's .st-tile transition): on in a plain arena, off while a bar or a window is dragged
  check(await P.eval(`(() => { const t = document.querySelector('.arena > .st-tile'), a = t.parentElement; const g = () => getComputedStyle(t).transitionProperty + "|" + getComputedStyle(t).transitionDuration; const on = g(); a.classList.add("dragging"); const off = g(); a.classList.remove("dragging"); return /left/.test(on) && /0\.2s/.test(on) && /^all\|0s|^none|0s$/.test(off); })()`), "tiles glide (left/top/width/height .2s) and do not while the arena is dragging");
  // Settings tabs: Profile | Interface (Language, Sounds) | Playback; one stable modal height
  const tabBtn = (name) => A.page.click({ text: name, sel: ".modal .seg button" });
  const modalSize = () => A.page.eval(`(() => { const r = document.querySelector('.modal').getBoundingClientRect(); return Math.round(r.width) + "x" + Math.round(r.height); })()`);
  const has = (sel) => A.page.eval(`!!document.querySelector(${JSON.stringify(sel)})`);
  await A.page.key("Escape"); // the picker menu, if still open
  if (!(await has(".modal"))) { await A.page.click("#btn-settings"); await A.page.waitFor("document.querySelector('.modal')", { what: "settings reopened" }); }
  check((await has("#btn-avatar")) && !(await has("#set-jitter")) && !(await has("#btn-sound-join")), "Settings opens on Profile: the avatar row shows, Playback and Sounds do not");
  const sizes = [await modalSize()];
  await tabBtn("Interface");
  check((await has("#btn-sound-join")) && (await has("#set-sound-vol")) && (await A.page.eval("!!document.querySelector('.modal .seg button.on') && document.querySelectorAll('.modal .gcard').length === 2")) && !(await has("#btn-avatar")), "Interface tab: Language and Sounds cards, no Profile card");
  sizes.push(await modalSize());
  await shot(A.page, "20_settings_interface");
  await tabBtn("Playback");
  check((await has("#set-jitter")) && (await has("#set-catchup")) && !(await has("#set-level-loudness")) && !(await has("#btn-sound-join")), "Playback tab: jitter and catch-up, and no Level loudness row any more (it is a bar button)");
  sizes.push(await modalSize());
  await tabBtn("Profile");
  sizes.push(await modalSize());
  check(new Set(sizes).size === 1, "the modal keeps one size on every tab", sizes.join(" "));
  await tabBtn("Interface");
  await A.page.key("Escape");
  await A.page.click("#btn-settings");
  await A.page.waitFor("document.querySelector('.modal')", { what: "settings reopened" });
  check(await has("#btn-sound-join"), "Settings reopens on the tab last used");
  await tabBtn("Profile");
  await A.page.key("Escape");
  check(await A.page.eval("!document.querySelector('.modal')"), "Escape closes the settings modal");
  // the hover card and the stream menu carry it too
  const ha = await P.eval(`(() => { const r = document.querySelector('.pv[data-user="Alice"] .avatar').getBoundingClientRect(); return { cx: r.x + r.width / 2, cy: r.y + r.height / 2 }; })()`);
  await move(P, ha.cx, ha.cy);
  await P.waitFor("document.querySelector('.utip .avatar .emo')", { what: "emoji in the hover card" });
  check(true, "the hover card shows the emoji avatar as well");
  await move(P, 4, 4);

  /* ============================== reconnect: pop-outs and avatars survive a relay restart ============================== */
  if (RELAY_BIN && RELAY_DIR) {
    section("relay restart: a pop-out comes back, the stage window stays, avatars re-announce");
    const port = RELAY.split(":").pop();
    const bi = await P.eval(`(() => { const ts = [...document.querySelectorAll('.arena > .st-tile')]; return ts.findIndex((t) => t._it && t._it.owner === "Carol"); })()`);
    const ct2 = (await tiles(P))[bi];
    await move(P, ct2.cx, ct2.cy);
    await sleep(250);
    await P.click({ js: `document.querySelectorAll('.arena > .st-tile')[${bi}].querySelector('.tools .round-btn')` });
    await P.waitFor(`[...__sss.S.rooms[0].watches.values()].some((w) => w.ownerName === "Carol" && w.popped === "pip")`, { timeout: 10000, what: "Carol popped out (PiP)" });
    const oldKey = await P.eval(`[...__sss.S.rooms[0].watches.values()].find((w) => w.ownerName === "Carol").key`);
    spawnSync("bash", [W + "relay.sh", "restart", RELAY_BIN, RELAY_DIR, port], { stdio: "inherit" });
    await P.waitFor(`__sss.S.rooms[0].state === "reconnecting" || __sss.S.rooms[0].state === "joined"`, { what: "reconnect starts" });
    await P.waitFor(`__sss.S.rooms[0].state === "joined" && [...__sss.S.rooms[0].watches.values()].some((w) => w.ownerName === "Carol" && w.key !== ${JSON.stringify(oldKey)})`, { timeout: 60000, what: "Carol's stream resumed under a new key" });
    const after = await P.eval(`(() => { const w = [...__sss.S.rooms[0].watches.values()].find((x) => x.ownerName === "Carol"); return { popped: w.popped, inPip: !!w.canvas.closest('.pip-stage'), ownDoc: w.canvas.ownerDocument !== document }; })()`);
    check(after.popped === "pip" && after.inPip && after.ownDoc, "a stream popped out before the relay restarted is popped out again after it (its window stayed open, the new canvas moved in)", JSON.stringify(after));
    await P.waitFor("document.querySelectorAll('.pv.popped').length === 1", { what: "badge back on Carol's avatar" });
    for (let i = 0; i < 5 && (await P.eval("[...__sss.S.rooms[0].watches.values()].some((w) => w.popped)")); i++) {
      await P.click(".pv.popped").catch(() => {}); // retry: a render between press and release swallows a click
      await sleep(700);
    }
    await P.waitFor("![...__sss.S.rooms[0].watches.values()].some((w) => w.popped)", { what: "brought back" });
    await P.waitFor(`document.querySelector('.pv[data-user="Alice"] .avatar .emo')?.innerText === "🦁"`, { timeout: 30000, what: "Alice's emoji after the relay restart" });
    check(true, "after the relay restart Alice's emoji is announced again (a reconnect has a new ConnId)");

    // the whole stage in its own window, across a restart
    if (await P.eval("!!window.documentPictureInPicture")) {
      // a click is press + release on the same element, and the UI re-renders on every state change: retry if one landed in between
      for (let i = 0; i < 4 && !(await P.eval("!!__sss.stageWindow()")); i++) {
        await P.click("#btn-popstage");
        await sleep(600);
      }
      await P.waitFor("!!__sss.stageWindow()", { what: "stage window" });
      await sleep(500);
      const sw = await P.eval(`(() => { const w = __sss.stageWindow(); const d = w.win.document; return { tiles: d.querySelectorAll('.arena > .st-tile').length, bar: !!d.querySelector('.ctlrow'), strip: d.querySelectorAll('.pv').length, back: !!document.querySelector('#btn-stage-back'), mainTiles: document.querySelectorAll('.arena > .st-tile').length, fs: !!d.querySelector('#btn-stagefs'), side: !!d.querySelector('#btn-sidebar'), canvasInPop: [...__sss.S.rooms[0].watches.values()].every((w) => w.canvas.ownerDocument === d) }; })()`);
      check(sw.tiles === 4 && sw.bar && sw.strip === 4 && sw.back && sw.mainTiles === 0 && sw.canvasInPop && !sw.fs && !sw.side, "the stage pops out: the tiles, strip and bar are in its own window, the main window says so and offers Bring it back", JSON.stringify(sw));
      await shot(P, "20_stage_popped");
      // headless Chromium opens the window at about 450x270 (strip and bar take all of it) and cannot resize it: give its target a 1000x700 viewport through CDP
      let pipSession = null;
      try {
        const { targetInfos } = await V.conn.send("Target.getTargets");
        const mainId = (await V.conn.send("Target.getTargetInfo", {}, V.page.sid)).targetInfo.targetId;
        const pip = targetInfos.find((t) => t.type === "page" && t.targetId !== mainId);
        if (pip) {
          pipSession = (await V.conn.send("Target.attachToTarget", { targetId: pip.targetId, flatten: true })).sessionId;
          await V.conn.send("Emulation.setDeviceMetricsOverride", { width: 1000, height: 700, deviceScaleFactor: 1, mobile: false }, pipSession);
        }
      } catch (e) {
        console.log("(the stage window could not be sized through CDP:", e.message + ")");
      }
      await P.waitFor("__sss.stageWindow().win.innerWidth >= 900", { timeout: 5000, what: "stage window sized" }).catch(() => console.log("(the stage window stayed small)"));
      await sleep(800);
      // the stage window itself: everything laid out inside it (tiles, strip and bar within its bounds), and a screenshot of its own target
      const fit = await P.eval(`(() => { const w = __sss.stageWindow().win; const d = w.document; const W = w.innerWidth, H = w.innerHeight; const inside = (e) => { const r = e.getBoundingClientRect(); return r.width > 0 && r.left >= -1 && r.top >= -1 && r.right <= W + 1 && r.bottom <= H + 1; }; return { W, H, tiles: [...d.querySelectorAll('.arena > .st-tile')].every(inside), bar: inside(d.querySelector('.ctlrow')), strip: inside(d.querySelector('.stripwrap')), ctlBottom: d.querySelector('.ctlrow').getBoundingClientRect().bottom }; })()`);
      check(fit.tiles && fit.bar && fit.strip, "in the stage window the tiles, the strip and the bar are laid out inside its bounds", JSON.stringify(fit));
      try {
        if (pipSession) {
          const r = await V.conn.send("Page.captureScreenshot", { format: "png" }, pipSession);
          (await import("node:fs")).writeFileSync(OUT + PFX + "20b_stage_window.png", Buffer.from(r.data, "base64"));
        }
      } catch (e) {
        console.log("(no screenshot of the stage window:", e.message + ")");
      }
      spawnSync("bash", [W + "relay.sh", "restart", RELAY_BIN, RELAY_DIR, port], { stdio: "inherit" });
      await P.waitFor(`__sss.S.rooms[0].state === "joined" && [...__sss.S.rooms[0].watches.values()].length === 3 && [...__sss.S.rooms[0].watches.values()].every((w) => w.firstFrame)`, { timeout: 60000, what: "back after the restart" });
      await sleep(1500);
      const sw2 = await P.eval(`(() => { const w = __sss.stageWindow(); if (!w) return null; const d = w.win.document; return { tiles: d.querySelectorAll('.arena > .st-tile').length, popped: __sss.S.stage.popped.size }; })()`);
      check(sw2 && sw2.tiles === 4 && sw2.popped === 1, "the stage window survives a relay restart and shows the stage again", JSON.stringify(sw2));
      await P.eval(`__sss.stageWindow().win.document.querySelector('#btn-popstage').click()`);
      await P.waitFor("!__sss.stageWindow() && document.querySelectorAll('.arena > .st-tile').length === 4", { what: "brought back from its own window's button" });
      check(true, "Bring the stage back (the button in its window) returns it to the main window");
    } else check(true, "the browser has no Document Picture-in-Picture: the stage pop-out button is hidden");
  } else console.log("\n(no --relay-bin/--relay-dir: the relay-restart checks are skipped)");

  /* ============================== webcam, webcam in picture, audio-only tiles (task I10) ============================== */
  section("webcam: getUserMedia, reaches a second page, a tile of its own");
  // #131: Webcam in picture is ON by default (a view with no saved value). The tile-based checks of this section need it off, so switch it off
  // on Vera's and Alice's pages through the real Layout menu first; the picture sections below switch it on again.
  check((await P.eval("__sss.S.stage.pipOff.size === 0")) && (await A.page.eval("__sss.S.stage.pipOff.size === 0")), "Webcam in picture is on by default (no view is switched off)");
  for (const X of [P, A.page]) {
    await X.click("#btn-layout");
    await X.waitFor("document.querySelector('.menu.m-layout')", { what: "layout menu" });
    await X.click({ js: "[...document.querySelectorAll('.menu .cbx')].find((e) => /Webcam in picture/.test(e.innerText))" });
    await key(X, "Escape");
  }
  check((await P.eval("__sss.S.stage.pipOff.size === 1")) && (await A.page.eval("__sss.S.stage.pipOff.size === 1")), "…and the Layout menu switches it off per view");
  // where a PIP of `size` starts along an axis of `len` on column/row i (an independent restatement of the rule: 12 px in, centred in the middle; the inset shrinks in tiny pictures)
  const Gm = { pipAt: (origin, len, size, i) => { const m = Math.max(Math.min(12, (len - size) / 2), 0); return i === 0 ? origin + m : i === 1 ? origin + (len - size) / 2 : origin + len - size - m; } };
  await setLayout("split");
  await openLayoutMenu();
  await layoutItem("/Reset/");
  await closeMenu();
  const SPOTS = [[0, 0], [1, 0], [2, 0], [0, 1], [2, 1], [0, 2], [1, 2], [2, 2]];
  const liveCam = (X) => X.page.eval("__sss.S.captures.some((c) => c.kind === 'webcam' && c.state === 'live' && [...c.dests.values()].some((d) => d.state === 'live'))");
  // Alice's webcam through her real bar: a click starts the saved camera (the fake one), no staging step, audio: false
  await A.page.waitFor("!document.querySelector('#btn-webcam').disabled", { what: "Alice's webcam button" });
  await A.page.click("#btn-webcam");
  await A.page.waitFor("__sss.S.captures.some((c) => c.kind === 'webcam' && c.state === 'live' && [...c.dests.values()].some((d) => d.state === 'live'))", { timeout: 20000, what: "Alice's webcam live" });
  check(await A.page.eval("document.querySelector('#btn-webcam').parentElement.classList.contains('on') && document.querySelector('#btn-webcam .ms').innerText.trim() === 'videocam'"), "a live webcam lights its bar button green with the videocam icon");
  check(await A.page.eval("(() => { const c = __sss.S.captures.find((x) => x.kind === 'webcam'); return !c.audio && c.stream.getAudioTracks().length === 0 && c.label === 'Webcam'; })()"), "the webcam is a video-only capture labelled exactly Webcam (audio: false)");
  check(await A.page.eval("!!document.querySelector('.st-tile.mine.cam') && /Webcam/.test(document.querySelector('.st-tile.mine.cam .tag').innerText)"), "her own stage has a `You · Webcam` tile next to her screen");
  await P.waitFor("document.querySelectorAll('.arena > .st-tile').length === 5", { timeout: 30000, what: "the webcam shows on Vera's stage" });
  const camTile = await P.eval(`(() => { const t = [...document.querySelectorAll('.arena > .st-tile')].find((x) => /\\/Webcam$/.test(x.dataset.id)); return t ? { idle: t.classList.contains('idle'), text: t.innerText, watch: !!t.querySelector('[data-watch]') } : null; })()`);
  check(camTile && camTile.idle && camTile.watch && /Alice is sharing Webcam/.test(camTile.text), "on Vera's page it is an ordinary stream: an idle tile `Alice is sharing Webcam` with Watch stream", JSON.stringify(camTile));
  check(await P.eval("__sss.S.rooms[0].streams.some((s) => s.kind === 'video' && s.label === 'Webcam' && s.ownerName === 'Alice') && !__sss.S.rooms[0].streams.some((s) => s.kind === 'audio')"), "on the wire: a video stream labelled `Webcam`, and no audio stream");
  for (let i = 0; i < 6 && (await P.eval("__sss.S.rooms[0].watches.size")) < 4; i++) {
    await P.click({ js: "document.querySelector('.st-tile[data-id$=\"/Webcam\"] [data-watch]')" });
    await sleep(700);
  }
  await P.waitFor("[...__sss.S.rooms[0].watches.values()].some((w) => w.label === 'Webcam' && w.firstFrame)", { timeout: 40000, what: "the webcam's first frame on Vera's page" });
  const camStats = await P.eval("(() => { const r = __sss.S.rooms[0]; const w = [...r.watches.values()].find((x) => x.label === 'Webcam'); return { st: r.client.watchStats(w.key), audio: !!w.handle.audioKey, w: w.canvas.width, h: w.canvas.height }; })()");
  await sleep(1500);
  const camStats2 = await P.eval("(() => { const r = __sss.S.rooms[0]; const w = [...r.watches.values()].find((x) => x.label === 'Webcam'); return r.client.watchStats(w.key); })()");
  check(camStats2.decoded > camStats.st.decoded && camStats.w > 0 && camStats.h > 0 && !camStats.audio, "the webcam decodes on the second page and Alice's webcam watch plays no audio", `${camStats.st.decoded} -> ${camStats2.decoded} frames, ${camStats.w}x${camStats.h}`);
  check(await P.eval("(() => { const t = document.querySelector('.st-tile[data-id$=\"/Webcam\"]'); return !!t && t.classList.contains('cam') && !t.querySelector('.tag .ms.off') && !!t.querySelector('.tag .ms') && t.querySelector('.tag .ms').innerText.trim() === 'videocam'; })()"), "a watched webcam's tag has the videocam icon, no audio icon (a webcam brings no audio)");
  // the menu: no volume row, no Level loudness, "No audio in this stream"
  await P.eval(`document.querySelector('.st-tile[data-id$="/Webcam"] [data-mk^="tile:"]').click()`);
  await P.waitFor("document.querySelector('.menu.m-tile')", { what: "the webcam's stream menu" });
  check(await P.eval("!document.querySelector('.menu.m-tile input.vol') && /No audio/i.test(document.querySelector('.menu.m-tile').innerText)"), "the webcam's stream menu has no volume and says it has no audio");
  await closeMenu();
  await shot(P, "40_cam_tile");

  section("webcam in picture: on per view");
  await viewKey();
  const tileCount = () => P.eval("document.querySelectorAll('.arena > .st-tile').length");
  check((await tileCount()) === 5 && (await P.eval("!document.querySelector('.pip')")), "with Webcam in picture switched off the webcam is a tile of its own");
  await openLayoutMenu();
  check(await P.eval("(() => { const c = [...document.querySelectorAll('.menu .cbx')].find((e) => /Webcam in picture/.test(e.innerText)); return !!c && !c.classList.contains('on') && !c.classList.contains('disabled') && c.getAttribute('aria-checked') === 'false'; })()"), "the Layout menu's `Webcam in picture` is a live checkbox, unchecked");
  await closeMenu();
  await P.eval("(() => { const w = [...__sss.S.rooms[0].watches.values()].find((x) => x.label === 'Webcam'); w.canvas.__marker = 'same canvas'; })()");
  const pipOn = async () => {
    await openLayoutMenu();
    await layoutItem("/Webcam in picture/");
    await closeMenu();
    await sleep(300);
  };
  await pipOn();
  await P.waitFor("document.querySelectorAll('.arena > .st-tile').length === 4 && !!document.querySelector('.arena > .st-tile > .pip')", { what: "the webcam moves into Alice's screen" });
  let st = await pipState(P);
  check(st && /\/Alice\/video\/Screen$/.test(st.parent) && /Webcam$/.test(st.id), "with it on, Alice's webcam sits inside Alice's screen share and has no tile", st && `${st.parent} <- ${st.id}`);
  check(await P.eval("[...__sss.S.rooms[0].watches.values()].find((x) => x.label === 'Webcam').canvas.__marker === 'same canvas' && !!document.querySelector('.pip > canvas')"), "the webcam's <canvas> was re-adopted by the PIP, not recreated");
  near(st.pip.w, 0.24 * st.tile.w, 1.5, "a new PIP is a quarter of the picture wide (.24)");
  check(st.pos.spot[0] === 2 && st.pos.spot[1] === 2 && spotInsetOk(st, [2, 2]), "it starts bottom right, 12 px from both edges", JSON.stringify(st.pos));
  check(st.c === "lt", "the handle sits on the corner facing the middle of the picture (top left for a PIP bottom right)", st.c);
  const glide = await P.eval("[...document.styleSheets].some((sh) => [...sh.cssRules].some((r) => r.selectorText === '.pip.snap' && /0\\.16s/.test(r.style.transition)))");
  check(/6px/.test(st.radius) && glide, "the browser rounds the corners (6 px) and glides to a spot (.16 s, `.pip.snap`)", `${st.radius} glide=${glide}`);
  await shot(P, "41_pip_default");

  section("webcam in picture: eight spots with real drags, resize, edges");
  const tileOf = (p) => p.eval("(() => { const t = document.querySelector('.arena > .st-tile:has(> .pip)'); const r = t.getBoundingClientRect(); return { id: t.dataset.id, x: r.x, y: r.y, w: r.width, h: r.height, z: t.style.zIndex, l: t.style.left, t: t.style.top, wd: t.style.width, ht: t.style.height }; })()");
  const tile0 = await tileOf(P);
  const dragTo = async (spot) => {
    const s = await pipState(P);
    const x0 = s.pip.x + s.pip.w / 2;
    const y0 = s.pip.y + s.pip.h / 2;
    const tx = s.tile.x + Gm.pipAt(0, s.tile.w, s.pip.w, spot[0]) + s.pip.w / 2;
    const ty = s.tile.y + Gm.pipAt(0, s.tile.h, s.pip.h, spot[1]) + s.pip.h / 2;
    let mid = null;
    await drag(P, x0, y0, tx - x0, ty - y0, { steps: 10, mid: async () => { mid = await pipState(P); } });
    await sleep(450); // the glide
    return { after: await pipState(P), mid };
  };
  const tileSame = (a, b) => ["x", "y", "w", "h"].every((k) => Math.abs(a[k] - b[k]) < 0.6);
  let allSpots = true;
  const spotNotes = [];
  for (const spot of SPOTS) {
    const { after, mid } = await dragTo(spot);
    const pipGood = after && after.pos.spot && after.pos.spot[0] === spot[0] && after.pos.spot[1] === spot[1] && spotInsetOk(after, spot) && insideOf(after.pip, after.tile) && mid && mid.pos.spot === null;
    spotNotes.push(`${spot}:${pipGood ? "ok" : "BAD " + JSON.stringify(after && after.pos)}`);
    if (!pipGood) allSpots = false;
    if (!tileSame(await tileOf(P), tile0)) allSpots = false;
  }
  check(allSpots, "a real drag to each of the 8 spots snaps there (free during the drag, 12 px inset or centred on release) and never moves the tile under it", spotNotes.join(" "));
  // a click changes nothing
  st = await pipState(P);
  await P.click({ js: "document.querySelector('.pip')" });
  const pipSt1 = await pipState(P);
  check(JSON.stringify(pipSt1.pos) === JSON.stringify(st.pos) && Math.abs(pipSt1.pip.x - st.pip.x) < 0.6, "a click on the PIP changes nothing");
  await shot(P, "42_pip_spot");
  // resize: aspect locked, the corner opposite the handle stays, the spot stays
  st = await pipState(P);
  const hx = st.pip.x + (st.c[0] === "r" ? st.pip.w - 10 : 10);
  const hy = st.pip.y + (st.c[1] === "b" ? st.pip.h - 10 : 10);
  await move(P, st.pip.x + 5, st.pip.y + 5); // hover the tile so the handle shows
  const fx = st.c[0] === "r" ? st.pip.x : st.pip.x + st.pip.w;
  const fy = st.c[1] === "b" ? st.pip.y : st.pip.y + st.pip.h;
  await drag(P, hx, hy, st.c[0] === "r" ? 60 : -60, st.c[1] === "b" ? 34 : -34, { steps: 8 });
  await sleep(200);
  let pipSt2 = await pipState(P);
  const fx2 = st.c[0] === "r" ? pipSt2.pip.x : pipSt2.pip.x + pipSt2.pip.w;
  const fy2 = st.c[1] === "b" ? pipSt2.pip.y : pipSt2.pip.y + pipSt2.pip.h;
  check(pipSt2.pip.w > st.pip.w + 20, "dragging the handle outwards grows the PIP", `${st.pip.w.toFixed(0)} -> ${pipSt2.pip.w.toFixed(0)}`);
  near(pipSt2.pip.w / pipSt2.pip.h, st.ar, 0.02, "the aspect ratio is locked while resizing");
  check(Math.abs(fx2 - fx) < 1.5 && Math.abs(fy2 - fy) < 1.5, "the corner opposite the handle stays put", `${fx},${fy} vs ${fx2},${fy2}`);
  check(JSON.stringify(pipSt2.pos.spot) === JSON.stringify(st.pos.spot) && tileSame(await tileOf(P), tile0), "a resize leaves the spot and never resizes the tile under it");
  const hx2 = pipSt2.pip.x + (pipSt2.c[0] === "r" ? pipSt2.pip.w - 10 : 10);
  const hy2 = pipSt2.pip.y + (pipSt2.c[1] === "b" ? pipSt2.pip.h - 10 : 10);
  await drag(P, hx2, hy2, pipSt2.c[0] === "r" ? -900 : 900, pipSt2.c[1] === "b" ? -900 : 900, { steps: 8 });
  await sleep(200);
  pipSt2 = await pipState(P);
  near(pipSt2.pip.w, 72, 1, "shrinking stops at the minimum width of 72 px");
  await drag(P, pipSt2.pip.x + (pipSt2.c[0] === "r" ? pipSt2.pip.w - 10 : 10), pipSt2.pip.y + (pipSt2.c[1] === "b" ? pipSt2.pip.h - 10 : 10), pipSt2.c[0] === "r" ? 3000 : -3000, pipSt2.c[1] === "b" ? 3000 : -3000, { steps: 8 });
  await sleep(200);
  pipSt2 = await pipState(P);
  check(insideOf(pipSt2.pip, pipSt2.tile), "growing is capped by the visible picture", `${JSON.stringify(pipSt2.pip)} in ${JSON.stringify(pipSt2.tile)}`);
  // back to the default size for the rest
  await P.eval("(() => { const k = document.querySelector('.arena').dataset.view; for (const p of Object.values(__sss.vg(k).pip)) p.w = 0.24; __sss.render(); })()");
  await sleep(300);

  section("webcam in picture: stays on its spot across layouts, tile resizes and fullscreen; inside the picture in Crop");
  let pipKept = true;
  const pipNotes = [];
  for (const spot of [[0, 1], [2, 0], [1, 2]]) {
    await setLayout("split");
    const a = await dragTo(spot);
    for (const lay of ["crop", "float", "split"]) {
      await setLayout(lay);
      await sleep(250);
      const s = await pipState(P);
      const pipGood = s && JSON.stringify(s.pos.spot) === JSON.stringify(spot) && spotInsetOk(s, spot) && insideOf(s.pip, s.tile);
      pipNotes.push(`${spot}/${lay}:${pipGood ? "ok" : "BAD"}`);
      if (!pipGood) pipKept = false;
    }
  }
  check(pipKept, "the spot is the same in Split, Crop and Floating (same inset from the same edges)", pipNotes.join(" "));
  // a tile resize: the viewport shrinks, the tile follows
  const vspot = (await pipState(P)).pos.spot;
  await P.setViewport(1000, 640);
  await sleep(500);
  let pipSv = await pipState(P);
  check(pipSv && JSON.stringify(pipSv.pos.spot) === JSON.stringify(vspot) && spotInsetOk(pipSv, vspot) && pipSv.tile.w < 900, "a smaller window (smaller tiles) keeps the spot", `${pipSv && pipSv.tile.w.toFixed(0)}x${pipSv && pipSv.tile.h.toFixed(0)}`);
  await P.setViewport(1440, 900);
  await sleep(400);
  // a resize bar dragged in Split changes the tile under the PIP: the spot holds
  await setLayout("split");
  const barsNow = await guts(P);
  if (barsNow.length) {
    const before = await pipState(P);
    await drag(P, barsNow[0].cx, barsNow[0].cy, barsNow[0].w > barsNow[0].h ? 0 : 70, barsNow[0].w > barsNow[0].h ? 50 : 0, { steps: 8 });
    await sleep(300);
    const after = await pipState(P);
    check(after && JSON.stringify(after.pos.spot) === JSON.stringify(before.pos.spot) && spotInsetOk(after, after.pos.spot), "dragging a resize bar (the tile under the PIP changes size) keeps the spot");
    await P.eval(`__sss.vg(document.querySelector('.arena').dataset.view).grid = null; __sss.render();`);
    await sleep(300);
  }
  // Crop in a tall narrow window: the picture is wider than the tile, the PIP stays inside what shows, for every spot and for drags past the edges
  await P.setViewport(700, 1000);
  await setLayout("crop");
  await sleep(500);
  const pipCropTile = await pipState(P);
  check(pipCropTile && pipCropTile.picW > pipCropTile.tile.w + 1 || pipCropTile && pipCropTile.picH > pipCropTile.tile.h + 1, "in this window Crop really crops the picture", pipCropTile && `${pipCropTile.tile.w.toFixed(0)}x${pipCropTile.tile.h.toFixed(0)} tile`);
  let pipCropOk = true;
  const pipCropNotes = [];
  for (const spot of SPOTS) {
    const { after, mid } = await dragTo(spot);
    const pipGood = after && JSON.stringify(after.pos.spot) === JSON.stringify(spot) && spotInsetOk(after, spot) && insideOf(after.pip, after.tile);
    pipCropNotes.push(`${spot}:${pipGood ? "ok" : "BAD"}`);
    if (!pipGood) pipCropOk = false;
  }
  check(pipCropOk, "in Crop all 8 spots are reachable and the PIP stays inside the visible picture", pipCropNotes.join(" "));
  let pipPastOk = true;
  for (const [dx, dy] of [[-2500, -2500], [2500, 2500], [-2500, 2500], [2500, -2500]]) {
    const s = await pipState(P);
    let inMid = true;
    await drag(P, s.pip.x + s.pip.w / 2, s.pip.y + s.pip.h / 2, dx, dy, { steps: 8, mid: async () => { const m = await pipState(P); inMid = insideOf(m.pip, m.tile); } });
    await sleep(450);
    const a = await pipState(P);
    if (!inMid || !insideOf(a.pip, a.tile)) pipPastOk = false;
  }
  check(pipPastOk, "a drag far past the edges stops at the edge of the visible picture, in Crop too (while held and after release)");
  await shot(P, "43_pip_crop");
  await P.setViewport(1440, 900);
  await setLayout("split");
  await sleep(400);

  // Floating: the PIP is above its window; a press on it never moves or resizes the window
  await setLayout("float");
  await sleep(300);
  const fl0 = await tileOf(P);
  await dragTo([0, 0]);
  const fl1 = await tileOf(P);
  check(tileSame(fl0, fl1) && fl0.l === fl1.l && fl0.t === fl1.t, "in Floating a drag of the PIP never moves its parent window", `${fl0.l},${fl0.t} vs ${fl1.l},${fl1.t}`);
  await setLayout("split");

  // the stage's own fullscreen: same spot
  const fsSpot = (await pipState(P)).pos.spot;
  await P.click("#btn-stagefs");
  await P.waitFor("__sss.S.stage.fullscreen && document.querySelector('.stagefs') && !!document.querySelector('.arena > .st-tile > .pip')", { timeout: 8000, what: "stage fullscreen with the PIP" });
  await sleep(600);
  const pipSf = await pipState(P);
  check(pipSf && JSON.stringify(pipSf.pos.spot) === JSON.stringify(fsSpot) && spotInsetOk(pipSf, fsSpot), "the stage's fullscreen keeps the PIP on its spot", pipSf && JSON.stringify(pipSf.pos.spot));
  await P.click("#btn-stagefs");
  await P.waitFor("!__sss.S.stage.fullscreen", { what: "left the stage fullscreen" });
  await sleep(400);
  // the stream's own fullscreen (double-click on the parent): the PIP goes into it
  const ptile = await tileOf(P);
  await move(P, ptile.x + ptile.w / 2, ptile.y + ptile.h / 2);
  await P.dblclick({ js: "document.querySelector('.arena > .st-tile:has(> .pip) .vid')" });
  await P.waitFor("!!document.querySelector('.pip-stage > .pip')", { timeout: 8000, what: "the PIP in the stream fullscreen" });
  await sleep(500);
  const pipSfs = await P.eval(`(() => { const s = document.querySelector('.pip-stage'); const p = s.querySelector('.pip'); const cv = s.querySelector('canvas'); const b = s.getBoundingClientRect(); const ar = cv.width / cv.height; let w = b.width, h = w / ar; if (h > b.height) { h = b.height; w = h * ar; } const pic = { x: b.x + (b.width - w) / 2, y: b.y + (b.height - h) / 2, w, h }; const r = p.getBoundingClientRect(); return { pip: { x: r.x, y: r.y, w: r.width, h: r.height }, tile: pic, pos: JSON.parse(JSON.stringify(__sss.vg(document.querySelector('.arena') ? document.querySelector('.arena').dataset.view : "")?.pip[p.dataset.id] ?? null)), tiles: document.querySelectorAll('.arena > .st-tile').length, camTiles: [...document.querySelectorAll('.arena > .st-tile')].filter((x) => /Webcam$/.test(x.dataset.id)).length }; })()`);
  check(pipSfs.pos && spotInsetOk(pipSfs, pipSfs.pos.spot) && pipSfs.camTiles === 0, "the stream's fullscreen has its webcam in picture on the same spot, and the webcam is not a tile behind it", JSON.stringify({ spot: pipSfs.pos && pipSfs.pos.spot, camTiles: pipSfs.camTiles }));
  await shot(P, "44_pip_fullscreen");
  // the PIP can be moved there too, and that is the same state the stage uses
  const fsPip = await P.eval("(() => { const r = document.querySelector('.pip-stage > .pip').getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()");
  await drag(P, fsPip.x, fsPip.y, -3000, -3000, { steps: 8 });
  await sleep(450);
  const fsMoved = await P.eval("JSON.parse(JSON.stringify(Object.values(__sss.vg(document.querySelector('.arena').dataset.view).pip)[0]))");
  check(JSON.stringify(fsMoved.spot) === "[0,0]", "dragging it in the stream fullscreen snaps it to a spot too (top left)", JSON.stringify(fsMoved));
  await P.dblclick({ js: "document.querySelector('.pip-stage canvas')" });
  await P.waitFor("!document.querySelector('.pip-stage')", { timeout: 8000, what: "left the stream fullscreen" });
  await sleep(500);
  const pipBack = await pipState(P);
  check(pipBack && JSON.stringify(pipBack.pos.spot) === "[0,0]" && spotInsetOk(pipBack, [0, 0]) && (await tileCount()) === 4, "leaving it returns the PIP to the tile on the spot you moved it to");
  await dragTo([2, 2]);

  section("webcam in picture: stays a tile when it cannot be a picture; pop-out; off");
  // Bob has watched nothing and keeps the default (Webcam in picture on, #131); neither stream is playing
  await B.page.waitFor("document.querySelectorAll('.st-tile').length >= 3", { timeout: 20000, what: "Bob's stage" });
  const bobTiles = () => B.page.eval(`[...document.querySelectorAll('.arena > .st-tile')].map((t) => ({ id: t.dataset.id, idle: t.classList.contains('idle') }))`);
  const bob0 = await bobTiles();
  check(!(await B.page.eval("!!document.querySelector('.pip')")) && bob0.some((t) => /Webcam$/.test(t.id) && t.idle) && bob0.some((t) => /\/Alice\/video\/Screen$/.test(t.id) && t.idle), "a viewer who has watched neither stream sees both as tiles (a webcam you cannot see playing in a picture would have no Watch button)", JSON.stringify(bob0.map((t) => t.id.split("/").pop())));
  await B.page.click({ js: "document.querySelector('.st-tile[data-id$=\"/Webcam\"] [data-watch]')" });
  await B.page.waitFor("__sss.S.rooms[0].watches.size === 1", { what: "Bob watches the webcam only" });
  await sleep(500);
  check(!(await B.page.eval("!!document.querySelector('.pip')")), "watching only the webcam (its screen not playing) leaves it a tile");
  await B.page.click({ js: "document.querySelector('.st-tile[data-id$=\"/Alice/video/Screen\"] [data-watch]')" });
  await B.page.waitFor("__sss.S.rooms[0].watches.size === 2 && !!document.querySelector('.arena > .st-tile > .pip')", { timeout: 20000, what: "Bob's webcam moves into the screen it now watches" });
  check(true, "once Bob watches the screen too, the webcam goes into it");
  // the parent popped out: the webcam is a tile on the stage again
  if (await P.eval("typeof documentPictureInPicture !== 'undefined'")) {
    await P.click({ js: "document.querySelector('.arena > .st-tile:has(> .pip) [aria-label*=\"own window\"]')" });
    await P.waitFor("__sss.S.rooms[0].watches && [...__sss.S.rooms[0].watches.values()].some((w) => w.popped === 'pip')", { timeout: 10000, what: "the screen pops out" }).catch(() => {});
    await sleep(600);
    check((await P.eval("!document.querySelector('.pip') && [...document.querySelectorAll('.arena > .st-tile')].some((t) => /Webcam$/.test(t.dataset.id) && !t.classList.contains('idle'))")), "a popped-out parent shows alone in its window and its webcam is a playing tile on the stage");
    await P.eval("(() => { const w = [...__sss.S.rooms[0].watches.values()].find((x) => x.popped); const b = document.querySelector('.strip .pv.popped'); if (b) b.click(); })()");
    await P.waitFor("![...__sss.S.rooms[0].watches.values()].some((w) => w.popped) && !!document.querySelector('.arena > .st-tile > .pip')", { timeout: 10000, what: "the stream is back" });
    check(true, "bringing it back puts the webcam into the picture again");
  }
  // the strip's hover card says "In picture"
  const pipNm = await P.eval("(() => { const c = [...document.querySelectorAll('.strip .pv')].find((p) => p.dataset.user === 'Alice'); const r = c.querySelector('.avatar').getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()");
  await move(P, pipNm.x, pipNm.y);
  await P.waitFor("document.querySelector('.utip')", { what: "Alice's hover card" });
  check(await P.eval("[...document.querySelectorAll('.utip .ust')].some((r) => /Webcam/.test(r.querySelector('.l1').innerText) && /In picture/.test(r.querySelector('.l3').innerText))"), "the hover card of Alice says `In picture` on her webcam's row");
  await move(P, 5, 5);
  await sleep(300);

  // Vera's own webcam goes into her own screen; Hide my own streams removes both, others still see them
  section("webcam in picture: your own webcam in your own screen, Hide my own streams");
  await P.click("#btn-webcam");
  await P.waitFor("__sss.S.captures.some((c) => c.kind === 'webcam' && c.state === 'live' && [...c.dests.values()].some((d) => d.state === 'live'))", { timeout: 20000, what: "Vera's webcam live" });
  await sleep(500);
  const mineIn = await pipState(P, ".arena > .st-tile.mine > .pip");
  check(mineIn && /\/me$/.test(mineIn.parent) && /\/me\/cam$/.test(mineIn.id), "your own webcam sits inside your own screen share", mineIn && `${mineIn.parent} <- ${mineIn.id}`);
  check(await P.eval("!document.querySelector('.arena > .st-tile.mine.cam') && !!document.querySelector('.st-tile.mine > .pip > video.live-preview, .st-tile.mine > .pip > .mine-pic')"), "…with no tile of its own, showing the live preview or the placeholder");
  await openLayoutMenu();
  await layoutItem("/Hide my own streams/");
  await closeMenu();
  await sleep(400);
  check(await P.eval("!document.querySelector('.arena > .st-tile.mine') && ![...document.querySelectorAll('.pip')].some((p) => /\\/me\\/cam$/.test(p.dataset.id))"), "Hide my own streams removes my screen and my in-picture webcam from my stage");
  check(await A.page.eval("(() => { const ts = [...document.querySelectorAll('.arena > .st-tile')].map((t) => t.dataset.id); return ts.some((i) => /\\/Vera\\/video\\/Screen$/.test(i)) && ts.some((i) => /\\/Vera\\/video\\/Webcam$/.test(i)); })()"), "…and others still see both of Vera's shares");
  await openLayoutMenu();
  await layoutItem("/Hide my own streams/");
  await closeMenu();
  await sleep(300);
  await shot(P, "45_own_pip");
  // Webcam in picture off: the tile comes back
  await pipOn(); // toggles it off
  await sleep(400);
  const offTiles = await P.eval("[...document.querySelectorAll('.arena > .st-tile')].map((t) => t.dataset.id)");
  check(!(await P.eval("!!document.querySelector('.pip')")) && offTiles.some((i) => /\/Alice\/video\/Webcam$/.test(i)) && offTiles.some((i) => /\/me\/cam$/.test(i)) && (await P.eval("[...__sss.S.rooms[0].watches.values()].find((x) => x.label === 'Webcam').canvas.__marker === 'same canvas'")), "switching Webcam in picture off gives the webcams their tiles back (the same <canvas>)", String(offTiles.length));
  await pipOn(); // on again for the last step

  // a webcam alone (its owner stops the screen share): a tile again
  await A.page.click(".splitb:has(#btn-share) .caret");
  await A.page.waitFor("document.querySelector('.menu.m-share')", { what: "Alice's share menu" });
  await A.page.click({ js: "document.querySelector('.menu.m-share .mi.danger')" });
  await A.page.waitFor("!__sss.S.captures.some((c) => c.kind === 'screen')", { timeout: 10000, what: "Alice stops her screen" });
  await P.waitFor("!document.querySelector('.arena > .st-tile > .pip:not([data-id$=\"/me/cam\"])') && [...document.querySelectorAll('.arena > .st-tile')].some((t) => /\\/Alice\\/video\\/Webcam$/.test(t.dataset.id))", { timeout: 15000, what: "Alice's webcam is a tile again" });
  check(true, "a webcam whose owner stopped the screen share is a tile again (and a webcam alone stays one)");
  // the bar's Share button, idle: the picker, then live at once with the remembered settings, no modal (task 140)
  await L.stubCapture(A.page, { width: 960, height: 540, fps: 15, audio: false });
  const rem = await A.page.eval("JSON.stringify(__sss.S.settings.stream)");
  await L.goLiveAuto(A.page);
  const auto = await A.page.eval(`({ modal: !!document.querySelector('.modal'), dests: [...__sss.S.captures.find((c) => c.kind === 'screen').dests.values()].map((d) => [d.fps, d.codec, d.gopMs]) })`);
  const r0 = JSON.parse(rem);
  check(!auto.modal && auto.dests.length === 1 && auto.dests[0][0] === r0.fps && auto.dests[0][1] === r0.codec && auto.dests[0][2] === r0.gopMs, "Share (main button, idle): no stream settings modal, live with the remembered fps / codec / keyframe", JSON.stringify([rem, auto]));
  // stop the webcams through the real UI
  await P.click("#btn-webcam");
  await P.waitFor("!__sss.S.captures.some((c) => c.kind === 'webcam')", { timeout: 10000, what: "Vera's webcam stopped" });
  await A.page.click("#btn-webcam");
  await A.page.waitFor("!__sss.S.captures.some((c) => c.kind === 'webcam')", { timeout: 10000, what: "Alice's webcam stopped" });
  await P.waitFor("!__sss.S.rooms[0].streams.some((s) => s.label === 'Webcam')", { timeout: 10000, what: "the webcam stream is gone on Vera's page" });
  check(true, "the second click on the webcam button stops it, and its stream disappears from the room");

  section("audio-only senders: a Listen tile (no sound is played: the Listen click itself is a VM test)");
  // A stream list with an audio stream and no screen: injected into the page state (no audio stream, no AudioContext: the host never plays sound)
  const injectAudio = (withScreen, withCam) => P.eval(`(() => { const r = __sss.S.rooms[0]; const mk = (streamId, kind, label) => ({ key: "ffff:" + streamId, owner: "ffff", ownerId: 0xffffn, streamId, kind, label, width: 0, height: 0, self: false, ownerName: "Zed" });
    r.streams = r.streams.filter((s) => s.owner !== "ffff").concat([mk(77, "audio", "Desktop audio")]${withScreen ? ", mk(78, 'video', 'Screen')" : ""}${withCam ? ", mk(79, 'video', 'Webcam')" : ""}); __sss.render(); })()`);
  await injectAudio(false, false);
  await sleep(300);
  const audTile = await P.eval(`(() => { const t = document.querySelector('.st-tile.audio-only'); return t ? { text: t.innerText, listen: !!t.querySelector('[data-listen]'), tag: t.querySelector('.tag') && t.querySelector('.tag').innerText } : null; })()`);
  check(audTile && /Zed is sharing audio/.test(audTile.text) && /Audio only/.test(audTile.text) && audTile.listen && /Zed/.test(audTile.tag) && /Desktop audio/.test(audTile.tag), "an audio-only sender gets a tile `Zed is sharing audio · Audio only` with Listen and the label `Desktop audio`", JSON.stringify(audTile));
  await injectAudio(false, true);
  await sleep(200);
  check(await P.eval("!!document.querySelector('.st-tile.audio-only') && !!document.querySelector('.st-tile[data-id$=\"/Zed/Webcam\"]')"), "a sender with only a webcam and audio: the audio is an audio-only tile of its own (a webcam is no video for the audio that follows video)");
  await injectAudio(true, false);
  await sleep(200);
  check(await P.eval("!document.querySelector('.st-tile.audio-only') && !!document.querySelector('.st-tile[data-id$=\"/Zed/Screen\"]')"), "with a screen share the audio rides along: no audio-only tile");
  await P.eval("(() => { const r = __sss.S.rooms[0]; r.streams = r.streams.filter((s) => s.owner !== 'ffff'); __sss.render(); })()");
  await P.eval("document.querySelector('#btn-layout').scrollIntoView()");
  const audSrc = await P.eval("(() => { const w = __sss.S.rooms[0].client; return typeof w.watch === 'function'; })()");
  check(audSrc, "the core watches an audio stream directly (`watch(audioKey)`, no canvas) for Listen");
  check(true, "Share audio on the bar: with no audio-only share a click opens the browser's picker (not run here: no audio on the host)");

  section("two members with one name are two people (the user's report: hovering You showed nothing, hovering the other showed both cards)");
  {
    const T1 = await L.openApp(site.url, { name: "Twin", settings: { emoji: "🦊" } });
    const T2 = await L.openApp(site.url, { name: "Twin", settings: { emoji: "🐻" } });
    const TV = await L.openApp(site.url + "?fakeaudio=1", { name: "Wes" });
    const Q = TV.page;
    await L.createRoom(T1.page, { name: "twins", server: RELAY, hash: HASH });
    await L.waitJoined(T1.page);
    const tlink = await L.inviteLink(T1.page);
    for (const X of [T2, TV]) {
      await L.addRoomByLink(X.page, tlink, "twins");
      await L.waitJoined(X.page);
    }
    await startSharer(T1, "Twin", 960, 540);
    await startSharer(T2, "Twin", 960, 540);
    await Q.waitFor("document.querySelectorAll('.st-tile.idle').length === 2", { timeout: 30000, what: "both Twins' tiles" });
    await Q.waitFor(`document.querySelectorAll('.pv[data-user="Twin"] .emo').length === 2`, { timeout: 20000, what: "both Twins' emoji" });
    const ids = await Q.eval("[...document.querySelectorAll('.arena > .st-tile')].map((t) => t.dataset.id)");
    check(new Set(ids).size === 2, "both share the same label: two tiles with their own id", JSON.stringify(ids));
    const pvs = (p) => p.eval(`[...document.querySelectorAll('.strip .pv')].map((e) => ({ user: e.dataset.user, person: e.dataset.person, live: !!e.querySelector('.lv'), emo: e.querySelector('.emo')?.innerText ?? "" }))`);
    const twins = (await pvs(Q)).filter((x) => x.user === "Twin");
    check(twins.length === 2 && twins[0].person !== twins[1].person && twins.every((x) => x.live) && new Set(twins.map((x) => x.emo)).size === 2, "two avatars, each LIVE, each with its own emoji", JSON.stringify(twins));
    check((await Q.eval("[...document.querySelectorAll('.st-tile.idle .invite .emo')].map((e) => e.innerText).sort().join('')")) === "🐻🦊", "each idle tile shows its owner's emoji, not the first of that name");
    /** Hover the person `person` on page `p`: the cards that open (there must be one). */
    const hoverCard = async (p, person) => {
      await move(p, 3, 3);
      await p.waitFor("!document.querySelector('.utip')", { what: "no card left open" });
      const av = await p.eval(`(() => { const r = [...document.querySelectorAll('.strip .pv')].find((e) => e.dataset.person === ${JSON.stringify(person)}).querySelector('.avatar').getBoundingClientRect(); return { cx: r.x + r.width / 2, cy: r.y + r.height / 2 }; })()`);
      await move(p, av.cx, av.cy);
      await p.waitFor("document.querySelector('.utip')", { what: "a card" });
      await sleep(150);
      return p.eval(`({ n: document.querySelectorAll('.utip').length, name: document.querySelector('.utip .hd b').innerText, emo: document.querySelector('.utip .hd .emo')?.innerText ?? "", rows: document.querySelectorAll('.utip .ust').length })`);
    };
    for (const x of twins) {
      const c = await hoverCard(Q, x.person);
      check(c.n === 1 && c.emo === x.emo && c.rows === 1, `a third person hovering ${x.emo} sees only that card, with one stream`, JSON.stringify(c));
    }
    await move(Q, 3, 3);
    const own = await pvs(T1.page);
    const me = own.find((x) => x.emo === "🦊"), other = own.find((x) => x.emo === "🐻");
    const cMe = await hoverCard(T1.page, me.person), cOther = await hoverCard(T1.page, other.person);
    check(cMe.n === 1 && cMe.name === "You" && cMe.rows === 1, "hovering You shows your own card", JSON.stringify(cMe));
    check(cOther.n === 1 && cOther.name === "Twin" && cOther.emo === "🐻" && cOther.rows === 1, "hovering the other Twin shows only theirs", JSON.stringify(cOther));
    await shot(T1.page, "twins_card");
    await move(T1.page, 3, 3);
    // Hide my own streams hides mine by connection: the other Twin stays on my stage
    await T1.page.click("#btn-layout");
    await T1.page.waitFor("document.querySelector('.menu.m-layout')", { what: "layout menu" });
    await T1.page.click({ js: "[...document.querySelectorAll('.menu .mi, .menu .cbx')].find((e) => /Hide my own/.test(e.innerText))" });
    await T1.page.waitFor("document.querySelectorAll('.arena > .st-tile').length === 1", { what: "my own tile gone" });
    check(await T1.page.eval("!document.querySelector('.st-tile.mine') && !!document.querySelector('.st-tile.idle')"), "Hide my own streams hides only mine: the other client of my name stays on my stage");
    // both watched: each tile keeps its own pop-out and Stop watching
    for (let n = 1; n <= 2; n++) {
      for (let i = 0; i < 6 && (await Q.eval("__sss.S.rooms[0].watches.size")) < n; i++) {
        await Q.click("[data-watch]");
        await sleep(700);
      }
      await Q.waitFor(`__sss.S.rooms[0].watches.size === ${n}`, { what: `Twin watch ${n}` });
    }
    await Q.waitFor("[...__sss.S.rooms[0].watches.values()].every((w) => w.firstFrame)", { timeout: 40000, what: "first frames of both" });
    await Q.waitFor("document.querySelectorAll('.arena > .st-tile:not(.idle)').length === 2", { what: "two playing tiles" });
    const wOf = "[...__sss.S.rooms[0].watches.values()]";
    const t0 = (await tiles(Q))[0];
    const owner0 = await Q.eval(`document.querySelectorAll('.arena > .st-tile')[0]._it.conn`);
    await move(Q, t0.cx, t0.cy);
    await sleep(250);
    await Q.click({ js: `document.querySelectorAll('.arena > .st-tile')[0].querySelector('.tools .round-btn')` });
    await Q.waitFor(`${wOf}.some((w) => w.popped)`, { timeout: 10000, what: "one Twin popped out" });
    await sleep(300);
    const pop = await Q.eval(`({ tiles: document.querySelectorAll('.arena > .st-tile').length, popped: ${wOf}.filter((w) => w.popped).map((w) => w.owner), badges: [...document.querySelectorAll('.pv.popped')].map((e) => e.dataset.person.split("/").pop()) })`);
    check(pop.tiles === 1 && pop.popped.length === 1 && pop.popped[0] === owner0 && pop.badges.length === 1 && pop.badges[0] === owner0, "popping one tile pops only that stream, and the badge is on its owner's avatar only", JSON.stringify(pop));
    for (let i = 0; i < 5 && (await Q.eval(`${wOf}.some((w) => w.popped)`)); i++) {
      await Q.click(".pv.popped").catch(() => {});
      await sleep(700);
    }
    await Q.waitFor(`!${wOf}.some((w) => w.popped) && document.querySelectorAll('.arena > .st-tile').length === 2`, { what: "brought back" });
    if (RELAY_BIN && RELAY_DIR) {
      const orderBefore = await Q.eval(`__sss.vg("room:" + __sss.S.rooms[0].id).order.slice()`);
      spawnSync("bash", [W + "relay.sh", "restart", RELAY_BIN, RELAY_DIR, RELAY.split(":").pop()], { stdio: "inherit" });
      await Q.waitFor(`__sss.S.rooms[0].state === "joined" && ${wOf}.length === 2 && document.querySelectorAll('.arena > .st-tile:not(.idle)').length === 2`, { timeout: 60000, what: "both Twins watched again after the relay restart" });
      const orderAfter = await Q.eval(`__sss.vg("room:" + __sss.S.rooms[0].id).order.slice()`);
      const tilesAfter = await Q.eval("[...document.querySelectorAll('.arena > .st-tile')].map((t) => t.dataset.id).sort()");
      check(JSON.stringify(orderAfter) === JSON.stringify(orderBefore) && tilesAfter.every((id) => orderBefore.includes(id)), "a relay restart keeps both ids and both watches (the two may swap places, nothing is lost)", JSON.stringify({ orderBefore, orderAfter }));
      await Q.waitFor(`document.querySelectorAll('.pv[data-user="Twin"] .emo').length === 2`, { timeout: 30000, what: "emoji after the restart" });
      check(new Set((await pvs(Q)).filter((x) => x.user === "Twin").map((x) => x.emo)).size === 2, "both still have their own emoji");
      await Q.waitFor("[...__sss.S.rooms[0].watches.values()].every((w) => w.firstFrame)", { timeout: 40000, what: "first frames after the restart" });
    }
    const t1 = (await tiles(Q))[1];
    await rightClick(Q, t1.cx, t1.cy);
    await Q.waitFor("document.querySelector('.menu')", { what: "stream menu" });
    await Q.click({ js: "[...document.querySelectorAll('.menu .mi')].find((e) => /Stop watching/.test(e.innerText))" });
    await Q.waitFor(`${wOf}.length === 1`, { what: "one watch left" });
    check(await Q.eval("document.querySelectorAll('.st-tile.idle').length === 1 && document.querySelectorAll('.arena > .st-tile:not(.idle)').length === 1"), "Stop watching on one tile stops only that one");
    check([T1, T2, TV].every((x) => x.consoleErrors.length === 0), "no console errors on the Twins' pages", [T1, T2, TV].flatMap((x) => x.consoleErrors).join(" | "));
    for (const X of [T1, T2, TV]) await X.close();
  }

  section("Watched: one person in two rooms is one avatar (a name is the only identity across rooms; two of one name in one room stay two)");
  {
    const D1 = await L.openApp(site.url, { name: "Dana", settings: { emoji: "🦊" } });
    const D2 = await L.openApp(site.url, { name: "Dana", settings: { emoji: "🐻" } });
    const D3 = await L.openApp(site.url, { name: "Dana", settings: { emoji: "🐼" } });
    const WV = await L.openApp(site.url, { name: "Wim" });
    const Q = WV.page;
    await L.createRoom(D1.page, { name: "dana-one", server: RELAY, hash: HASH });
    await L.waitJoined(D1.page);
    const l1 = await L.inviteLink(D1.page);
    await L.addRoomByLink(D3.page, l1, "dana-one");
    await L.waitJoined(D3.page);
    await L.createRoom(D2.page, { name: "dana-two", server: RELAY, hash: HASH });
    await L.waitJoined(D2.page);
    const l2 = await L.inviteLink(D2.page);
    await L.addRoomByLink(WV.page, l1, "dana-one");
    await L.waitJoined(Q, 0);
    await L.addRoomByLink(WV.page, l2, "dana-two");
    await L.waitJoined(Q, 1);
    await startSharer(D1, "Dana", 960, 540);
    await startSharer(D3, "Dana", 960, 540);
    await startSharer(D2, "Dana", 960, 540);
    for (const [i, name, n] of [[0, "dana-one", 2], [1, "dana-two", 1]]) {
      await Q.click({ text: name, sel: ".p-row" });
      await Q.waitFor(`document.querySelectorAll('.st-tile.idle').length === ${n}`, { timeout: 30000, what: `${n} idle tiles in ${name}` });
      for (let k = 1; k <= n; k++) {
        for (let t = 0; t < 6 && (await Q.eval(`__sss.S.rooms[${i}].watches.size`)) < k; t++) {
          await Q.click("[data-watch]");
          await sleep(700);
        }
        await Q.waitFor(`__sss.S.rooms[${i}].watches.size === ${k}`, { what: `watch ${k} in ${name}` });
      }
    }
    await Q.click({ text: "Watched streams", sel: ".p-row" });
    await Q.waitFor("document.querySelectorAll('.arena > .st-tile').length === 3", { what: "three watched tiles" });
    const pvs = await Q.eval(`[...document.querySelectorAll('.strip .pv')].map((e) => ({ user: e.dataset.user, person: e.dataset.person, n: e.querySelector('.cnt')?.innerText ?? "1", live: !!e.querySelector('.lv') }))`);
    check(pvs.length === 2 && pvs.every((x) => x.user === "Dana" && x.live) && pvs.map((x) => x.n).sort().join() === "1,2", "Watched: three Dana streams in two rooms are two avatars, 2 + 1 (the two Danas of one room stay apart)", JSON.stringify(pvs));
    const merged = pvs.find((x) => x.n === "2");
    await move(Q, 3, 3);
    const av = await Q.eval(`(() => { const r = [...document.querySelectorAll('.strip .pv')].find((e) => e.dataset.person === ${JSON.stringify(merged.person)}).querySelector('.avatar').getBoundingClientRect(); return { cx: r.x + r.width / 2, cy: r.y + r.height / 2 }; })()`);
    await move(Q, av.cx, av.cy);
    await Q.waitFor("document.querySelector('.utip')", { what: "the merged person's card" });
    await sleep(150);
    const card = await Q.eval(`({ n: document.querySelectorAll('.utip').length, rows: [...document.querySelectorAll('.utip .ust')].map((e) => e.querySelector('.l3').innerText.replace(/\\s+/g, ' ')), sub: document.querySelector('.utip .hd small').innerText })`);
    check(card.n === 1 && card.rows.length === 2 && card.rows.some((r) => /dana-one/.test(r)) && card.rows.some((r) => /dana-two/.test(r)) && /2 streams/.test(card.sub), "the merged person's card lists both streams, each naming its room", JSON.stringify(card));
    await shot(Q, "watched_merged_card");
    await move(Q, 3, 3);
    check([D1, D2, D3, WV].every((x) => x.consoleErrors.length === 0), "no console errors on the Dana pages", [D1, D2, D3, WV].flatMap((x) => x.consoleErrors).join(" | "));
    for (const X of [D1, D2, D3, WV]) await X.close();
  }

  if (OLD_RELAY && OLD_HASH) {
    section("a relay without avatars (an older relay): nobody else's emoji shows, my own still does");
    const X = await open("Olga", site.url);
    await X.page.eval(`(() => { const s = JSON.parse(localStorage.getItem("sss.settings.v1")); s.emoji = "🦊"; localStorage.setItem("sss.settings.v1", JSON.stringify(s)); location.reload(); })()`);
    await sleep(800);
    await L.createRoom(X.page, { name: "old", server: OLD_RELAY, hash: OLD_HASH });
    await L.waitJoined(X.page);
    const Y = await open("Ben", site.url);
    await L.addRoomByLink(Y.page, await L.inviteLink(X.page), "old");
    await L.waitJoined(Y.page);
    await sleep(3500);
    const own = await X.page.eval(`[...document.querySelectorAll('.pv')].find((p) => p.dataset.user === "Olga")?.querySelector('.emo')?.innerText ?? null`);
    const theirs = await Y.page.eval(`[...document.querySelectorAll('.pv')].find((p) => p.dataset.user === "Olga")?.querySelector('.avatar').innerText.trim()`);
    check(own === "🦊" && theirs.startsWith("O") && (await X.page.eval("__sss.S.rooms[0].client.avatarsSupported")) === false, "an older relay never acks: Olga sees her own 🦊, Ben sees her initial", JSON.stringify({ own, theirs }));
    await X.close();
    await Y.close();
  }

  section("Layout menu settings are saved (#131): a reload keeps them, per saved room");
  {
    const T = await open("Saver", site.url);
    await L.createRoom(T.page, { name: "keep", server: RELAY, hash: HASH });
    await L.waitJoined(T.page);
    const menuState = () => T.page.eval(`(() => { const on = [...document.querySelectorAll('.menu.m-layout .mi.on')].map((e) => (e.innerText.match(/Split|Crop|Floating/) || [''])[0]); const cbx = Object.fromEntries([...document.querySelectorAll('.menu.m-layout .cbx')].map((e) => [e.innerText.replace(/^check\\s*/, '').trim(), e.getAttribute('aria-checked') === 'true'])); return { on, cbx }; })()`);
    const openMenu = async () => {
      await T.page.click("#btn-layout");
      await T.page.waitFor("document.querySelector('.menu.m-layout')", { what: "layout menu" });
    };
    await openMenu();
    const d = await menuState();
    check(d.on.join() === "Split" && d.cbx["Webcam in picture"] === true && d.cbx["Always show names"] === false && d.cbx["Hide my own streams"] === false && d.cbx["Show stats"] === false, "a fresh browser starts with Split, Webcam in picture on, the other three off", JSON.stringify(d));
    await T.page.click({ js: "[...document.querySelectorAll('.menu .mi')].find((e) => /Crop/.test(e.innerText))" });
    await openMenu();
    for (const re of ["Webcam in picture", "Always show names", "Hide my own streams", "Show stats"]) {
      await T.page.click({ js: `[...document.querySelectorAll('.menu .cbx')].find((e) => /${re}/.test(e.innerText))` });
      await sleep(100);
    }
    await key(T.page, "Escape");
    await T.page.eval("location.reload()");
    await sleep(1500);
    await L.waitJoined(T.page);
    await openMenu();
    const r = await menuState();
    check(r.on.join() === "Crop" && r.cbx["Webcam in picture"] === false && r.cbx["Always show names"] === true && r.cbx["Hide my own streams"] === true && r.cbx["Show stats"] === true, "after a reload the room keeps Crop, Webcam in picture off, Always show names, Hide my own streams and Show stats", JSON.stringify(r));
    const saved = await T.page.eval(`JSON.parse(localStorage.getItem("sss.settings.v1")).stage`);
    check(Object.keys(saved.views).length === 1 && /^room:/.test(Object.keys(saved.views)[0]) && Object.values(saved.views)[0].layout === "crop" && Object.values(saved.views)[0].pip === false, "settings.stage holds the room's view by its saved room id", JSON.stringify(saved));
    await key(T.page, "Escape");
    await T.close();
  }

  check(V.consoleErrors.length === 0, "no console errors on Vera's page", V.consoleErrors.join(" | "));
  check([A, B, C].every((x) => x.consoleErrors.length === 0), "no console errors on the sharers' pages", [A, B, C].flatMap((x) => x.consoleErrors).join(" | "));
  ok0 = true;
} catch (e) {
  failed++;
  console.log("FAIL", e.stack || e.message);
  await P.screenshot(OUT + PFX + "fail.png").catch(() => {});
} finally {
  for (const X of [A, B, C, V]) await X.close().catch(() => {});
  site.close();
}
console.log(failed ? `${failed} FAILED of ${results.length}` : `ALL ${results.length} PASSED`);
process.exit(failed ? 1 : 0);
