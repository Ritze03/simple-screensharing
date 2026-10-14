#!/usr/bin/env node
// Dev-only: browser sharer -> relay -> browser viewer, one headless Chromium per peer, for each codec x fps.
//
//   node docs/dev/matrix.mjs --relay 127.0.0.1:4433 --hash <relay cert hash> [--codecs h264,h265,av1]
//        [--fps 60,120,240] [--size 1920x1080] [--bitrate 8000] [--duration 10] [--hw no-preference] [--flag <chromium flag>]
//
// Needs a running relay (dev/relay.sh start …). Prints a table plus JSON; kills every Chromium it starts.

import { spawn } from "node:child_process";
import { randomBytes } from "node:crypto";
import { writeFileSync } from "node:fs";
import { launchChromium, startServer } from "./headless.mjs";

const BASE = "/simple-screensharing/";

function parseArgs(argv) {
  const o = { codecs: "h264", fps: "60", size: "1920x1080", bitrate: "8000", duration: "10", hw: "no-preference", flags: [], preset: "balanced" };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) continue;
    const k = a.slice(2);
    if (k === "flag") o.flags.push(argv[++i]);
    else o[k] = argv[++i];
  }
  return o;
}

const b64url = (b) => b.toString("base64").replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");

export async function runPair({ relay, hash, codec, fps, size, bitrate, duration, hw, flags, preset, extra = "", timeoutS, nativeViewer = null }) {
  const [w, h] = size.split("x").map(Number);
  const link = `${relay}|${randomBytes(16).toString("hex")}|${b64url(randomBytes(32))}${hash ? `|${hash}` : ""}`;
  const { server, port, results, waitersResult } = await startServer({ onLog: (id, m) => process.env.VERBOSE && console.log(`[${id}] ${m}`) });
  const common = `link=${encodeURIComponent(link)}&codec=${codec}&fps=${fps}&w=${w}&h=${h}&bitrate=${bitrate}&duration=${duration}&hw=${hw}&preset=${preset}${extra}`;
  const url = (id, role, name) => `http://127.0.0.1:${port}${BASE}dev/live.html?id=${id}&role=${role}&name=${name}&${common}`;
  const procs = [];
  const timeout = (timeoutS ?? Number(duration) + 70) * 1000;
  try {
    let nativeOut = "";
    let nativeProc = null;
    if (nativeViewer) {
      // The native viewer (real client lib + real StreamDecoder) replaces the browser viewer: browser -> native direction.
      nativeProc = spawn(nativeViewer, [link, "--seconds", String(Number(duration) + 8), "--owner", "sharer"], { stdio: ["ignore", "pipe", "pipe"] });
      nativeProc.stdout.on("data", (d) => (nativeOut += d));
      nativeProc.stderr.on("data", (d) => process.env.VERBOSE && process.stderr.write(`[native] ${d}`));
    } else {
      procs.push(launchChromium(url(0, "view", "viewer"), { flags }));
    }
    await new Promise((r) => setTimeout(r, 800));
    procs.push(launchChromium(url(1, "share", "sharer"), { flags }));
    const sharerIdx = nativeViewer ? 0 : 1;
    await new Promise((ok, fail) => {
      const t = setTimeout(() => fail(new Error(`timeout; have results for [${[...results.keys()]}]`)), timeout);
      const check = () => {
        if (nativeViewer ? results.has("1") : results.size >= 2) {
          clearTimeout(t);
          ok();
        }
      };
      waitersResult.push(check);
      procs.forEach((p, i) => p.proc.on("exit", (c) => !results.has(String(nativeViewer ? 1 : i)) && fail(new Error(`chromium exited (${c}): ${p.stderr.slice(-1500)}`))));
    });
    if (nativeProc) {
      await new Promise((r) => (nativeProc.exitCode !== null ? r() : nativeProc.on("exit", r)));
      const line = nativeOut.split("\n").find((l) => l.startsWith("RESULT "));
      return { nativeViewer: line ? JSON.parse(line.slice(7)) : { error: "native viewer printed no RESULT", out: nativeOut }, sharer: results.get("1"), link };
    }
    return { viewer: results.get("0"), sharer: results.get("1"), link };
  } finally {
    procs.forEach((p) => p.kill());
    server.close();
    server.closeAllConnections?.();
  }
}

async function main() {
  const o = parseArgs(process.argv.slice(2));
  const rows = [];
  const all = [];
  for (const codec of o.codecs.split(",")) {
    for (const fps of o.fps.split(",")) {
      process.stdout.write(`${codec} @${fps}fps ${o.size} ${o.bitrate}kbps … `);
      let r;
      try {
        r = await runPair({ relay: o.relay, hash: o.hash, codec, fps, size: o.size, bitrate: o.bitrate, duration: o.duration, hw: o.hw, flags: o.flags, preset: o.preset, extra: o.audio ? "&audio=1" : "", nativeViewer: o["native-viewer"] || null });
      } catch (e) {
        console.log(`FAILED: ${e.message}`);
        rows.push({ codec, fps, error: e.message });
        continue;
      }
      const s = r.sharer || {};
      if (r.nativeViewer) {
        console.log(`native viewer: ${JSON.stringify(r.nativeViewer)}`);
        console.log(`  sharer: src ${s.source?.generatedFps} enc ${s.sender?.avgEncodedFps} fps, ${s.sender?.avgKbps} kbps, codec ${s.codecString}, keyframes ${s.sender?.keyframes}${s.error ? `, ERROR ${s.error.split("\n")[0]}` : ""}`);
        rows.push({ codec, reqFps: Number(fps), ...r.nativeViewer });
        continue;
      }
      const v = r.viewer || {};
      const row = {
        codec, reqFps: Number(fps),
        codecString: s.codecString,
        srcFps: s.source?.generatedFps, capFps: s.sender?.capturedFps, encFps: s.sender?.avgEncodedFps, sendKbps: s.sender?.avgKbps, keyframes: s.sender?.keyframes,
        txDropped: s.sender?.txDropped, encDropped: s.sender?.droppedBackpressure,
        rxFps: v.receiver?.avgReceivedFps, decFps: v.receiver?.avgDecodedFps, drawnFps: v.receiver?.drawnFps, rxKbps: v.receiver?.avgKbps,
        loss: v.receiver?.lossEvents, lat50: v.receiver?.latencyMs?.p50, lat95: v.receiver?.latencyMs?.p95, bufMs: v.receiver?.bufferMs,
        res: v.receiver?.resolution, error: s.error || v.error,
      };
      rows.push(row);
      all.push({ row, sharer: s, viewer: { ...v, snapshotJpegDataUrl: undefined } });
      console.log(row.error ? `ERROR ${row.error.split("\n")[0]}` : `src ${row.srcFps} enc ${row.encFps} -> rx ${row.rxFps} dec ${row.decFps} fps, p50 ${row.lat50} ms`);
      if (v.snapshotJpegDataUrl && o.snapshots) {
        writeFileSync(`${o.snapshots}/${codec}-${fps}.jpg`, Buffer.from(v.snapshotJpegDataUrl.split(",")[1], "base64"));
      }
    }
  }
  console.table(rows);
  if (o.out) writeFileSync(o.out, JSON.stringify(all, null, 2));
}

import { fileURLToPath } from "node:url";
import { resolve } from "node:path";
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
