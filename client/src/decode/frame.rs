//! GPU-ready decoder output: [`DecodedFrame`] (NV12 planes + timing), the
//! shared latest-frame slot ([`FrameSource`]) the decoder publishes into, and
//! the decode/present statistics.
//!
//! # Why this shape
//!
//! * **NV12, not RGBA.** VA decoders emit NV12 natively; software decoders
//!   emit I420 which a single `videoconvert` turns into NV12 cheaply. A GPU
//!   renderer uploads the Y plane (`R8`) and the interleaved UV plane
//!   (`Rg8`) as two textures and does the YUV→RGB matrix in its shader, so
//!   the CPU never touches per-pixel colour work. The legacy egui path
//!   ([`DecodedFrame::to_color_image`]) converts on the CPU only because the
//!   old UI wants an `egui::ColorImage`.
//! * **No copy after the decoder.** A [`DecodedFrame`] keeps the decoder's
//!   output `GstBuffer` mapped read-only and hands out slices into it
//!   ([`DecodedFrame::y`], [`DecodedFrame::uv`]). The renderer's
//!   `queue.write_texture` reads straight from decoder memory.
//! * **Cheap to share.** Frames travel as `Arc<DecodedFrame>`; publishing is
//!   one pointer swap under a short mutex, and a consumer that polls
//!   [`FrameSource::frame_counter`] (a single atomic load) only takes the lock
//!   when something new actually arrived.
//! * **Latest-only.** The slot holds the newest frame; an unread older one is
//!   dropped. Ingestion never waits for a consumer and never triggers a UI
//!   repaint — the render side pulls on its own cadence.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use gstreamer_video::{VideoColorMatrix, VideoColorRange, VideoFrame, VideoInfo};

use super::codec::VideoCodec;
use super::FpsCounter;

/// YUV→RGB matrix a frame was encoded with (what a shader needs to undo it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YuvMatrix {
    Bt601,
    Bt709,
    Bt2020,
}

/// One decoded video frame in NV12 layout.
///
/// Plane layout (what a renderer uploads):
/// * [`y`](Self::y): `height` rows of `y_stride` bytes, the first `width`
///   bytes of each row are luma.
/// * [`uv`](Self::uv): `ceil(height / 2)` rows of `uv_stride` bytes, the first
///   `2 * ceil(width / 2)` bytes of each row are interleaved `U V` pairs.
///
/// Strides are the decoder's real row pitches (they may exceed the visible
/// width); upload with `bytes_per_row = *_stride`.
pub struct DecodedFrame {
    /// Monotonically increasing, starts at 1: the n-th frame this decoder
    /// published. Equal to [`FrameSource::frame_counter`] at publish time.
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    pub y_stride: usize,
    pub uv_stride: usize,
    /// The sender's capture-time presentation timestamp, microseconds on the
    /// sender's own clock (`DecryptedMediaFrame::pts`). `0` if the decoder
    /// dropped it (never expected).
    pub pts_us: u64,
    /// When the decoder output reached us — the start of this frame's
    /// receive-side latency budget.
    pub decoded_at: Instant,
    pub matrix: YuvMatrix,
    /// `true` for full-range (0..=255) YUV, `false` for limited (16..=235).
    pub full_range: bool,
    /// The codec that produced this frame.
    pub codec: VideoCodec,
    frame: VideoFrame<gstreamer_video::video_frame::Readable>,
}

impl fmt::Debug for DecodedFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedFrame")
            .field("seq", &self.seq)
            .field("size", &(self.width, self.height))
            .field("y_stride", &self.y_stride)
            .field("uv_stride", &self.uv_stride)
            .field("pts_us", &self.pts_us)
            .field("codec", &self.codec)
            .finish()
    }
}

