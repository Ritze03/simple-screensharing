// Dev-only: a tiny Chrome DevTools Protocol driver (no npm deps, Node >= 22 for the global WebSocket) so scripts can
// drive the REAL site (docs/index.html) in headless or headed Chromium: click, type, evaluate, screenshot, read the
// console. Not part of the site.
//
//   import { launchBrowser } from "./cdp.mjs";
//   const b = await launchBrowser({ url, headed: false });
//   const page = b.page;           // the first tab
//   await page.eval("document.title");
//   await page.click(".btn");      // CSS selector or {text: "Live gehen"} (matches the innermost element whose text contains it)
//   await page.screenshot("/path/out.png");
//   b.consoleErrors                // page code errors (console.error / uncaught), as strings
//   b.netLines                     // Chromium's own stderr lines (ERR_QUIC_* etc.), separately
//   await b.close();
//
// Headed runs are only allowed with SSS_GUI_LOCKED=1 (hold the shared desktop lock; see docs/dev/README.md).

import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const BASE_FLAGS = [
  "--no-first-run",
  "--no-default-browser-check",
  "--disable-background-timer-throttling",
  "--disable-renderer-backgrounding",
  "--disable-backgrounding-occluded-windows",
  "--autoplay-policy=no-user-gesture-required",
  "--enable-features=WebCodecs,WebTransport",
  "--enable-logging=stderr",
  "--v=0",
];

class Conn {
  constructor(ws) {
    this.ws = ws;
    this.id = 0;
    this.pending = new Map();
    this.handlers = [];
    ws.addEventListener("message", (m) => {
      const msg = JSON.parse(m.data);
      if (msg.id && this.pending.has(msg.id)) {
        const { ok, fail } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        if (msg.error) fail(new Error(`${msg.error.message} (${msg.error.code})`));
        else ok(msg.result);
      } else if (msg.method) this.handlers.forEach((h) => h(msg));
    });
  }
  send(method, params = {}, sessionId) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params, sessionId }));
    return new Promise((ok, fail) => this.pending.set(id, { ok, fail }));
  }
  on(fn) {
    this.handlers.push(fn);
  }
}

export class Page {
  constructor(conn, sessionId, owner) {
    this.conn = conn;
    this.sid = sessionId;
    this.owner = owner;
  }
  send(method, params) {
    return this.conn.send(method, params, this.sid);
  }
  /** Evaluate an expression (awaits promises) and return its JSON value. */
  async eval(expression, { userGesture = true } = {}) {
    const r = await this.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true, userGesture });
    if (r.exceptionDetails) throw new Error(`eval failed: ${r.exceptionDetails.exception?.description || r.exceptionDetails.text}\n${expression.slice(0, 200)}`);
    return r.result.value;
  }
  async goto(url) {
    await this.send("Page.navigate", { url });
    await sleep(300);
  }
  async waitFor(expression, { timeout = 15000, every = 100, what = expression } = {}) {
    const t0 = Date.now();
    for (;;) {
      let v;
      try {
        v = await this.eval(expression, { userGesture: false });
      } catch {}
      if (v) return v;
      if (Date.now() - t0 > timeout) throw new Error(`timeout (${timeout} ms) waiting for: ${what}`);
      await sleep(every);
    }
  }
  /** Viewport-centre of the element for a selector or {text, sel?}. */
  async #locate(target) {
    const expr =
      typeof target === "string"
        ? `(() => { const e = document.querySelector(${JSON.stringify(target)}); if (!e) return null; e.scrollIntoView({block:"center"}); const r = e.getBoundingClientRect(); return {x: r.x + r.width/2, y: r.y + r.height/2}; })()`
        : `(() => { const want = ${JSON.stringify(target.text)}; const sel = ${JSON.stringify(target.sel || "button, [role=button], a, label, .btn, [tabindex], div, span")};
             let best = null; for (const e of document.querySelectorAll(sel)) { const tx = (e.innerText || "").trim(); if (!tx.includes(want)) continue;
             const r = e.getBoundingClientRect(); if (!r.width || !r.height) continue; const area = r.width * r.height; if (!best || area < best.area) best = {e, area}; }
             if (!best) return null; best.e.scrollIntoView({block:"center"}); const r = best.e.getBoundingClientRect(); return {x: r.x + r.width/2, y: r.y + r.height/2}; })()`;
    return this.eval(expr, { userGesture: false });
  }
  async click(target, { timeout = 10000 } = {}) {
    let pos = null;
    const t0 = Date.now();
    while (!(pos = await this.#locate(target))) {
      if (Date.now() - t0 > timeout) throw new Error(`click: no element for ${JSON.stringify(target)}`);
      await sleep(100);
    }
    await this.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: pos.x, y: pos.y });
    await this.send("Input.dispatchMouseEvent", { type: "mousePressed", x: pos.x, y: pos.y, button: "left", clickCount: 1 });
    await this.send("Input.dispatchMouseEvent", { type: "mouseReleased", x: pos.x, y: pos.y, button: "left", clickCount: 1 });
    await sleep(120);
  }
  async hover(target) {
    const pos = await this.#locate(target);
    if (!pos) throw new Error(`hover: no element for ${JSON.stringify(target)}`);
    await this.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: pos.x, y: pos.y });
  }
  /** Focus an input (selector), select its content and type text. */
  async type(selector, text) {
    await this.eval(`(() => { const e = document.querySelector(${JSON.stringify(selector)}); e.focus(); e.select && e.select(); })()`, { userGesture: false });
    await this.send("Input.insertText", { text });
    await sleep(80);
  }
  async key(key) {
    const k = { Enter: { key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 }, Escape: { key: "Escape", code: "Escape", windowsVirtualKeyCode: 27 } }[key] || { key };
    await this.send("Input.dispatchKeyEvent", { type: "keyDown", ...k });
    await this.send("Input.dispatchKeyEvent", { type: "keyUp", ...k });
  }
  async screenshot(file, { fullPage = false } = {}) {
    const r = await this.send("Page.captureScreenshot", { format: "png", captureBeyondViewport: fullPage });
    writeFileSync(file, Buffer.from(r.data, "base64"));
    return file;
  }
  async setViewport(width, height, scale = 1) {
    await this.send("Emulation.setDeviceMetricsOverride", { width, height, deviceScaleFactor: scale, mobile: false });
  }
}

