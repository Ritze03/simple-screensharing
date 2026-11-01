// Viewer-side audio/video sync, the browser twin of client/src/decode/avsync.rs (same constants, same maths).
// Write-up with the why: superdoc/features/audio-sharing.md.
//
// All of one sender's streams stamp wire PTS from one clock (capture time), so a video frame and an audio packet
// captured together have equal PTS. The viewer does not know that clock's offset to its own, and does not need to:
//
//   floor(stream) = windowed minimum of (arrival - pts)             // clock offset included, identical in both
//   delta         = floor(video presentation) - floor(audio arrival)
//   extra delay   = clamp(delta - audio's own latency, 0, MAX_AUDIO_SYNC_DELAY_MS)
//
// "video presentation" is when the jitter buffer will actually release the frame (decodeAt), so the video side's own
// buffering is matched too. The applied delay moves only past a deadband and at most once per interval, so audio is
// re-timed in rare discrete steps, never continuously.

export const MAX_AUDIO_SYNC_DELAY_MS = 2000;
export const PLAUSIBLE_DELTA_MIN_MS = -1000;
export const PLAUSIBLE_DELTA_MAX_MS = MAX_AUDIO_SYNC_DELAY_MS + 1000;
export const FLOOR_WINDOW_MS = 3000;
export const FLOOR_BUCKET_MS = 250;
export const DELAY_DEADBAND_MS = 60;
export const DELAY_MIN_INTERVAL_MS = 500;

/** Windowed minimum of `arrival - pts` in ms. Held (not cleared) when samples stop: an idle screen is not information. */
export class LatencyFloor {
  constructor() {
    this.buckets = []; // [bucketIndex, minMs]
    this.held = null;
  }
  /** @param {number} nowMs any monotonic local clock; @param {number} ptsUs the sender's wire PTS */
  observe(nowMs, ptsUs) {
    const d = nowMs - ptsUs / 1000;
    const b = Math.floor(nowMs / FLOOR_BUCKET_MS);
    const keepFrom = b - FLOOR_WINDOW_MS / FLOOR_BUCKET_MS;
    while (this.buckets.length && this.buckets[0][0] <= keepFrom) this.buckets.shift();
    const last = this.buckets[this.buckets.length - 1];
    if (last && last[0] === b) last[1] = Math.min(last[1], d);
    else this.buckets.push([b, d]);
    this.held = Math.min(...this.buckets.map((x) => x[1]));
  }
  floor() {
    return this.held;
  }
}

/**
 * Extra audio delay (ms) that lines audio up with the picture. Pure.
 * @param {number|null} videoFloor
 * @param {number|null} audioFloor
 * @param {boolean} videoAlive false when this sender has no video receiver (audio-only, or the watch ended)
 * @param {number} audioLeadMs the playout latency the audio path adds by itself (already part of its delay)
 */
export function targetDelayMs(videoFloor, audioFloor, videoAlive, audioLeadMs) {
  if (!videoAlive || videoFloor == null || audioFloor == null) return 0;
  const delta = videoFloor - audioFloor;
  // Outside this range the two PTS are not on one clock (older native sender): apply nothing rather than noise.
  if (delta < PLAUSIBLE_DELTA_MIN_MS || delta > PLAUSIBLE_DELTA_MAX_MS) return 0;
  return Math.max(0, Math.min(MAX_AUDIO_SYNC_DELAY_MS, delta - audioLeadMs));
}

/** Deadband + minimum interval between applied changes. */
export class DelayController {
  constructor() {
    this.appliedMs = 0;
    this.lastChangeMs = null;
  }
  update(targetMs, nowMs) {
    const target = Math.max(0, Math.min(MAX_AUDIO_SYNC_DELAY_MS, targetMs));
    const settled = this.lastChangeMs == null || nowMs - this.lastChangeMs >= DELAY_MIN_INTERVAL_MS;
    if (settled && Math.abs(target - this.appliedMs) >= DELAY_DEADBAND_MS) {
      this.appliedMs = target;
      this.lastChangeMs = nowMs;
    }
    return this.appliedMs;
  }
}

/** One sender's sync state, shared by its video and audio receivers (the room client keeps one per owner). */
export class AvSync {
  constructor() {
    this.video = new LatencyFloor();
    this.audio = new LatencyFloor();
    this.videoReceivers = 0;
  }
  videoStarted() {
    this.videoReceivers++;
  }
  videoGone() {
    this.videoReceivers = Math.max(0, this.videoReceivers - 1);
  }
  /** `presentAtMs`: when the video jitter buffer will release this frame (local clock). Stalled keepalives are skipped:
   *  they are a cached IDR re-sent with no encoder latency and would pull the floor down. */
  noteVideo(presentAtMs, ptsUs, stalled = false) {
    if (!stalled) this.video.observe(presentAtMs, ptsUs);
  }
  noteAudio(arrivalMs, ptsUs) {
    this.audio.observe(arrivalMs, ptsUs);
  }
  targetDelayMs(audioLeadMs) {
    return targetDelayMs(this.video.floor(), this.audio.floor(), this.videoReceivers > 0, audioLeadMs);
  }
}