impl DecodedFrame {
    /// Wrap a mapped NV12 `frame`. `Err` if it isn't NV12 or its planes are
    /// shorter than its strides and dimensions imply.
    pub(super) fn new(
        frame: VideoFrame<gstreamer_video::video_frame::Readable>,
        info: &VideoInfo,
        pts_us: Option<u64>,
        seq: u64,
        codec: VideoCodec,
    ) -> Result<Self, &'static str> {
        if info.format() != gstreamer_video::VideoFormat::Nv12 {
            return Err("decoded frame is not NV12");
        }
        let (width, height) = (info.width(), info.height());
        let y_stride = usize::try_from(info.stride()[0]).map_err(|_| "negative Y stride")?;
        let uv_stride = usize::try_from(info.stride()[1]).map_err(|_| "negative UV stride")?;
        let y_len = frame.plane_data(0).map_err(|_| "no Y plane")?.len();
        let uv_len = frame.plane_data(1).map_err(|_| "no UV plane")?.len();
        let uv_rows = (height as usize).div_ceil(2);
        // Last row only needs its visible bytes, not a full stride.
        let y_min = y_stride * (height as usize - 1) + width as usize;
        let uv_min = uv_stride * (uv_rows - 1) + 2 * (width as usize).div_ceil(2);
        if y_len < y_min || uv_len < uv_min {
            return Err("decoded NV12 planes are shorter than their strides imply");
        }

        let colorimetry = info.colorimetry();
        let matrix = match colorimetry.matrix() {
            VideoColorMatrix::Bt709 => YuvMatrix::Bt709,
            VideoColorMatrix::Bt601 | VideoColorMatrix::Fcc | VideoColorMatrix::Smpte240m => YuvMatrix::Bt601,
            VideoColorMatrix::Bt2020 => YuvMatrix::Bt2020,
            // Unspecified / RGB: the same guess `videoconvert` makes — HD is
            // BT.709, SD is BT.601.
            _ if height >= 720 => YuvMatrix::Bt709,
            _ => YuvMatrix::Bt601,
        };
        let full_range = colorimetry.range() == VideoColorRange::Range0_255;

        Ok(Self {
            seq,
            width,
            height,
            y_stride,
            uv_stride,
            pts_us: pts_us.unwrap_or(0),
            decoded_at: Instant::now(),
            matrix,
            full_range,
            codec,
            frame,
        })
    }

    /// The luma plane, `y_stride * height` bytes (the last row may be short).
    pub fn y(&self) -> &[u8] {
        // Length was validated in `new`.
        self.frame.plane_data(0).expect("validated at construction")
    }

    /// The interleaved chroma plane.
    pub fn uv(&self) -> &[u8] {
        self.frame.plane_data(1).expect("validated at construction")
    }

    /// CPU conversion to a tightly packed RGBA8 buffer (`width * height * 4`
    /// bytes, alpha 255). For the legacy egui path and tests only; the real
    /// renderer should do this in a shader.
    pub fn to_rgba(&self) -> Vec<u8> {
        nv12_to_rgba(
            self.y(),
            self.uv(),
            self.width as usize,
            self.height as usize,
            self.y_stride,
            self.uv_stride,
            self.matrix,
            self.full_range,
        )
    }

    /// [`Self::to_rgba`] as an `egui::ColorImage`.
    pub fn to_color_image(&self) -> egui::ColorImage {
        egui::ColorImage::from_rgba_unmultiplied([self.width as usize, self.height as usize], &self.to_rgba())
    }
}

/// `(kr, kb)` luma coefficients for each matrix.
pub(super) fn matrix_coefficients(matrix: YuvMatrix) -> (f32, f32) {
    match matrix {
        YuvMatrix::Bt601 => (0.299, 0.114),
        YuvMatrix::Bt709 => (0.2126, 0.0722),
        YuvMatrix::Bt2020 => (0.2627, 0.0593),
    }
}

