#!/usr/bin/env node
// Dev-only: run dev/scenarios.html in headless Chromium against a real relay; restarts the relay when the page asks.
//
//   node docs/dev/scenarios.mjs --relay 127.0.0.1:4433 --hash <cert hash> --relay-bin <bin> --relay-dir <scratch> [--only a,b] [--timeout 300]

import { spawnSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { launchChromium, startServer } from "./headless.mjs";

const here = dirname(fileURLToPath(import.meta.url));
const args = Object.fromEntries(process.argv.slice(2).reduce((a, x, i, arr) => (x.startsWith("--") ? [...a, [x.slice(2), arr[i + 1]]] : a), []));
const port = args.relay.split(":")[1];
const { server, port: httpPort, results, waitersResult, kv } = await startServer({ onLog: (id, m) => console.log(`  ${m}`) });
const url = `http://127.0.0.1:${httpPort}/simple-screensharing/dev/scenarios.html?id=0&relay=${args.relay}&hash=${args.hash}${args.only ? `&only=${encodeURIComponent(args.only)}` : ""}`;
const b = launchChromium(url, { flags: ["--disable-frame-rate-limit", "--disable-gpu-vsync"] });
let restarting = false;
const watcher = setInterval(async () => {
  if (kv.has("restart-relay") && !restarting) {
    restarting = true;
    console.log("  [runner] restarting the relay (graceful, certificate kept)");
    const r = spawnSync("bash", [resolve(here, "relay.sh"), "restart", args["relay-bin"], args["relay-dir"], port], { encoding: "utf8" });
    console.log(`  [runner] relay restarted: ${r.stdout.trim()} ${r.stderr.trim()}`);
    await fetch(`http://127.0.0.1:${httpPort}/kv/relay-restarted`, { method: "POST", body: "1" });
  }
}, 200);
let code = 1;
try {
  await new Promise((ok, fail) => {
    const t = setTimeout(() => fail(new Error("timeout")), Number(args.timeout ?? 300) * 1000);
    waitersResult.push(() => {
      if (results.size) {
        clearTimeout(t);
        ok();
      }
    });
    b.proc.on("exit", (c) => !results.size && fail(new Error(`chromium exited ${c}: ${b.stderr.slice(-1500)}`)));
  });
  const r = results.get("0");
  console.log(`\n${r.passed}/${r.total} scenarios passed`);
  for (const x of r.results) console.log(`${x.ok ? "PASS" : "FAIL"} ${String(x.ms).padStart(6)} ms  ${x.name}${x.ok ? "" : `\n        ${x.error}`}`);
  code = r.ok ? 0 : 1;
} catch (e) {
  console.error(String(e.message || e));
} finally {
  clearInterval(watcher);
  b.kill();
  server.close();
  server.closeAllConnections?.();
}
process.exit(code);
