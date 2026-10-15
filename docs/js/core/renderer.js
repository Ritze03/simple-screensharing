// Puts decoded VideoFrames on a canvas: the newest frame once per display refresh, every superseded one closed unseen.
//
// Backend: WebGL2 (`texImage2D(videoFrame)` + one full-screen triangle), falling back to a 2D `drawImage`.
// Why WebGL first (task #67, 2026-10-15; user report "anything above 60 fps hangs and skips" in Floorp/Firefox 157):
// Firefox draws a 2D canvas in software, so `drawImage(VideoFrame)` reads the (hardware) decoded surface back to the CPU and
// converts it on the MAIN thread: measured 13 ms per 1440p frame in headed Firefox (rig, VAAPI), 7 ms headless. Two things
// then break above ~60 fps: the draws alone fill the main thread (13 ms x 60 = 78 %), and the VideoDecoder's output and
// decode() callbacks run on that same thread, so decoding collapsed to the draw rate and the decode-queue backstop dropped
// whole GOPs ("hangs, skips"). `texImage2D(videoFrame)` takes the frame as a GPU texture (0.2 ms at 1440p in the same
// browser, same machine) and leaves the main thread free.
//
// The backend is chosen once per page, on the first frame, by timing both on scratch canvases with that exact frame (see
// `webglIsFaster`; WebGL only when clearly cheaper): a canvas that ever got a "webgl2" context cannot get a "2d" one, so the
// choice has to be made before the real canvas is touched.
// Independently of the backend, the renderer measures its own draw time and spaces draws so that drawing never takes more than
// about half of the main thread (`minIntervalMs`); if even that is not enough the picture runs at a lower rate instead of the
// whole page (decode callbacks, input, jitter-buffer timer) starving.

const VERT = `#version 300 es
out vec2 uv;
void main() {
  vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
  uv = vec2(p.x, 1.0 - p.y);
  gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}`;
const FRAG = `#version 300 es
precision mediump float;
in vec2 uv;
uniform sampler2D tex;
out vec4 color;
void main() { color = vec4(texture(tex, uv).rgb, 1.0); }`;

const GL_OPTS = { alpha: false, antialias: false, depth: false, stencil: false, desynchronized: true, powerPreference: "high-performance" };
const MAX_MAIN_THREAD_SHARE = 0.5; // draws may use at most this share of the thread
const MIN_SPACING_MS = 3; // below this the display refresh is the limit anyway

let glVerdict = null; // null = not tried yet; remembered for the page's lifetime

function scratchCanvas() {
  return typeof document !== "undefined" ? document.createElement("canvas") : new OffscreenCanvas(2, 2);
}

/** Compile the full-screen-triangle program and create the frame texture on `gl` (left bound and current). */
function initGl(gl) {
  const compile = (type, src) => {
    const s = gl.createShader(type);
    gl.shaderSource(s, src);
    gl.compileShader(s);
    if (!gl.getShaderParameter(s, gl.COMPILE_STATUS)) throw new Error(gl.getShaderInfoLog(s));
    return s;
  };
  const p = gl.createProgram();
  gl.attachShader(p, compile(gl.VERTEX_SHADER, VERT));
  gl.attachShader(p, compile(gl.FRAGMENT_SHADER, FRAG));
  gl.linkProgram(p);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) throw new Error(gl.getProgramInfoLog(p));
  gl.useProgram(p);
  const tex = gl.createTexture();
  gl.bindTexture(gl.TEXTURE_2D, tex);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MIN_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_MAG_FILTER, gl.LINEAR);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_S, gl.CLAMP_TO_EDGE);
  gl.texParameteri(gl.TEXTURE_2D, gl.TEXTURE_WRAP_T, gl.CLAMP_TO_EDGE);
  return true;
}

const median = (a) => a.slice().sort((x, y) => x - y)[a.length >> 1];

/**
 * Is WebGL clearly cheaper than 2D `drawImage` for frames like this one? Times both, synchronously, on scratch canvases of the
 * frame's size (GL: upload + draw + finish; 2D: drawImage + a 1-pixel read-back to flush a GPU-backed canvas). Measured instead of
 * assuming because it goes both ways: Firefox 2D is a CPU path (13 ms vs 0.2 ms at 1440p), but where WebGL is itself software
 * (Chromium headless / SwiftShader, VMs without a GPU) it is slower than Chromium's 2D (18 ms vs 5 ms). Ties go to 2D.
 */
function webglIsFaster(frame) {
  try {
    const w = frame.displayWidth;
    const h = frame.displayHeight;
    const glCanvas = scratchCanvas();
    glCanvas.width = w;
    glCanvas.height = h;
    const gl = glCanvas.getContext("webgl2", GL_OPTS);
    if (!gl || !initGl(gl)) return false;
    const c2 = scratchCanvas();
    c2.width = w;
    c2.height = h;
    const ctx = c2.getContext("2d", { alpha: false, willReadFrequently: false });
    const glOnce = () => {
      gl.viewport(0, 0, w, h);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, frame);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      gl.finish();
    };
    const twoOnce = () => {
      ctx.drawImage(frame, 0, 0, w, h);
      ctx.getImageData(0, 0, 1, 1);
    };
    const time = (fn) => {
      const t = performance.now();
      fn();
      return performance.now() - t;
    };
    glOnce();
    if (gl.getError() !== gl.NO_ERROR) return false;
    twoOnce(); // warm-ups: shader/driver setup, first-use allocation
    const g = median([time(glOnce), time(glOnce), time(glOnce)]);
    const d = median([time(twoOnce), time(twoOnce), time(twoOnce)]);
    const lose = gl.getExtension("WEBGL_lose_context");
    if (lose) lose.loseContext();
    CanvasRenderer.lastCalibration = { glMs: +g.toFixed(2), twoDMs: +d.toFixed(2), size: `${w}x${h}` };
    return g < d * 0.8;
  } catch {
    return false;
  }
}

