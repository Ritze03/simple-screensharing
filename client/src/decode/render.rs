//! GPU video display for egui (eframe, wgpu backend): NV12 [`DecodedFrame`]s
//! uploaded as two textures, converted to RGB in a WGSL shader, drawn with an
//! `egui_wgpu` paint callback, repainted whenever a new frame arrives.
//!
//! # Usage
//!
//! ```ignore
//! use simple_screensharing_client::decode::render::{Fit, VideoRenderer, VideoView};
//!
//! // Once, at app creation (needs eframe's wgpu renderer):
//! VideoRenderer::register(cc.wgpu_render_state.as_ref().expect("wgpu renderer"));
//!
//! // Per displayed stream (keep it in app state, drop it when the tile goes away):
//! let mut view = VideoView::new(decoder.frames());
//!
//! // Every frame, in the main window or inside a `show_viewport_deferred`
//! // closure (keep the view in an `Arc<Mutex<_>>` for that):
//! let response = view.show(ui, ui.available_size(), Fit::Contain);
//! // ... draw overlays on top with `ui.painter_at(response.rect)` ...
//! ```
//!
//! That's all: `show` also keeps a small waker thread alive that requests a
//! repaint of the viewport the view was last shown in whenever the decoder
//! publishes a frame, so a 240 fps stream drives ~240 repaints with no input.
//!
//! # Design
//!
//! * **One device, one pipeline.** eframe 0.33's wgpu painter owns a single
//!   `RenderState` (adapter/device/queue/renderer) shared by every viewport,
//!   including deferred ones (`egui_wgpu::winit::Painter::render_state` is one
//!   `Option<RenderState>`; viewports only get their own *surface*). So
//!   [`VideoRenderer`] lives once in `renderer.callback_resources` and holds
//!   the pipeline, the sampler, and per-stream GPU state for all viewports.
//! * **Per-stream, not per-view, textures.** GPU state is keyed by the
//!   decoder's output ([`FrameSource`] identity). A stream shown in a tile
//!   *and* a pop-out is uploaded once per decoded frame, whichever viewport
//!   paints first; the other one draws the same textures. `Why:` halves upload
//!   bandwidth for the tile+pop-out case and gives a natural presenter rule.
//! * **Presenter rule:** a frame counts as presented
//!   ([`FrameSource::note_presented`]) exactly once, when it is first uploaded
//!   for display, regardless of how many views/viewports draw it. `Why:` there
//!   is no "designated presenter" to keep in sync when views come and go
//!   (closing the pop-out can't stop the count), and presented fps then means
//!   "distinct frames that reached a screen", which is what the stats overlay
//!   wants. A frame that is decoded but superseded before any repaint uploads
//!   it is not counted (it was never shown).
//! * **Upload path:** `queue.write_texture` straight from the decoder's mapped
//!   GstBuffer slices with `bytes_per_row = stride` (no 256-byte alignment
//!   needed for `write_texture`), Y as `R8Unorm` (w×h) and UV as `Rg8Unorm`
//!   (⌈w/2⌉×⌈h/2⌉). Textures are recreated only on a size change.
//! * **Colour:** the YUV→RGB matrix (BT.601/709/2020) and limited/full range
//!   expansion are folded into three affine rows on the CPU
//!   ([`yuv_to_rgb_rows`]) and passed as a uniform. Output is gamma-encoded
//!   R'G'B' like egui's own colours; if the target format is `*Srgb` the shader
//!   linearizes so the hardware's encode round-trips.
//! * **Clipping:** egui clamps a callback's viewport to the screen, which
//!   would squash a partially off-screen video; `paint` resets the viewport to
//!   the full, unclamped video rect (wgpu allows that) and relies on egui's
//!   scissor for the clip.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use eframe::egui_wgpu::{self, wgpu};
use egui::{Color32, Rect, Response, Sense, Ui, Vec2, ViewportId};

use super::frame::{matrix_coefficients, Shared};
use super::{DecodedFrame, FrameSource, YuvMatrix};

/// How the video is placed inside the rect given to [`VideoView::show`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fit {
    /// Keep aspect ratio, fit entirely inside, black bars on the rest.
    #[default]
    Contain,
    /// Fill the rect, ignoring aspect ratio.
    Stretch,
}

