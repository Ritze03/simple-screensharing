#!/usr/bin/env node
// Dev-only helper (not part of the site): static server + headless Chromium runner for the test pages.
//
//   node docs/dev/headless.mjs serve [--port 8123]
//        Serve docs/ under /simple-screensharing/ (like GitHub Pages) until Ctrl-C.
//
//   node docs/dev/headless.mjs run dev/tests.html [--timeout 60]
//   node docs/dev/headless.mjs run dev/live.html?role=share ... [--page url]...
//        Serve, launch one headless Chromium per --page (default: the single positional page), wait until every
//        page POSTed its /result, print the results as JSON and exit non-zero if any page reported failure.
//
// Pages talk back through POST /log (console), POST /result?id=N (final JSON) and GET/POST /kv/<name>
// (rendezvous between pages: GET long-polls until the value is set).
// Everything started here is killed before exit.

import { spawn } from "node:child_process";
import { createServer } from "node:http";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const DOCS = resolve(HERE, "..");
const BASE = "/simple-screensharing/";
const MIME = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript", ".json": "application/json", ".css": "text/css", ".md": "text/plain", ".svg": "image/svg+xml" };

export function startServer({ port = 0, onLog = (id, m) => console.log(m), docs = DOCS } = {}) {
  const results = new Map();
  const kv = new Map();
  const waiters = new Map();
  const waitersResult = [];
  const server = createServer(async (req, res) => {
    const url = new URL(req.url, "http://x");
    const send = (code, body = "", type = "text/plain") => {
      res.writeHead(code, { "content-type": type, "cache-control": "no-store" });
      res.end(body);
    };
    const body = async () => {
      const chunks = [];
      for await (const c of req) chunks.push(c);
      return Buffer.concat(chunks).toString("utf8");
    };
    if (req.method === "POST" && url.pathname === "/log") {
      onLog(url.searchParams.get("id"), await body());
      return send(204);
    }
    if (req.method === "POST" && url.pathname === "/result") {
      const id = url.searchParams.get("id") ?? "0";
      results.set(id, JSON.parse((await body()) || "null"));
      waitersResult.forEach((f) => f());
      return send(204);
    }
    if (url.pathname.startsWith("/kv/")) {
      const name = url.pathname.slice(4);
      if (req.method === "POST") {
        kv.set(name, await body());
        (waiters.get(name) || []).forEach((f) => f());
        waiters.delete(name);
        return send(204);
      }
      if (kv.has(name)) return send(200, kv.get(name));
      const t = setTimeout(() => send(204), 55_000);
      const arr = waiters.get(name) || [];
      arr.push(() => {
        clearTimeout(t);
        send(200, kv.get(name));
      });
      waiters.set(name, arr);
      return;
    }
    if (url.pathname.startsWith(BASE) || url.pathname === BASE.slice(0, -1)) {
      let rel = normalize(decodeURIComponent(url.pathname.slice(BASE.length)));
      if (rel === "." || rel === "") rel = "index.html";
      const file = join(docs, rel);
      if (!file.startsWith(docs)) return send(403);
      try {
        const st = statSync(file);
        if (st.isDirectory()) return send(404);
        return send(200, readFileSync(file), MIME[extname(file)] || "application/octet-stream");
      } catch {
        return send(404, "not found");
      }
    }
    send(404);
  });
  return new Promise((ok) => {
    server.listen(port, "127.0.0.1", () => ok({ server, port: server.address().port, results, waitersResult, kv }));
  });
}

const DEFAULT_FLAGS = [
  "--headless=new",
  "--no-first-run",
  "--no-default-browser-check",
  "--disable-background-timer-throttling",
  "--disable-renderer-backgrounding",
  "--disable-backgrounding-occluded-windows",
  "--autoplay-policy=no-user-gesture-required",
  "--enable-features=WebCodecs,WebTransport",
];

