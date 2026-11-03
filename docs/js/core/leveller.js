// Viewer-side loudness levelling (task #119): a slow AGC followed by an instant peak limiter.
//
// A line-for-line port of the native `client/src/leveller.rs` (same constants, same maths, same
// numbers in the tests), so a viewer hears the same thing in the browser and in the app. It works
// on the decoded PCM of each 20 ms Opus frame *before* it is scheduled (`AudioReceiver.#play`), not
// on WebAudio nodes:
//   - exact: the gain is applied per sample, the limiter's ceiling is a guarantee, not a compressor's
//     best effort (DynamicsCompressorNode has a ~6 ms lookahead and no hard ceiling);
//   - timer-free: a setInterval-driven AGC is throttled to 1 Hz in a background tab, the decoder
//     callback is not;
//   - no extra latency: the frame is already buffered (the playout delay), no lookahead is added.
// Why not a loudness normaliser with seconds of lookahead: it would add that much delay to a live
// stream with a picture. See superdoc/features/audio-sharing.md.

/** Short-term RMS the AGC aims at, dBFS (about -16 LUFS for typical stereo content). */
export const TARGET_DB = -18;
/** Most the AGC may add / take away, dB. */
export const MAX_GAIN_DB = 12;
export const MIN_GAIN_DB = -20;
/** Hard output ceiling, dBFS. Nothing leaves the leveller above this. */
export const CEILING_DB = -1;
/** The gain is not raised while the 400 ms level is below this, dBFS (near-silence is not boosted). */
export const GATE_DB = -50;
/** Fastest the gain rises ("slow up"), dB per second. */
export const RISE_DB_PER_S = 4;
/** Fastest the gain falls while the 400 ms level is above target, dB per second. */
export const FALL_DB_PER_S = 12;
/** How far above the target the fast detector may read before the gain is pulled down at once. */
export const HEADROOM_DB = 4;
/** Time constant of that quick pull-down, ms. */
export const ATTACK_MS = 5;
/** The limiter lets go of a reduction over this long, ms. */
export const LIMITER_RELEASE_MS = 100;

const FAST_ATTACK_MS = 5;
const FAST_RELEASE_MS = 80;
const SLOW_MS = 400;
const BLOCK_MS = 1;

const dbToLin = (db) => Math.pow(10, db / 20);
const powToDb = (p) => 10 * Math.log10(p + 1e-12);
const coeff = (ms, rate) => 1 - Math.exp(-1 / (ms * 1e-3 * rate));

export class Leveller {
  /** @param {number} rate sample rate, Hz */
  constructor(rate = 48000) {
    const r = Math.max(8000, rate);
    this.block = Math.max(1, Math.floor(BLOCK_MS * 1e-3 * r));
    this.cFastUp = coeff(FAST_ATTACK_MS, r);
    this.cFastDown = coeff(FAST_RELEASE_MS, r);
    this.cSlow = coeff(SLOW_MS, r);
    this.cAttack = 1 - Math.exp(-BLOCK_MS / ATTACK_MS);
    this.cLimit = coeff(LIMITER_RELEASE_MS, r);
    this.ceiling = dbToLin(CEILING_DB);
    this.reset();
  }

  reset() {
    this.inBlock = 0;
    this.pFast = 0;
    this.pSlow = 0;
    this.gainDb = 0;
    this.gLin = 1;
    this.gStep = 0;
    this.lim = 1;
  }

  /**
   * Level `planes` (one Float32Array per channel, all the same length) in place. The result does not
   * depend on how a stream is cut into calls.
   */
  process(planes) {
    const ch = planes.length;
    if (!ch) return;
    const n = planes[0].length;
    const c = this.ceiling;
    for (let i = 0; i < n; i++) {
      let power = 0;
      for (let k = 0; k < ch; k++) {
        let s = planes[k][i];
        if (!Number.isFinite(s)) s = planes[k][i] = 0;
        power += s * s;
      }
      power /= ch;
      this.pFast = Math.max(1e-18, this.pFast + (power > this.pFast ? this.cFastUp : this.cFastDown) * (power - this.pFast));
      this.pSlow = Math.max(1e-18, this.pSlow + this.cSlow * (power - this.pSlow));

      this.gLin += this.gStep;
      let peak = 0;
      for (let k = 0; k < ch; k++) {
        const y = planes[k][i] * this.gLin;
        planes[k][i] = y;
        peak = Math.max(peak, Math.abs(y));
      }
      // Zero-attack limiter: this sample's own need wins at once, the memory only releases.
      const need = peak > c ? c / peak : 1;
      this.lim = Math.min(this.lim + (1 - this.lim) * this.cLimit, need);
      for (let k = 0; k < ch; k++) planes[k][i] = Math.max(-c, Math.min(c, planes[k][i] * this.lim));

      if (++this.inBlock >= this.block) {
        this.inBlock = 0;
        this.#updateGain();
      }
    }
  }

  #updateGain() {
    const dt = BLOCK_MS * 1e-3;
    const slowDb = powToDb(this.pSlow);
    const fastDb = powToDb(this.pFast);
    const want = Math.max(MIN_GAIN_DB, Math.min(MAX_GAIN_DB, TARGET_DB - slowDb));
    let g = this.gainDb;
    if (want > g) {
      if (slowDb > GATE_DB) g += Math.min((want - g) * 1.5, RISE_DB_PER_S) * dt;
    } else {
      g -= Math.min((g - want) * 3, FALL_DB_PER_S) * dt;
    }
    const cap = Math.max(MIN_GAIN_DB, Math.min(MAX_GAIN_DB, TARGET_DB + HEADROOM_DB - fastDb));
    if (g > cap) g += (cap - g) * this.cAttack;
    this.gainDb = Math.max(MIN_GAIN_DB, Math.min(MAX_GAIN_DB, g));
    this.gStep = (dbToLin(this.gainDb) - this.gLin) / this.block;
  }
}