/// Where the video goes inside `outer` for a `content` (frame size in pixels)
/// with the given [`Fit`], centred and snapped to the physical pixel grid
/// (`pixels_per_point`) so 1:1 display isn't blurred by a half-pixel offset.
pub fn fit_rect(outer: Rect, content: Vec2, fit: Fit, pixels_per_point: f32) -> Rect {
    if content.x <= 0.0 || content.y <= 0.0 || outer.width() <= 0.0 || outer.height() <= 0.0 {
        return Rect::from_min_size(outer.min, Vec2::ZERO);
    }
    let size = match fit {
        Fit::Stretch => outer.size(),
        Fit::Contain => {
            let scale = (outer.width() / content.x).min(outer.height() / content.y);
            content * scale
        }
    };
    let ppp = if pixels_per_point > 0.0 { pixels_per_point } else { 1.0 };
    let snap = |v: f32| (v * ppp).round() / ppp;
    let min = egui::pos2(snap(outer.center().x - size.x / 2.0), snap(outer.center().y - size.y / 2.0));
    let max = egui::pos2(snap(min.x + size.x).min(snap(outer.max.x)), snap(min.y + size.y).min(snap(outer.max.y)));
    Rect::from_min_max(min, max)
}

/// Size of the interleaved chroma plane / `Rg8` texture for a `width`×`height`
/// NV12 frame: `ceil(w/2) × ceil(h/2)`.
pub fn chroma_size(width: u32, height: u32) -> (u32, u32) {
    (width.div_ceil(2), height.div_ceil(2))
}

/// Scale from luma texture coordinates to chroma texture coordinates. For odd
/// sizes the chroma texture covers one luma column/row more than the image, so
/// sampling it at the same normalized coordinate would drift by up to half a
/// chroma texel at the far edge.
pub fn chroma_tc_scale(width: u32, height: u32) -> [f32; 2] {
    let (cw, ch) = chroma_size(width, height);
    [width as f32 / (2 * cw.max(1)) as f32, height as f32 / (2 * ch.max(1)) as f32]
}

/// YUV→RGB as three affine rows over normalized `(y, u, v, 1)` (each sample
/// `byte / 255`): `r = dot(rows[0], v)` etc. Folds the matrix's luma
/// coefficients and the limited-range expansion (Y 16..=235, C 16..=240) into
/// one set of constants. Matches the CPU path in `frame::nv12_to_rgba`.
pub fn yuv_to_rgb_rows(matrix: YuvMatrix, full_range: bool) -> [[f32; 4]; 3] {
    let (kr, kb) = matrix_coefficients(matrix);
    let kg = 1.0 - kr - kb;
    let (y_scale, y_off, c_scale) =
        if full_range { (1.0, 0.0, 1.0) } else { (255.0 / 219.0, 16.0 / 255.0, 255.0 / 224.0) };
    let c_off = 128.0 / 255.0;
    // (U coefficient, V coefficient) per output channel.
    let uv = [
        (0.0, 2.0 * (1.0 - kr)),
        (-2.0 * kb * (1.0 - kb) / kg, -2.0 * kr * (1.0 - kr) / kg),
        (2.0 * (1.0 - kb), 0.0),
    ];
    uv.map(|(cu, cv)| {
        let (cu, cv) = (cu * c_scale, cv * c_scale);
        [y_scale, cu, cv, -y_scale * y_off - (cu + cv) * c_off]
    })
}

const SHADER: &str = include_str!("render.wgsl");
/// `Params` in the shader: 4 × vec4<f32>.
const PARAMS_SIZE: u64 = 64;
/// Per-stream GPU state not used by any paint for this long is freed.
const STREAM_IDLE_EVICT: Duration = Duration::from_secs(5);

/// Shared GPU resources for video display: pipeline, sampler, bind group
/// layout, and per-stream textures. Register once with [`Self::register`].
pub struct VideoRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    srgb_target: bool,
    streams: Vec<StreamGpu>,
}

