// Pure tests for avsync.js (mirror of the Rust tests in client/src/decode/avsync.rs). Run: node docs/js/core/avsync.test.mjs
import assert from "node:assert/strict";
import { AvSync, DelayController, LatencyFloor, MAX_AUDIO_SYNC_DELAY_MS, targetDelayMs } from "./avsync.js";

const LEAD = 80;
function feed(f, t0, n, fps, latMs) {
  const stepMs = 1000 / fps;
  let t = t0;
  for (let i = 0; i < n; i++) {
    f.observe(t + latMs, t * 1000);
    t += stepMs;
  }
  return t;
}
const tests = [];
const test = (name, fn) => tests.push([name, fn]);

test("floor is the minimum, not the jittery latest", () => {
  const f = new LatencyFloor();
  f.observe(100, 0);
  f.observe(180, 50_000);
  f.observe(230, 100_000);
  assert.equal(f.floor(), 100);
});

test("floor follows a rise after the window and a fall at once; held when idle", () => {
  const f = new LatencyFloor();
  let t = feed(f, 0, 120, 60, 50);
  t = feed(f, t, 60, 60, 750);
  assert.ok(Math.abs(f.floor() - 50) < 1e-6);
  t = feed(f, t, 240, 60, 750);
  assert.ok(Math.abs(f.floor() - 750) < 1e-6);
  assert.ok(Math.abs(f.floor() - 750) < 1e-6); // idle: held
  feed(f, t, 3, 60, 50);
  assert.ok(Math.abs(f.floor() - 50) < 1e-6);
});

test("clock offset cancels; target = video - audio - audio lead", () => {
  const off = 5 * 3600 * 1000;
  const v = new LatencyFloor(), a = new LatencyFloor();
  feed(v, off, 120, 60, 750);
  feed(a, off, 100, 50, 30);
  assert.ok(Math.abs(targetDelayMs(v.floor(), a.floor(), true, LEAD) - (750 - 30 - LEAD)) < 1e-6);
});

test("target: audio-only, missing samples, negative, cap, incomparable clocks", () => {
  assert.equal(targetDelayMs(800, 0, false, LEAD), 0);
  assert.equal(targetDelayMs(null, 0, true, LEAD), 0);
  assert.equal(targetDelayMs(40, 30, true, LEAD), 0);
  assert.equal(targetDelayMs(2500, 0, true, LEAD), MAX_AUDIO_SYNC_DELAY_MS);
  assert.equal(targetDelayMs(1000, -200000, true, LEAD), 0);
  assert.equal(targetDelayMs(-90000, 500, true, LEAD), 0);
});

test("controller: deadband, rate limit, one step for a big change", () => {
  const c = new DelayController();
  assert.equal(c.update(30, 0), 0);
  assert.equal(c.update(700, 10), 700);
  assert.equal(c.update(740, 2000), 700);
  assert.equal(c.update(665, 2100), 700);
  const d = new DelayController();
  d.update(400, 0);
  assert.equal(d.update(600, 100), 400);
  assert.equal(d.update(700, 400), 400);
  assert.equal(d.update(700, 600), 700);
});

test("AvSync: stalled keepalives ignored, video gone -> 0", () => {
  const s = new AvSync();
  s.videoStarted();
  for (let i = 0; i < 120; i++) {
    const t = i * 16;
    s.noteVideo(t + 750, t * 1000);
    s.noteAudio(t + 30, t * 1000);
  }
  s.noteVideo(2000 + 5, 2_000_000, true); // a stalled keepalive with no latency must not matter
  assert.ok(Math.abs(s.targetDelayMs(LEAD) - (720 - LEAD)) < 20);
  s.videoGone();
  assert.equal(s.targetDelayMs(LEAD), 0);
});

let failed = 0;
for (const [name, fn] of tests) {
  try {
    fn();
    console.log("ok   ", name);
  } catch (e) {
    failed++;
    console.log("FAIL ", name, "\n", e.message);
  }
}
process.exit(failed ? 1 : 0);
