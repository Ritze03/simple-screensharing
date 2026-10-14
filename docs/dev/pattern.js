// Synthetic capture source for the dev harness: a canvas.captureStream() test pattern (noisy rectangles so the encoder has
// real work, 16-bit frame counter + 24-bit wall-clock ms as 40 black/white blocks so a viewer can verify continuity and
// measure latency) plus an optional 440 Hz tone as the audio track.

// ---- test pattern: 16-bit frame counter + 24-bit wall clock ms as 40 black/white blocks, plus noisy rectangles ----
const BLOCK = 12;
const BITS = 40;
export function makePatternSource(width, height, targetFps, { audio = false } = {}) {
  const canvas = document.createElement("canvas");
  canvas.width = width;
  canvas.height = height;
  canvas.style.cssText = "width:480px;height:270px;background:#000";
  document.body.appendChild(canvas);
  const ctx = canvas.getContext("2d", { alpha: false });
  const stream = canvas.captureStream(0);
  const track = stream.getVideoTracks()[0];
  let audioCtx = null;
  if (audio) {
    // A 440 Hz tone as the "captured" audio, so the audio share path (AudioEncoder -> Opus -> relay) is exercised too.
    audioCtx = new AudioContext({ sampleRate: 48000 });
    const osc = audioCtx.createOscillator();
    const gain = audioCtx.createGain();
    gain.gain.value = 0.4;
    const dest = audioCtx.createMediaStreamDestination();
    osc.frequency.value = 440;
    osc.connect(gain).connect(dest);
    osc.start();
    stream.addTrack(dest.stream.getAudioTracks()[0]);
  }
  let n = 0;
  let stopped = false;
  let paused = false; // static screen: no new frames at all (like a damage-driven screen capture)
  const period = 1000 / targetFps;
  let next = performance.now();
  const stats = { generated: 0, firstAt: 0, lastAt: 0 };
  const draw = () => {
    ctx.fillStyle = `hsl(${(n * 3) % 360} 40% 25%)`;
    ctx.fillRect(0, 0, width, height);
    for (let i = 0; i < 40; i++) {
      ctx.fillStyle = `rgb(${(Math.random() * 255) | 0} ${(Math.random() * 255) | 0} ${(Math.random() * 255) | 0})`;
      const w = 40 + Math.random() * 300;
      const h = 40 + Math.random() * 200;
      ctx.fillRect(BLOCK * 2 + Math.random() * (width - w - BLOCK * 2), BLOCK * 3 + Math.random() * (height - h - BLOCK * 3), w, h);
    }
    ctx.fillStyle = "#808080";
    ctx.fillRect(0, 0, BLOCK * (BITS + 2), BLOCK * 2);
    const ts = Date.now() & 0xffffff;
    const value = (BigInt(n & 0xffff) << 24n) | BigInt(ts);
    for (let b = 0; b < BITS; b++) {
      ctx.fillStyle = (value >> BigInt(BITS - 1 - b)) & 1n ? "#fff" : "#000";
      ctx.fillRect(BLOCK * (1 + b), BLOCK / 2, BLOCK, BLOCK);
    }
  };
  const tick = () => {
    if (stopped) return;
    const now = performance.now();
    if (now >= next && !paused) {
      draw();
      track.requestFrame();
      n++;
      stats.generated++;
      if (!stats.firstAt) stats.firstAt = now;
      stats.lastAt = now;
      next += period;
      if (next < now - period * 3) next = now + period; // fell behind: skip instead of bursting
    }
    setTimeout(tick, Math.max(0, next - performance.now() - 0.5));
  };
  tick();
  return {
    stream,
    stats,
    pause() {
      paused = true;
    },
    resume() {
      paused = false;
      next = performance.now();
    },
    stop() {
      stopped = true;
      track.stop();
      if (audioCtx) audioCtx.close().catch(() => {});
      canvas.remove();
    },
  };
}

// Read the 40-bit pattern out of a decoded frame (async copy of the top strip, RGBA).
export async function readPattern(frame) {
  const w = BLOCK * (BITS + 2);
  const rect = { x: 0, y: 0, width: w, height: BLOCK * 2 };
  const buf = new Uint8Array(w * BLOCK * 2 * 4);
  await frame.copyTo(buf, { rect, format: "RGBA" });
  let v = 0n;
  for (let b = 0; b < BITS; b++) {
    const x = BLOCK * (1 + b) + BLOCK / 2;
    const y = BLOCK;
    const o = (y * w + x) * 4;
    const lum = (buf[o] + buf[o + 1] + buf[o + 2]) / 3;
    v = (v << 1n) | (lum > 128 ? 1n : 0n);
  }
  return { counter: Number(v >> 24n), ts: Number(v & 0xffffffn) };
}