struct StreamGpu {
    source: Weak<Shared>,
    /// `seq` of the frame currently in the textures (0 = none).
    seq: u64,
    size: (u32, u32),
    params: Option<(YuvMatrix, bool)>,
    last_used: Instant,
    gpu: Option<StreamTextures>,
}

struct StreamTextures {
    y: wgpu::Texture,
    uv: wgpu::Texture,
    uniform: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl VideoRenderer {
    /// Build the pipeline and insert it into `render_state`'s
    /// `callback_resources` (replacing any previous one). Assumes eframe's
    /// defaults of no MSAA and no depth buffer; see [`Self::register_with`].
    pub fn register(render_state: &egui_wgpu::RenderState) {
        Self::register_with(render_state, 1, None);
    }

    /// Like [`Self::register`] for an app that set
    /// `NativeOptions::multisampling` / `depth_buffer`: the pipeline must match
    /// egui's render pass.
    pub fn register_with(
        render_state: &egui_wgpu::RenderState,
        msaa_samples: u32,
        depth_format: Option<wgpu::TextureFormat>,
    ) {
        let renderer = Self::new(&render_state.device, render_state.target_format, msaa_samples, depth_format);
        render_state.renderer.write().callback_resources.insert(renderer);
    }

    fn new(
        device: &wgpu::Device,
        target_format: wgpu::TextureFormat,
        msaa_samples: u32,
        depth_format: Option<wgpu::TextureFormat>,
    ) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("video nv12"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let tex_entry = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("video nv12"),
            entries: &[
                tex_entry(0),
                tex_entry(1),
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(PARAMS_SIZE),
                    },
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("video nv12"),
            bind_group_layouts: &[&layout],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("video nv12"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleStrip, ..Default::default() },
            depth_stencil: depth_format.map(|format| wgpu::DepthStencilState {
                format,
                depth_write_enabled: false,
                depth_compare: wgpu::CompareFunction::Always,
                stencil: Default::default(),
                bias: Default::default(),
            }),
            multisample: wgpu::MultisampleState { count: msaa_samples.max(1), ..Default::default() },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("video nv12"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Self { pipeline, layout, sampler, srgb_target: target_format.is_srgb(), streams: Vec::new() }
    }

    /// Upload `source`'s newest frame if it is newer than what's on the GPU.
    fn prepare_source(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, source: &FrameSource) {
        let now = Instant::now();
        self.streams.retain(|s| s.source.strong_count() > 0 && now.duration_since(s.last_used) < STREAM_IDLE_EVICT);
        let key = Arc::as_ptr(&source.0);
        let idx = match self.streams.iter().position(|s| s.source.as_ptr() == key) {
            Some(i) => i,
            None => {
                self.streams.push(StreamGpu {
                    source: Arc::downgrade(&source.0),
                    seq: 0,
                    size: (0, 0),
                    params: None,
                    last_used: now,
                    gpu: None,
                });
                self.streams.len() - 1
            }
        };
        let stream = &mut self.streams[idx];
        stream.last_used = now;
        let Some(frame) = source.latest_if_newer(stream.seq) else { return };
        if frame.width == 0 || frame.height == 0 {
            return;
        }
        if stream.gpu.is_none() || stream.size != (frame.width, frame.height) {
            stream.gpu = Some(create_textures(device, &self.layout, &self.sampler, frame.width, frame.height));
            stream.size = (frame.width, frame.height);
            stream.params = None;
        }
        let gpu = stream.gpu.as_ref().expect("just created");
        if stream.params != Some((frame.matrix, frame.full_range)) {
            queue.write_buffer(&gpu.uniform, 0, &params_bytes(&frame, self.srgb_target));
            stream.params = Some((frame.matrix, frame.full_range));
        }
        upload(queue, gpu, &frame);
        stream.seq = frame.seq;
        source.note_presented();
    }

    fn bind_group_for(&self, source: &FrameSource) -> Option<&wgpu::BindGroup> {
        let key = Arc::as_ptr(&source.0);
        self.streams
            .iter()
            .find(|s| s.source.as_ptr() == key)
            .filter(|s| s.seq > 0)
            .and_then(|s| s.gpu.as_ref())
            .map(|g| &g.bind_group)
    }
}

fn create_textures(
    device: &wgpu::Device,
    layout: &wgpu::BindGroupLayout,
    sampler: &wgpu::Sampler,
    width: u32,
    height: u32,
) -> StreamTextures {
    let tex = |label, w, h, format| {
        device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        })
    };
    let (cw, ch) = chroma_size(width, height);
    let y = tex("video Y", width, height, wgpu::TextureFormat::R8Unorm);
    let uv = tex("video UV", cw, ch, wgpu::TextureFormat::Rg8Unorm);
    let uniform = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("video params"),
        size: PARAMS_SIZE,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let y_view = y.create_view(&Default::default());
    let uv_view = uv.create_view(&Default::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("video nv12"),
        layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&y_view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&uv_view) },
            wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(sampler) },
            wgpu::BindGroupEntry { binding: 3, resource: uniform.as_entire_binding() },
        ],
    });
    StreamTextures { y, uv, uniform, bind_group }
}

