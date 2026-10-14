// Preview thumbnails (spec §7; client/src/preview.rs): ~30 s encrypted JPEGs, at most 320 px wide,
// JPEG quality 0.75, sent on a reliable uni stream and cached by the relay for late joiners.

export const PREVIEW_INTERVAL_MS = 30_000;
export const PREVIEW_FIRST_DELAY_MS = 1_500; // native waits a full interval; a browser sharer shows up with a thumbnail sooner
export const PREVIEW_MAX_WIDTH = 320;
export const PREVIEW_JPEG_QUALITY = 0.75;

export class PreviewSampler {
  constructor() {
    this.canvas = null;
  }

  /** Draw `frame` (a VideoFrame) onto the scratch canvas, downscaled. Synchronous: call while the frame is open. */
  capture(frame) {
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    if (!w || !h) return false;
    const tw = Math.min(w, PREVIEW_MAX_WIDTH);
    const th = Math.max(1, Math.round((h * tw) / w));
    if (!this.canvas) this.canvas = new OffscreenCanvas(tw, th);
    if (this.canvas.width !== tw || this.canvas.height !== th) {
      this.canvas.width = tw;
      this.canvas.height = th;
    }
    const ctx = this.canvas.getContext("2d");
    ctx.drawImage(frame, 0, 0, tw, th);
    return true;
  }

  /** JPEG of the last captured frame. */
  async toJpeg() {
    const blob = await this.canvas.convertToBlob({ type: "image/jpeg", quality: PREVIEW_JPEG_QUALITY });
    return new Uint8Array(await blob.arrayBuffer());
  }
}