/// Plain integer NV12 → RGBA8 with 16.16 fixed-point coefficients. Chroma is
/// nearest-neighbour upsampled (each UV pair covers a 2×2 block).
#[allow(clippy::too_many_arguments)]
pub(super) fn nv12_to_rgba(
    y_plane: &[u8],
    uv_plane: &[u8],
    width: usize,
    height: usize,
    y_stride: usize,
    uv_stride: usize,
    matrix: YuvMatrix,
    full_range: bool,
) -> Vec<u8> {
    let (kr, kb) = matrix_coefficients(matrix);
    let kg = 1.0 - kr - kb;
    // Range expansion: limited-range luma 16..=235 and chroma 16..=240 map to
    // full range; full-range input is used as is.
    let (y_scale, y_off, c_scale) = if full_range { (1.0, 0.0, 1.0) } else { (255.0 / 219.0, 16.0, 255.0 / 224.0) };
    let fix = |v: f32| (v * 65536.0).round() as i32;
    let ys = fix(y_scale);
    let r_v = fix(c_scale * 2.0 * (1.0 - kr));
    let g_u = fix(-c_scale * 2.0 * kb * (1.0 - kb) / kg);
    let g_v = fix(-c_scale * 2.0 * kr * (1.0 - kr) / kg);
    let b_u = fix(c_scale * 2.0 * (1.0 - kb));
    let y_off = y_off as i32;

    let mut out = vec![0u8; width * height * 4];
    // Rows are independent, so split them across threads: this is the legacy
    // UI's per-repaint cost, and 1080p single-threaded is several ms.
    let threads = if width * height >= 256 * 256 {
        std::thread::available_parallelism().map_or(1, |n| n.get()).min(8)
    } else {
        1
    };
    let rows_per_chunk = height.div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        for (chunk_idx, chunk) in out.chunks_mut(rows_per_chunk * width * 4).enumerate() {
            let first_row = chunk_idx * rows_per_chunk;
            scope.spawn(move || {
                for (i, out_row) in chunk.chunks_exact_mut(width * 4).enumerate() {
                    let row = first_row + i;
                    let y_row = &y_plane[row * y_stride..row * y_stride + width];
                    let uv_row = &uv_plane[(row / 2) * uv_stride..];
                    for col in 0..width {
                        let uv_idx = (col / 2) * 2;
                        let u = i32::from(uv_row[uv_idx]) - 128;
                        let v = i32::from(uv_row[uv_idx + 1]) - 128;
                        let y = (i32::from(y_row[col]) - y_off) * ys;
                        let r = (y + r_v * v + 32768) >> 16;
                        let g = (y + g_u * u + g_v * v + 32768) >> 16;
                        let b = (y + b_u * u + 32768) >> 16;
                        let px = &mut out_row[col * 4..col * 4 + 4];
                        px[0] = r.clamp(0, 255) as u8;
                        px[1] = g.clamp(0, 255) as u8;
                        px[2] = b.clamp(0, 255) as u8;
                        px[3] = 255;
                    }
                }
            });
        }
    });
    out
}

/// What the decoder is currently using, for stats/UI.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DecoderInfo {
    /// Sniffed codec; `None` until the first decodable keyframe arrived.
    pub codec: Option<VideoCodec>,
    /// GStreamer element factory of the active decoder, e.g. `"vah264dec"`,
    /// `"avdec_h265"`. `None` until a pipeline is up.
    pub decoder_name: Option<String>,
    /// `true` if `decoder_name` is a VA hardware decoder.
    pub hardware: bool,
}

/// Snapshot of a decoder's counters. Rates are over a rolling 2 s window.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DecoderStats {
    pub info: DecoderInfo,
    /// Frames leaving the decoder per second (counted at the decoder output,
    /// independent of whether anything consumes them).
    pub decoded_fps: f64,
    /// Frames the consumer reported showing per second
    /// ([`FrameSource::note_presented`], or legacy `latest_frame()` yields).
    pub presented_fps: f64,
    pub frames_decoded: u64,
    pub frames_presented: u64,
    /// Compressed access units discarded before decoding because the decoder
    /// was behind (ingest channel full, or backlog trimmed). Each one costs
    /// picture integrity until the next keyframe unless the trim jumped to a
    /// keyframe.
    pub access_units_dropped: u64,
}