fn upload(queue: &wgpu::Queue, gpu: &StreamTextures, frame: &DecodedFrame) {
    let plane = |texture: &wgpu::Texture, data: &[u8], stride: usize, w: u32, h: u32| {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            data,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride as u32), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    };
    let (cw, ch) = chroma_size(frame.width, frame.height);
    plane(&gpu.y, frame.y(), frame.y_stride, frame.width, frame.height);
    plane(&gpu.uv, frame.uv(), frame.uv_stride, cw, ch);
}

fn params_bytes(frame: &DecodedFrame, srgb_target: bool) -> [u8; PARAMS_SIZE as usize] {
    let rows = yuv_to_rgb_rows(frame.matrix, frame.full_range);
    let [sx, sy] = chroma_tc_scale(frame.width, frame.height);
    let extra = [sx, sy, if srgb_target { 1.0 } else { 0.0 }, 0.0];
    let mut out = [0u8; PARAMS_SIZE as usize];
    for (i, v) in rows.iter().flatten().chain(extra.iter()).enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    out
}

/// The paint callback one [`VideoView`] adds per frame.
struct VideoCallback {
    source: FrameSource,
}

impl egui_wgpu::CallbackTrait for VideoCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen_descriptor: &egui_wgpu::ScreenDescriptor,
        _egui_encoder: &mut wgpu::CommandEncoder,
        resources: &mut egui_wgpu::CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        match resources.get_mut::<VideoRenderer>() {
            Some(renderer) => renderer.prepare_source(device, queue, &self.source),
            None => warn_unregistered(),
        }
        Vec::new()
    }

    fn paint(
        &self,
        info: egui::PaintCallbackInfo,
        render_pass: &mut wgpu::RenderPass<'static>,
        resources: &egui_wgpu::CallbackResources,
    ) {
        let Some(renderer) = resources.get::<VideoRenderer>() else { return };
        let Some(bind_group) = renderer.bind_group_for(&self.source) else { return };
        // Unclamped viewport (egui's default is clamped to the screen, which
        // would squash a partially off-screen video); egui's scissor clips.
        let ppp = info.pixels_per_point;
        let r = info.viewport;
        let (x, y) = ((r.min.x * ppp).round(), (r.min.y * ppp).round());
        let (w, h) = ((r.max.x * ppp).round() - x, (r.max.y * ppp).round() - y);
        if w < 1.0 || h < 1.0 {
            return;
        }
        render_pass.set_viewport(x, y, w, h, 0.0, 1.0);
        render_pass.set_pipeline(&renderer.pipeline);
        render_pass.set_bind_group(0, bind_group, &[]);
        render_pass.draw(0..4, 0..1);
    }
}

fn warn_unregistered() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::error!("VideoView used without VideoRenderer::register(); video will stay black");
    }
}

/// A view decides it is hidden (and its waker thread exits) when it asked for
/// a repaint this long ago and still hasn't been shown since.
const HIDDEN_AFTER: Duration = Duration::from_millis(500);
/// Upper bound on how long a waker sleeps before re-checking its stop flag.
const WAKER_POLL: Duration = Duration::from_millis(200);
/// Delay passed with each frame-driven repaint request (see `run_waker`).
const REPAINT_NUDGE: Duration = Duration::from_micros(1);