export async function launchBrowser({ url = "about:blank", headed = false, flags = [], bin = "/usr/bin/chromium", userDataDir, viewport = [1440, 900], grantClipboard = true } = {}) {
  if (headed && !process.env.SSS_GUI_LOCKED) throw new Error("headed Chromium needs the shared GUI lock (SSS_GUI_LOCKED=1, see docs/dev/README.md)");
  const dir = userDataDir || mkdtempSync(join(tmpdir(), "sss-cdp-"));
  const args = [...BASE_FLAGS, ...(headed ? [] : ["--headless=new"]), "--remote-debugging-port=0", `--user-data-dir=${dir}`, ...flags, "about:blank"];
  const proc = spawn(bin, args, { stdio: ["ignore", "ignore", "pipe"] });
  const netLines = [];
  let buf = "";
  proc.stderr.on("data", (d) => {
    buf += d;
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const l = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (l) netLines.push(l);
    }
  });
  const portFile = join(dir, "DevToolsActivePort");
  for (let i = 0; i < 200 && !existsSync(portFile); i++) await sleep(50);
  if (!existsSync(portFile)) throw new Error(`Chromium did not start:\n${netLines.slice(-10).join("\n")}`);
  const [port, path] = readFileSync(portFile, "utf8").trim().split("\n");
  const ws = new WebSocket(`ws://127.0.0.1:${port}${path}`);
  await new Promise((ok, fail) => {
    ws.addEventListener("open", ok);
    ws.addEventListener("error", fail);
  });
  const conn = new Conn(ws);
  const consoleErrors = [];
  const consoleAll = [];
  conn.on((m) => {
    if (m.method === "Runtime.consoleAPICalled") {
      const text = m.params.args.map((a) => a.value ?? a.description ?? "").join(" ");
      consoleAll.push(`[${m.params.type}] ${text}`);
      if (m.params.type === "error") consoleErrors.push(text);
    } else if (m.method === "Runtime.exceptionThrown") {
      const e = m.params.exceptionDetails;
      consoleErrors.push(`uncaught: ${e.exception?.description || e.text}`);
      consoleAll.push(`[exception] ${e.exception?.description || e.text}`);
    } else if (m.method === "Log.entryAdded") {
      const e = m.params.entry;
      consoleAll.push(`[log.${e.level}] ${e.text} ${e.url || ""}`);
      if (e.level === "error") consoleErrors.push(`${e.text} ${e.url || ""}`);
    }
  });
  const { targetInfos } = await conn.send("Target.getTargets");
  const tgt = targetInfos.find((t) => t.type === "page");
  const { sessionId } = await conn.send("Target.attachToTarget", { targetId: tgt.targetId, flatten: true });
  const page = new Page(conn, sessionId);
  for (const d of ["Page", "Runtime", "Log"]) await page.send(`${d}.enable`);
  if (grantClipboard) {
    await conn.send("Browser.grantPermissions", { permissions: ["clipboardReadWrite", "clipboardSanitizedWrite"], origin: new URL(url).origin }).catch(() => {});
  }
  if (!headed) await page.setViewport(viewport[0], viewport[1]);
  if (url !== "about:blank") await page.goto(url);
  return {
    proc,
    page,
    conn,
    dir,
    netLines,
    consoleErrors,
    consoleAll,
    async newPage(u) {
      const { targetId } = await conn.send("Target.createTarget", { url: u });
      const { sessionId: sid } = await conn.send("Target.attachToTarget", { targetId, flatten: true });
      const p = new Page(conn, sid);
      for (const d of ["Page", "Runtime", "Log"]) await p.send(`${d}.enable`);
      return p;
    },
    async close() {
      try {
        ws.close();
      } catch {}
      try {
        proc.kill("SIGKILL");
      } catch {}
      if (!userDataDir) {
        await sleep(200);
        try {
          rmSync(dir, { recursive: true, force: true });
        } catch {}
      }
    },
  };
}