/// State shared between the decode pipeline (producer, GStreamer streaming
/// thread) and any number of readers.
pub(super) struct Shared {
    slot: Mutex<Option<Arc<DecodedFrame>>>,
    /// Signalled (with `slot` as its mutex) on every publish, so a repaint
    /// waker can sleep until a frame arrives instead of polling
    /// ([`FrameSource::wait_for_newer`]).
    arrived: Condvar,
    /// Number of frames published into `slot`; also the `seq` of the newest.
    published: AtomicU64,
    /// Frames seen leaving the decoder (pad probe) — can run ahead of
    /// `published` if a frame fails NV12 validation.
    decoded_total: AtomicU64,
    presented_total: AtomicU64,
    dropped_total: AtomicU64,
    decoded_fps: Mutex<FpsCounter>,
    presented_fps: Mutex<FpsCounter>,
    info: Mutex<DecoderInfo>,
}

impl Shared {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            slot: Mutex::new(None),
            arrived: Condvar::new(),
            published: AtomicU64::new(0),
            decoded_total: AtomicU64::new(0),
            presented_total: AtomicU64::new(0),
            dropped_total: AtomicU64::new(0),
            decoded_fps: Mutex::new(FpsCounter::new()),
            presented_fps: Mutex::new(FpsCounter::new()),
            info: Mutex::new(DecoderInfo::default()),
        })
    }

    /// Next `seq` to assign. Only the (single) active pipeline publishes, so
    /// read-then-publish is race-free.
    pub(super) fn next_seq(&self) -> u64 {
        self.published.load(Ordering::Acquire) + 1
    }

    /// Replace the latest frame. Overwrites an unread one (latest-only).
    pub(super) fn publish(&self, frame: DecodedFrame) {
        let seq = frame.seq;
        let mut slot = self.slot.lock().unwrap();
        *slot = Some(Arc::new(frame));
        // Stored under the lock so a `wait_for_newer` that checked the
        // counter under the same lock can't miss the wake-up.
        self.published.store(seq, Ordering::Release);
        drop(slot);
        self.arrived.notify_all();
    }

    /// Called for every buffer leaving the decoder.
    pub(super) fn record_decoded(&self) {
        self.decoded_total.fetch_add(1, Ordering::Relaxed);
        self.decoded_fps.lock().unwrap().record();
    }

    pub(super) fn add_dropped(&self, n: u64) {
        self.dropped_total.fetch_add(n, Ordering::Relaxed);
    }

    pub(super) fn decoded_total(&self) -> u64 {
        self.decoded_total.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(super) fn info_snapshot(&self) -> DecoderInfo {
        self.info.lock().unwrap().clone()
    }

    pub(super) fn set_info(&self, info: DecoderInfo) {
        *self.info.lock().unwrap() = info;
    }
}

/// Cloneable, thread-safe handle to a [`StreamDecoder`](super::StreamDecoder)'s
/// output. This is what a renderer holds: it does not need the decoder's own
/// lock, and it stays valid (returning the last frame) after the decoder is
/// dropped.
///
/// Typical render loop:
/// ```ignore
/// if let Some(frame) = source.latest_if_newer(last_seq) {
///     last_seq = frame.seq;
///     upload(frame.y(), frame.y_stride, frame.uv(), frame.uv_stride);
///     source.note_presented(); // after it actually hit the screen
/// }
/// ```
#[derive(Clone)]
pub struct FrameSource(pub(super) Arc<Shared>);

impl FrameSource {
    /// Number of frames published so far (== `seq` of the newest). One atomic
    /// load; poll this to decide whether to upload.
    pub fn frame_counter(&self) -> u64 {
        self.0.published.load(Ordering::Acquire)
    }