/// State shared between a [`VideoView`] and its repaint-waker thread.
struct Waker {
    epoch: Instant,
    stop: AtomicBool,
    /// A waker thread is running (or about to).
    running: AtomicBool,
    viewport: Mutex<ViewportId>,
    /// Ms since `epoch` the view was last shown (painted visibly).
    shown_ms: AtomicU64,
    /// Ms since `epoch` of the oldest repaint request not yet followed by a
    /// show (0 = none outstanding).
    pending_ms: AtomicU64,
}

impl Waker {
    fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64 + 1
    }
}

/// A displayed video stream: draws the newest frame of a [`FrameSource`]
/// letterboxed into a rect and keeps the viewport repainting at the stream's
/// frame rate while it is visible. Pure display: overlays are the caller's.
///
/// Several views may show the same source (e.g. tile + pop-out); they share
/// GPU textures and the frame is counted as presented once (see module docs).
pub struct VideoView {
    source: FrameSource,
    /// Cached frame size for layout, and the `seq` it was read at.
    frame_size: Option<(u32, u32)>,
    size_seq: u64,
    waker: Arc<Waker>,
}

impl VideoView {
    pub fn new(source: FrameSource) -> Self {
        Self {
            source,
            frame_size: None,
            size_seq: 0,
            waker: Arc::new(Waker {
                epoch: Instant::now(),
                stop: AtomicBool::new(false),
                running: AtomicBool::new(false),
                viewport: Mutex::new(ViewportId::ROOT),
                shown_ms: AtomicU64::new(0),
                pending_ms: AtomicU64::new(0),
            }),
        }
    }

    pub fn source(&self) -> &FrameSource {
        &self.source
    }

    /// Pixel size of the newest frame, `None` before the first one.
    pub fn frame_size(&self) -> Option<(u32, u32)> {
        self.frame_size
    }

    /// Allocate `size` in `ui` and draw the video into it (black where there
    /// is no picture). The response's rect is the whole allocation; use
    /// [`Self::paint`] directly to get the letterboxed video rect.
    pub fn show(&mut self, ui: &mut Ui, size: Vec2, fit: Fit) -> Response {
        let (rect, response) = ui.allocate_exact_size(size, Sense::hover());
        self.paint(ui, rect, fit);
        response
    }

    /// Draw into `rect` without allocating layout space. Returns the rect the
    /// picture occupies (empty before the first frame).
    pub fn paint(&mut self, ui: &Ui, rect: Rect, fit: Fit) -> Rect {
        if let Some(frame) = self.source.latest_if_newer(self.size_seq) {
            self.size_seq = frame.seq;
            self.frame_size = Some((frame.width, frame.height));
        }
        let visible = ui.is_rect_visible(rect);
        self.keep_waker(ui.ctx(), visible);
        if !visible {
            return Rect::NOTHING;
        }
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::BLACK);
        let Some((w, h)) = self.frame_size else { return Rect::from_min_size(rect.min, Vec2::ZERO) };
        let video = fit_rect(rect, egui::vec2(w as f32, h as f32), fit, ui.ctx().pixels_per_point());
        if video.width() > 0.0 && video.height() > 0.0 {
            painter.add(egui_wgpu::Callback::new_paint_callback(video, VideoCallback { source: self.source.clone() }));
        }
        video
    }

    /// Record that the view was shown in `ctx`'s viewport and make sure a
    /// waker thread is running for it.
    fn keep_waker(&self, ctx: &egui::Context, visible: bool) {
        if !visible {
            return;
        }
        let w = &self.waker;
        *w.viewport.lock().unwrap() = ctx.viewport_id();
        w.shown_ms.store(w.now_ms(), Ordering::Release);
        w.pending_ms.store(0, Ordering::Release);
        if !w.running.swap(true, Ordering::AcqRel) {
            let waker = w.clone();
            let source = self.source.clone();
            let ctx = ctx.clone();
            let spawned = std::thread::Builder::new()
                .name("video-repaint".into())
                .spawn(move || run_waker(waker, source, ctx));
            if let Err(e) = spawned {
                tracing::warn!("failed to spawn video repaint thread: {e}");
                w.running.store(false, Ordering::Release);
            }
        }
    }
}