export class CanvasRenderer {
  /**
   * @param {HTMLCanvasElement|OffscreenCanvas} canvas
   * @param {{onDraw?:()=>void, webgl?:boolean, preserveDrawingBuffer?:boolean}} [o]
   *   webgl: true (default) = WebGL if a first-frame timing says it is clearly cheaper than 2D, false = always 2D,
   *   "force" = WebGL whenever it works (tests). preserveDrawingBuffer keeps the picture readable with toDataURL (dev snapshots).
   */
  constructor(canvas, { onDraw = null, webgl = true, preserveDrawingBuffer = false } = {}) {
    this.canvas = canvas;
    this.wantGl = webgl;
    this.preserve = preserveDrawingBuffer;
    this.backend = null; // "webgl" | "2d", decided on the first frame
    this.ctx = null;
    this.gl = null;
    this.glLost = false;
    this.latest = null;
    this.raf = 0;
    this.drawn = 0;
    this.onDraw = onDraw;
    this.stopped = false;
    this.lastDrawAt = -Infinity;
    this.drawMsEwma = 0;
    this.minIntervalMs = 0;
    this.tick = this.tick.bind(this);
    this.onLost = (e) => {
      e.preventDefault();
      this.glLost = true;
    };
    this.onRestored = () => {
      this.glLost = !this.#setupGl();
    };
  }

  /** Hand a frame over (takes ownership). */
  present(frame) {
    if (this.stopped) {
      frame.close();
      return;
    }
    if (this.latest) this.latest.close();
    this.latest = frame;
    if (!this.raf) this.raf = requestAnimationFrame(this.tick);
  }

  #chooseBackend(frame) {
    if (this.wantGl) {
      if (glVerdict === null && this.wantGl !== "force") glVerdict = webglIsFaster(frame);
      if ((this.wantGl === "force" || glVerdict) && this.#setupGl()) {
        this.backend = "webgl";
        this.canvas.addEventListener("webglcontextlost", this.onLost);
        this.canvas.addEventListener("webglcontextrestored", this.onRestored);
        return;
      }
    }
    this.backend = "2d";
    this.ctx = this.canvas.getContext("2d", { alpha: false, desynchronized: true });
  }

  #setupGl() {
    try {
      const gl = this.gl && !this.gl.isContextLost() ? this.gl : this.canvas.getContext("webgl2", { ...GL_OPTS, preserveDrawingBuffer: this.preserve });
      if (!gl || !initGl(gl)) return false;
      this.gl = gl;
      return true;
    } catch {
      return false;
    }
  }

  #draw(f, w, h) {
    if (this.backend === "webgl") {
      if (this.glLost) return false;
      const gl = this.gl;
      gl.viewport(0, 0, w, h);
      gl.texImage2D(gl.TEXTURE_2D, 0, gl.RGBA, gl.RGBA, gl.UNSIGNED_BYTE, f);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      return true;
    }
    this.ctx.drawImage(f, 0, 0, w, h);
    return true;
  }

  tick() {
    this.raf = 0;
    const f = this.latest;
    if (!f) return;
    const t0 = performance.now();
    if (t0 - this.lastDrawAt < this.minIntervalMs) {
      this.raf = requestAnimationFrame(this.tick); // too soon after the last (expensive) draw: keep the newest frame and wait
      return;
    }
    this.latest = null;
    if (!this.backend) this.#chooseBackend(f);
    const w = f.displayWidth;
    const h = f.displayHeight;
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.canvas.width = w;
      this.canvas.height = h;
    }
    let ok = false;
    try {
      ok = this.#draw(f, w, h);
    } catch {}
    f.close();
    const t1 = performance.now();
    this.lastDrawAt = t1;
    this.drawMsEwma = this.drawMsEwma ? this.drawMsEwma * 0.8 + (t1 - t0) * 0.2 : t1 - t0;
    const spacing = (this.drawMsEwma * (1 - MAX_MAIN_THREAD_SHARE)) / MAX_MAIN_THREAD_SHARE;
    this.minIntervalMs = spacing > MIN_SPACING_MS ? spacing : 0;
    if (!ok) return;
    this.drawn++;
    if (this.onDraw) this.onDraw();
  }

  stop() {
    this.stopped = true;
    if (this.raf) cancelAnimationFrame(this.raf);
    this.raf = 0;
    if (this.latest) this.latest.close();
    this.latest = null;
    if (this.backend === "webgl") {
      this.canvas.removeEventListener("webglcontextlost", this.onLost);
      this.canvas.removeEventListener("webglcontextrestored", this.onRestored);
    }
  }
}