    /// The newest decoded frame, if any has been decoded yet. Clones an `Arc`.
    pub fn latest(&self) -> Option<Arc<DecodedFrame>> {
        self.0.slot.lock().unwrap().clone()
    }

    /// The newest frame if its `seq` is greater than `last_seen_seq`, else
    /// `None` without touching the lock.
    pub fn latest_if_newer(&self, last_seen_seq: u64) -> Option<Arc<DecodedFrame>> {
        if self.frame_counter() <= last_seen_seq {
            return None;
        }
        self.latest().filter(|f| f.seq > last_seen_seq)
    }

    /// Block until a frame newer than `last_seen_seq` is published or
    /// `timeout` passes; returns [`Self::frame_counter`] at wake-up (compare
    /// with `last_seen_seq` to tell the two apart). For repaint wakers
    /// ([`render::VideoView`](super::render::VideoView)): sleeps on a condvar,
    /// never spins, and never touches the decoder itself.
    pub fn wait_for_newer(&self, last_seen_seq: u64, timeout: Duration) -> u64 {
        if self.frame_counter() > last_seen_seq {
            return self.frame_counter();
        }
        let slot = self.0.slot.lock().unwrap();
        let _unused = self
            .0
            .arrived
            .wait_timeout_while(slot, timeout, |_| self.0.published.load(Ordering::Acquire) <= last_seen_seq)
            .unwrap();
        self.frame_counter()
    }

    /// Wake every [`Self::wait_for_newer`] caller now (used when a view is
    /// dropped so its waker thread exits promptly).
    pub fn wake_waiters(&self) {
        let _slot = self.0.slot.lock().unwrap();
        self.0.arrived.notify_all();
    }