export function launchChromium(url, { flags = [], userDataDir, bin = "/usr/bin/chromium" } = {}) {
  const dir = userDataDir || mkdtempSync(join(tmpdir(), "ssshare-chromium-"));
  // HEADED=1 drops --headless (real window; only ever do this on a monitor you own, see the testing notes) - needed for GPU video paths.
  if (process.env.HEADED && !process.env.SSS_GUI_LOCKED) {
    throw new Error("HEADED=1 needs the shared GUI lock: run under `exec 9>/tmp/claude-1000/sss-gui.lock && flock 9` and set SSS_GUI_LOCKED=1 (see the testing notes)");
  }
  const base = process.env.HEADED ? DEFAULT_FLAGS.filter((f) => !f.startsWith("--headless")) : DEFAULT_FLAGS;
  const proc = spawn(bin, [...base, `--user-data-dir=${dir}`, ...flags, url], { stdio: ["ignore", "ignore", "pipe"] });
  let stderr = "";
  proc.stderr.on("data", (d) => (stderr += d));
  return {
    proc,
    get stderr() {
      return stderr;
    },
    kill() {
      try {
        proc.kill("SIGKILL");
      } catch {}
      if (!userDataDir) {
        try {
          rmSync(dir, { recursive: true, force: true });
        } catch {}
      }
    },
  };
}

async function main() {
  const args = process.argv.slice(2);
  const cmd = args.shift();
  const opt = (name, dflt) => {
    const i = args.indexOf(name);
    if (i < 0) return dflt;
    const v = args[i + 1];
    args.splice(i, 2);
    return v;
  };
  const optAll = (name) => {
    const out = [];
    for (let i; (i = args.indexOf(name)) >= 0; ) out.push(...args.splice(i, 2).slice(1));
    return out;
  };
  if (cmd === "serve") {
    const { port } = await startServer({ port: Number(opt("--port", 8123)) });
    console.log(`serving ${DOCS} at http://127.0.0.1:${port}${BASE}`);
    return;
  }
  if (cmd !== "run") {
    console.error("usage: headless.mjs serve | run <page> [--page <page>]... [--timeout s] [--flag <chromium flag>]...");
    process.exit(2);
  }
  const timeout = Number(opt("--timeout", 60)) * 1000;
  const outFile = opt("--out", null);
  const extraFlags = optAll("--flag");
  const pages = optAll("--page");
  if (args.length) pages.unshift(args[0]);
  const { server, port, results, waitersResult } = await startServer({ onLog: (id, m) => console.log(`[page ${id}] ${m}`) });
  const browsers = [];
  const cleanup = () => {
    browsers.forEach((b) => b.kill());
    server.close();
    server.closeAllConnections?.();
  };
  process.on("SIGINT", () => {
    cleanup();
    process.exit(130);
  });
  try {
    pages.forEach((p, i) => {
      const sep = p.includes("?") ? "&" : "?";
      const url = `http://127.0.0.1:${port}${BASE}${p}${sep}id=${i}&host=127.0.0.1:${port}`;
      browsers.push(launchChromium(url, { flags: extraFlags }));
    });
    await new Promise((ok, fail) => {
      const t = setTimeout(() => fail(new Error(`timeout after ${timeout / 1000}s; got results for pages [${[...results.keys()]}]`)), timeout);
      const check = () => {
        if (results.size >= pages.length) {
          clearTimeout(t);
          ok();
        }
      };
      waitersResult.push(check);
      browsers.forEach((b, i) => b.proc.on("exit", (code) => !results.has(String(i)) && code !== null && fail(new Error(`chromium ${i} exited early (${code}):\n${b.stderr.slice(-2000)}`))));
      check();
    });
    const out = pages.map((_, i) => results.get(String(i)));
    if (outFile) writeFileSync(outFile, JSON.stringify(out.length === 1 ? out[0] : out));
    // keep the console readable: big blobs (reverse vectors) stay in the --out file
    const shown = JSON.parse(JSON.stringify(out.length === 1 ? out[0] : out, (k, v) => (k === "reverse" ? "[omitted, use --out]" : v)));
    console.log(JSON.stringify(shown, null, 2));
    const failed = out.some((r) => r && (r.ok === false || r.error));
    cleanup();
    process.exit(failed ? 1 : 0);
  } catch (e) {
    console.error(String(e.message || e));
    cleanup();
    process.exit(1);
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
