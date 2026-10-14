#!/usr/bin/env node
// Dev-only: native sharer (a Rust program built outside the repo, using the real client lib + GStreamer) -> relay -> headless
// Chromium viewer. Verifies the browser decodes what native senders actually put on the wire.
//
//   node docs/dev/native.mjs --relay 127.0.0.1:4433 --hash <cert hash> --bin <nativesharer> [--codec h264] [--backend sw|va]
//        [--fps 60] [--size 1920x1080] [--bitrate 8000] [--duration 8] [--audio] [--late-viewer 33] [--flag <chromium flag>]
//
// --late-viewer N also starts a second viewer N seconds after the sharer to prove the relay replays the cached preview.

import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { writeFileSync } from "node:fs";
import { launchChromium, startServer } from "./headless.mjs";

const BASE = "/simple-screensharing/";
const b64url = (b) => b.toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

function parseArgs(argv) {
  const o = { codec: "h264", backend: "sw", fps: "60", size: "1920x1080", bitrate: "8000", duration: "8", flags: [], hw: "no-preference" };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) continue;
    const k = a.slice(2);
    if (k === "flag") o.flags.push(argv[++i]);
    else if (k === "audio") o.audio = true;
    else o[k] = argv[++i];
  }
  return o;
}

export async function runNative(o) {
  const link = `${o.relay}|${randomBytes(16).toString("hex")}|${b64url(randomBytes(32))}${o.hash ? `|${o.hash}` : ""}`;
  const lateS = o["late-viewer"] ? Number(o["late-viewer"]) : 0;
  const seconds = Number(o.duration) + 12 + lateS + (lateS ? Number(o.duration) : 0);
  const { server, port, results, waitersResult } = await startServer({ onLog: (id, m) => process.env.VERBOSE && console.log(`[${id}] ${m}`) });
  const url = (id, name) =>
    `http://127.0.0.1:${port}${BASE}dev/live.html?id=${id}&role=view&name=${name}&owner=native&pattern=none&link=${encodeURIComponent(link)}&duration=${o.duration}&hw=${o.hw}&audio=${o.audio ? 1 : 0}`;
  const procs = [];
  const args = [link, "--codec", o.codec, "--backend", o.backend, "--fps", o.fps, "--size", o.size, "--bitrate", o.bitrate, "--seconds", String(seconds), ...(o.audio ? ["--audio"] : [])];
  const sharer = spawn(o.bin, args, { stdio: ["ignore", "pipe", "pipe"], env: { ...process.env, GST_DEBUG: "0" } });
  let sharerLog = "";
  sharer.stderr.on("data", (d) => (sharerLog += d));
  sharer.stdout.on("data", (d) => (sharerLog += d));
  const want = lateS ? 2 : 1;
  try {
    await sleep(3000);
    procs.push(launchChromium(url(0, "viewer"), { flags: o.flags }));
    if (lateS) {
      await sleep(lateS * 1000);
      procs.push(launchChromium(url(1, "late-viewer"), { flags: o.flags }));
    }
    await new Promise((ok, fail) => {
      const t = setTimeout(() => fail(new Error(`timeout; results for [${[...results.keys()]}]\n${sharerLog.slice(-1500)}`)), (seconds + 40) * 1000);
      const check = () => {
        if (results.size >= want) {
          clearTimeout(t);
          ok();
        }
      };
      waitersResult.push(check);
      sharer.on("exit", (c) => c && results.size < want && fail(new Error(`native sharer exited ${c}:\n${sharerLog.slice(-2500)}`)));
    });
    return { viewer: results.get("0"), lateViewer: results.get("1"), sharerLog };
  } finally {
    if (process.env.CHROME_STDERR) procs.forEach((p, i) => console.error(`--- chromium ${i} stderr ---\n${p.stderr.slice(-6000)}`));
    procs.forEach((p) => p.kill());
    sharer.kill("SIGTERM");
    await sleep(300);
    sharer.kill("SIGKILL");
    server.close();
    server.closeAllConnections?.();
  }
}

async function main() {
  const o = parseArgs(process.argv.slice(2));
  const r = await runNative(o);
  const brief = (v) =>
    v && {
      error: v.error,
      stream: v.stream && { label: v.stream.label, size: `${v.stream.width}x${v.stream.height}`, owner: v.stream.ownerName },
      receiver: v.receiver && { ...v.receiver, samples: undefined },
      audio: v.audio,
      previews: v.previews,
      events: (v.events || []).filter((e) => e.t === 'media' || e.t === 'error').slice(0, 12),
    };
  console.log(JSON.stringify({ viewer: brief(r.viewer), lateViewer: brief(r.lateViewer) }, null, 2));
  console.log("--- native sharer log (tail) ---\n" + r.sharerLog.split("\n").slice(-12).join("\n"));
  if (o.snapshot && r.viewer?.snapshotJpegDataUrl) writeFileSync(o.snapshot, Buffer.from(r.viewer.snapshotJpegDataUrl.split(",")[1], "base64"));
  if (o.out) writeFileSync(o.out, JSON.stringify(r, null, 2));
  process.exit(r.viewer?.error || r.lateViewer?.error ? 1 : 0);
}

import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