impl Drop for VideoView {
    fn drop(&mut self) {
        self.waker.stop.store(true, Ordering::Release);
        self.source.wake_waiters();
    }
}

/// Waker loop: sleep on the decoder's frame-arrival condvar, request a repaint
/// of the view's viewport per new frame. Never touches the decoder or the UI
/// state, so ingestion and presentation stay decoupled. Exits on drop, or when
/// the view stopped being shown (it re-spawns on the next visible show).
fn run_waker(waker: Arc<Waker>, source: FrameSource, ctx: egui::Context) {
    let mut seen = source.frame_counter();
    loop {
        if waker.stop.load(Ordering::Acquire) {
            break;
        }
        let latest = source.wait_for_newer(seen, WAKER_POLL);
        if waker.stop.load(Ordering::Acquire) {
            break;
        }
        let now = waker.now_ms();
        let pending = waker.pending_ms.load(Ordering::Acquire);
        if pending != 0 && now.saturating_sub(pending) > HIDDEN_AFTER.as_millis() as u64 {
            // Asked for a repaint long ago and the view never came back:
            // hidden/scrolled away/closed. Hand the job back to `show`.
            waker.running.store(false, Ordering::Release);
            if waker.shown_ms.load(Ordering::Acquire) >= pending && !waker.running.swap(true, Ordering::AcqRel) {
                continue; // raced with a show that saw us as running
            }
            break;
        }
        if latest > seen {
            seen = latest;
            let _ = waker.pending_ms.compare_exchange(0, now, Ordering::AcqRel, Ordering::Acquire);
            let viewport = *waker.viewport.lock().unwrap();
            // Not `request_repaint_of`: egui turns every zero-delay request
            // into *two* passes ("to let things settle"), which at 240 fps
            // doubled the paint rate (480/s measured) for nothing. A non-zero
            // delay skips that; egui subtracts the predicted frame time from
            // it, so this still means "repaint now".
            ctx.request_repaint_after_for(REPAINT_NUDGE, viewport);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::frame::nv12_to_rgba;
    use super::*;

    fn apply(rows: &[[f32; 4]; 3], y: u8, u: u8, v: u8) -> [u8; 3] {
        let x = [f32::from(y) / 255.0, f32::from(u) / 255.0, f32::from(v) / 255.0, 1.0];
        rows.map(|r| {
            let c = r[0] * x[0] + r[1] * x[1] + r[2] * x[2] + r[3];
            (c.clamp(0.0, 1.0) * 255.0).round() as u8
        })
    }

    #[test]
    fn matrix_rows_match_cpu_converter() {
        for matrix in [YuvMatrix::Bt601, YuvMatrix::Bt709, YuvMatrix::Bt2020] {
            for full_range in [false, true] {
                let rows = yuv_to_rgb_rows(matrix, full_range);
                for y in (0..=255).step_by(17) {
                    for u in (0..=255).step_by(15) {
                        for v in (0..=255).step_by(15) {
                            let cpu = nv12_to_rgba(&[y; 4], &[u, v], 2, 2, 2, 2, matrix, full_range);
                            let gpu = apply(&rows, y, u, v);
                            for c in 0..3 {
                                let d = (i32::from(cpu[c]) - i32::from(gpu[c])).abs();
                                assert!(d <= 1, "{matrix:?} full={full_range} yuv=({y},{u},{v}) cpu={:?} gpu={gpu:?}", &cpu[..3]);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn limited_range_black_white_and_grey() {
        for matrix in [YuvMatrix::Bt601, YuvMatrix::Bt709, YuvMatrix::Bt2020] {
            let rows = yuv_to_rgb_rows(matrix, false);
            assert_eq!(apply(&rows, 16, 128, 128), [0, 0, 0]);
            assert_eq!(apply(&rows, 235, 128, 128), [255, 255, 255]);
            let full = yuv_to_rgb_rows(matrix, true);
            assert_eq!(apply(&full, 0, 128, 128), [0, 0, 0]);
            assert_eq!(apply(&full, 255, 128, 128), [255, 255, 255]);
            assert_eq!(apply(&full, 128, 128, 128), [128, 128, 128]);
        }
    }

    #[test]
    fn bt709_limited_primaries() {
        // BT.709 limited-range 100% red / green / blue.
        let rows = yuv_to_rgb_rows(YuvMatrix::Bt709, false);
        let close = |a: [u8; 3], b: [u8; 3]| a.iter().zip(b).all(|(x, y)| (i32::from(*x) - i32::from(y)).abs() <= 2);
        assert!(close(apply(&rows, 63, 102, 240), [255, 0, 0]), "{:?}", apply(&rows, 63, 102, 240));
        assert!(close(apply(&rows, 173, 42, 26), [0, 255, 0]), "{:?}", apply(&rows, 173, 42, 26));
        assert!(close(apply(&rows, 32, 240, 118), [0, 0, 255]), "{:?}", apply(&rows, 32, 240, 118));
    }

    #[test]
    fn wait_for_newer_sleeps_until_timeout_without_frames() {
        let source = FrameSource(Shared::new());
        let started = Instant::now();
        assert_eq!(source.wait_for_newer(0, Duration::from_millis(30)), 0);
        assert!(started.elapsed() >= Duration::from_millis(30));
        // A wake-up without a frame doesn't report one either.
        source.wake_waiters();
        assert_eq!(source.wait_for_newer(0, Duration::from_millis(1)), 0);
    }

    #[test]
    fn chroma_size_rounds_up() {
        assert_eq!(chroma_size(1920, 1080), (960, 540));
        assert_eq!(chroma_size(1921, 1081), (961, 541));
        assert_eq!(chroma_size(1, 1), (1, 1));
        assert_eq!(chroma_tc_scale(1920, 1080), [1.0, 1.0]);
        let [sx, sy] = chroma_tc_scale(3, 5);
        assert!((sx - 3.0 / 4.0).abs() < 1e-6 && (sy - 5.0 / 6.0).abs() < 1e-6);
    }

    #[test]
    fn fit_contain_letterboxes_and_centres() {
        let outer = Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(1000.0, 1000.0));
        let r = fit_rect(outer, egui::vec2(1920.0, 1080.0), Fit::Contain, 1.0);
        assert_eq!(r.width(), 1000.0);
        assert!((r.height() - 562.5).abs() <= 1.0, "{r:?}");
        assert!((r.center().y - outer.center().y).abs() <= 1.0);
        assert!(outer.contains_rect(r));
        // Pillarbox: tall rect content in a wide outer.
        let outer = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1600.0, 900.0));
        let r = fit_rect(outer, egui::vec2(1080.0, 1920.0), Fit::Contain, 1.0);
        assert_eq!(r.height(), 900.0);
        assert!((r.width() - 506.25).abs() <= 1.0);
        assert!((r.center().x - 800.0).abs() <= 1.0);
        // Exact fit is the identity.
        let r = fit_rect(outer, egui::vec2(1600.0, 900.0), Fit::Contain, 1.0);
        assert_eq!(r, outer);
    }

    #[test]
    fn fit_snaps_to_physical_pixels() {
        let outer = Rect::from_min_size(egui::pos2(0.3, 0.3), egui::vec2(333.3, 200.0));
        let r = fit_rect(outer, egui::vec2(1920.0, 1080.0), Fit::Contain, 1.5);
        for v in [r.min.x, r.min.y, r.max.x, r.max.y] {
            assert!(((v * 1.5) - (v * 1.5).round()).abs() < 1e-3, "{r:?}");
        }
    }

    #[test]
    fn fit_stretch_and_degenerate() {
        let outer = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(640.0, 480.0));
        assert_eq!(fit_rect(outer, egui::vec2(1920.0, 1080.0), Fit::Stretch, 1.0), outer);
        assert_eq!(fit_rect(outer, egui::vec2(0.0, 1080.0), Fit::Contain, 1.0).area(), 0.0);
        let empty = Rect::from_min_size(egui::Pos2::ZERO, egui::Vec2::ZERO);
        assert_eq!(fit_rect(empty, egui::vec2(1920.0, 1080.0), Fit::Contain, 1.0).area(), 0.0);
    }
}