    /// Identity of the underlying decoder output: equal for clones of the
    /// same `FrameSource`. Lets a renderer key per-stream GPU state.
    pub fn same_source(&self, other: &FrameSource) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Record access units dropped before reaching the decoder (used by
    /// `StreamDecoder::feed` when the ingest channel is full).
    pub(super) fn add_dropped(&self, n: u64) {
        self.0.add_dropped(n);
    }

    /// Total frames that have left the decoder so far (one atomic load).
    pub fn frames_decoded(&self) -> u64 {
        self.0.decoded_total()
    }

    /// Report that one frame was shown to the user (feeds
    /// [`DecoderStats::presented_fps`]). Call once per presented frame.
    pub fn note_presented(&self) {
        self.0.presented_total.fetch_add(1, Ordering::Relaxed);
        self.0.presented_fps.lock().unwrap().record();
    }

    pub fn stats(&self) -> DecoderStats {
        DecoderStats {
            info: self.0.info.lock().unwrap().clone(),
            decoded_fps: self.0.decoded_fps.lock().unwrap().fps(),
            presented_fps: self.0.presented_fps.lock().unwrap().fps(),
            frames_decoded: self.0.decoded_total.load(Ordering::Relaxed),
            frames_presented: self.0.presented_total.load(Ordering::Relaxed),
            access_units_dropped: self.0.dropped_total.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_nv12(width: usize, height: usize, y: u8, u: u8, v: u8) -> (Vec<u8>, Vec<u8>) {
        let y_plane = vec![y; width * height];
        let mut uv_plane = Vec::new();
        for _ in 0..height.div_ceil(2) * width.div_ceil(2) {
            uv_plane.push(u);
            uv_plane.push(v);
        }
        (y_plane, uv_plane)
    }

    fn px(rgba: &[u8], width: usize, x: usize, y: usize) -> [u8; 4] {
        let i = (y * width + x) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    fn near(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| (i32::from(*x) - i32::from(*y)).abs() <= tol)
    }

    /// Reference values for limited-range BT.709 (the encoder default for HD):
    /// black, white, and pure primaries.
    #[test]
    fn nv12_to_rgba_bt709_limited_primaries() {
        let cases: [([u8; 3], [u8; 4]); 5] = [
            ([16, 128, 128], [0, 0, 0, 255]),
            ([235, 128, 128], [255, 255, 255, 255]),
            ([63, 102, 240], [255, 0, 0, 255]),
            ([173, 42, 26], [0, 255, 0, 255]),
            ([32, 240, 118], [0, 0, 255, 255]),
        ];
        for (yuv, want) in cases {
            let (y, uv) = solid_nv12(4, 4, yuv[0], yuv[1], yuv[2]);
            let rgba = nv12_to_rgba(&y, &uv, 4, 4, 4, 4, YuvMatrix::Bt709, false);
            let got = px(&rgba, 4, 1, 1);
            assert!(near(got, want, 3), "BT.709 limited {yuv:?}: got {got:?}, want {want:?}");
        }
    }

    /// Large frames take the multi-threaded path; a luma ramp with neutral
    /// chroma must come out as the same gray ramp, row by row, with no seams
    /// at the thread-chunk boundaries.
    #[test]
    fn nv12_to_rgba_threaded_path_has_no_chunk_seams() {
        let (w, h) = (640usize, 481usize); // odd height, multiple chunks
        let mut y = vec![0u8; w * h];
        for row in 0..h {
            for col in 0..w {
                y[row * w + col] = (16 + (row * 219 / (h - 1))) as u8;
            }
        }
        let uv = vec![128u8; w * h.div_ceil(2)];
        let rgba = nv12_to_rgba(&y, &uv, w, h, w, w, YuvMatrix::Bt709, false);
        let mut prev = 0u8;
        for row in 0..h {
            let p = px(&rgba, w, 7, row);
            assert!(p[0] == p[1] && p[1] == p[2] && p[3] == 255, "row {row}: not gray: {p:?}");
            assert!(p[0] >= prev, "row {row}: ramp went backwards ({} < {prev})", p[0]);
            prev = p[0];
            assert_eq!(px(&rgba, w, 300, row), p, "row {row}: columns differ");
        }
        assert!(near(px(&rgba, w, 0, 0), [0, 0, 0, 255], 1));
        assert!(near(px(&rgba, w, 0, h - 1), [255, 255, 255, 255], 1));
    }

    #[test]
    fn nv12_to_rgba_bt601_full_range_gray() {
        let (y, uv) = solid_nv12(2, 2, 200, 128, 128);
        let rgba = nv12_to_rgba(&y, &uv, 2, 2, 2, 2, YuvMatrix::Bt601, true);
        assert!(near(px(&rgba, 2, 0, 0), [200, 200, 200, 255], 1));
    }

    /// Stride padding must be skipped, and odd sizes handled: a 3x3 frame in
    /// 8-byte rows with 0xEE padding.
    #[test]
    fn nv12_to_rgba_honours_strides_and_odd_sizes() {
        let (w, h, stride) = (3usize, 3usize, 8usize);
        let mut y = vec![0xEEu8; stride * h];
        for row in 0..h {
            for col in 0..w {
                y[row * stride + col] = 126; // limited-range mid gray
            }
        }
        let mut uv = vec![0xEEu8; stride * h.div_ceil(2)];
        for row in 0..h.div_ceil(2) {
            for pair in 0..w.div_ceil(2) {
                uv[row * stride + pair * 2] = 128;
                uv[row * stride + pair * 2 + 1] = 128;
            }
        }
        let rgba = nv12_to_rgba(&y, &uv, w, h, stride, stride, YuvMatrix::Bt709, false);
        assert_eq!(rgba.len(), w * h * 4);
        for (i, p) in rgba.chunks(4).enumerate() {
            assert!(near([p[0], p[1], p[2], p[3]], [128, 128, 128, 255], 2), "pixel {i} = {p:?}");
        }
    }
}
