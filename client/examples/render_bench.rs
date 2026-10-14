//! Display-side benchmark for `simple_screensharing_client::decode::render`.
//!
//! ```text
//! cargo run --release -p simple-screensharing-client --example render_bench -- \
//!     [--mode single|quad|popout] [--present auto|fifo|mailbox|immediate] \
//!     [--fps 240] [--seconds 10] [--warmup 2] [--frames 480] \
//!     [--pattern smpte|smpte75|zone-plate|ball] [--speed 8] \
//!     [--codec h264|h265|av1] [--bitrate 30000] [--shared] [--hold]
//! ```
//!
//! 1. Encodes `--frames` frames of a 1080p `videotestsrc` with the VA encoder
//!    (same setup as `decode_bench`; default SMPTE bars scrolling
//!    `--speed` px/frame, so it moves *and* has known colours; `--speed 0` for
//!    a static colour check).
//! 2. A feeder thread loops those access units into real `StreamDecoder`s at
//!    a real-time `--fps` pace (pts keep increasing across loops).
//! 3. An eframe window (wgpu, chosen `--present` mode) shows the stream(s)
//!    with `VideoView`: one tile, 2×2 tiles (`quad`: four decoders, or one
//!    shared decoder with `--shared`), or one tile + the same stream in a
//!    `show_viewport_deferred` pop-out (`popout`).
//!
//! Nothing requests repaints except `VideoView`'s frame-driven waker, so the
//! numbers measure exactly the production repaint path.
//!
//! After `--warmup` + `--seconds` it prints, per stream, decoded fps and
//! presented fps (distinct frames uploaded per second over the window, plus
//! the decoder's rolling `presented_fps`), and per viewport the repaint rate
//! and frame interval p50/p99/max, then exits (`--hold` keeps it open).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use eframe::egui_wgpu;
use gstreamer::prelude::*;
use gstreamer::{BufferFlags, Element, Pipeline, State};
use gstreamer_app::AppSink;
use simple_screensharing_client::decode::render::{Fit, VideoRenderer, VideoView};
use simple_screensharing_client::decode::{FrameSource, StreamDecoder, VideoCodec};
use simple_screensharing_client::net::DecryptedMediaFrame;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Single,
    Quad,
    Popout,
}

struct Args {
    mode: Mode,
    present: wgpu::PresentMode,
    fps: u32,
    seconds: f64,
    warmup: f64,
    frames: u32,
    width: u32,
    height: u32,
    pattern: String,
    speed: i32,
    codec: VideoCodec,
    bitrate_kbps: u32,
    shared: bool,
    hold: bool,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        mode: Mode::Single,
        present: wgpu::PresentMode::AutoVsync,
        fps: 240,
        seconds: 10.0,
        warmup: 2.0,
        frames: 480,
        width: 1920,
        height: 1080,
        pattern: "smpte".into(),
        speed: 8,
        codec: VideoCodec::H264,
        bitrate_kbps: 30_000,
        shared: false,
        hold: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--mode" => {
                a.mode = match value()?.as_str() {
                    "single" => Mode::Single,
                    "quad" => Mode::Quad,
                    "popout" => Mode::Popout,
                    o => bail!("unknown mode {o}"),
                }
            }
            "--present" => {
                // Same variants `rooms::PresentationMode::to_wgpu` produces
                // (AutoVsync/Immediate/Mailbox), plus plain Fifo.
                a.present = match value()?.as_str() {
                    "auto" => wgpu::PresentMode::AutoVsync,
                    "fifo" => wgpu::PresentMode::Fifo,
                    "mailbox" => wgpu::PresentMode::Mailbox,
                    "immediate" => wgpu::PresentMode::Immediate,
                    o => bail!("unknown present mode {o}"),
                }
            }
            "--fps" => a.fps = value()?.parse()?,
            "--seconds" => a.seconds = value()?.parse()?,
            "--warmup" => a.warmup = value()?.parse()?,
            "--frames" => a.frames = value()?.parse()?,
            "--width" => a.width = value()?.parse()?,
            "--height" => a.height = value()?.parse()?,
            "--pattern" => a.pattern = value()?,
            "--speed" => a.speed = value()?.parse()?,
            "--bitrate" => a.bitrate_kbps = value()?.parse()?,
            "--shared" => a.shared = true,
            "--hold" => a.hold = true,
            "--codec" => {
                a.codec = match value()?.as_str() {
                    "h264" => VideoCodec::H264,
                    "h265" => VideoCodec::H265,
                    "av1" => VideoCodec::Av1,
                    o => bail!("unknown codec {o}"),
                }
            }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

struct AccessUnit {
    pts_us: u64,
    keyframe: bool,
    data: Vec<u8>,
}

/// `(encoder element, output caps)` exactly as `capture.rs` configures them.
fn encoder_for(codec: VideoCodec) -> (&'static str, &'static str) {
    match codec {
        VideoCodec::H264 => ("vah264enc", "video/x-h264,stream-format=byte-stream,alignment=au"),
        VideoCodec::H265 => ("vah265enc", "video/x-h265,stream-format=byte-stream,alignment=au"),
        VideoCodec::Av1 => ("vaav1enc", "video/x-av1,stream-format=obu-stream,alignment=tu"),
    }
}

/// Same encode setup as `decode_bench`, plus a scrolling-pattern option.
fn encode(args: &Args) -> Result<Vec<AccessUnit>> {
    let (enc, caps) = encoder_for(args.codec);
    let extra = match args.pattern.as_str() {
        "zone-plate" => "kx2=10 ky2=10 kt=2".to_string(),
        _ => format!("horizontal-speed={}", args.speed),
    };
    let desc = format!(
        "videotestsrc num-buffers={n} pattern={pattern} {extra} is-live=false \
         ! video/x-raw,width={w},height={h},framerate={fps}/1 \
         ! videoconvert ! video/x-raw,format=NV12 \
         ! {enc} bitrate={br} key-int-max={n} \
         ! {caps} ! appsink name=out sync=false",
        n = args.frames,
        pattern = args.pattern,
        w = args.width,
        h = args.height,
        fps = args.fps,
        br = args.bitrate_kbps,
    );
    let pipeline = gstreamer::parse::launch(&desc)
        .with_context(|| format!("failed to build the {enc} encode pipeline"))?
        .downcast::<Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let sink = pipeline
        .by_name("out")
        .and_then(|e: Element| e.downcast::<AppSink>().ok())
        .ok_or_else(|| anyhow!("no appsink"))?;
    pipeline.set_state(State::Playing)?;
    let mut aus = Vec::new();
    loop {
        match sink.try_pull_sample(gstreamer::ClockTime::from_seconds(10)) {
            Some(sample) => {
                let buffer = sample.buffer().ok_or_else(|| anyhow!("sample without buffer"))?;
                let map = buffer.map_readable()?;
                aus.push(AccessUnit {
                    pts_us: buffer.pts().map(|p| p.useconds()).unwrap_or(0),
                    keyframe: !buffer.flags().contains(BufferFlags::DELTA_UNIT),
                    data: map.as_slice().to_vec(),
                });
            }
            None if sink.is_eos() => break,
            None => bail!("encoder stalled after {} access units", aus.len()),
        }
    }
    pipeline.set_state(State::Null)?;
    if aus.is_empty() || !aus[0].keyframe {
        bail!("{enc} produced no usable stream");
    }
    Ok(aus)
}

/// Loop `aus` into every decoder at `fps`, until `stop`.
fn feed(mut decoders: Vec<StreamDecoder>, aus: Vec<AccessUnit>, fps: u32, stop: Arc<AtomicBool>) {
    let period = Duration::from_secs_f64(1.0 / f64::from(fps));
    let loop_us = aus.len() as u64 * 1_000_000 / u64::from(fps);
    let started = Instant::now();
    let mut n: u64 = 0;
    'outer: for round in 0u64.. {
        for au in &aus {
            if stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            let due = started + period * n as u32;
            let now = Instant::now();
            if due > now {
                std::thread::sleep(due - now);
            }
            for dec in &mut decoders {
                dec.feed(&DecryptedMediaFrame {
                    conn_id: 1,
                    stream_id: dec.stream_id(),
                    pts: au.pts_us + round * loop_us,
                    keyframe: au.keyframe,
                    stalled: false,
                    payload: au.data.clone(),
                });
            }
            n += 1;
        }
    }
}

/// Frame-interval recorder for one viewport.
#[derive(Default)]
struct Timing {
    last: Option<Instant>,
    intervals_ms: Vec<f64>,
    frames: u64,
}

impl Timing {
    fn tick(&mut self, measuring: bool) {
        let now = Instant::now();
        if measuring {
            if let Some(last) = self.last {
                self.intervals_ms.push(now.duration_since(last).as_secs_f64() * 1000.0);
            }
            self.frames += 1;
        }
        self.last = Some(now);
    }

    fn report(&self, name: &str, secs: f64) {
        let mut v = self.intervals_ms.clone();
        v.sort_by(f64::total_cmp);
        let pct = |p: f64| if v.is_empty() { f64::NAN } else { v[((v.len() - 1) as f64 * p).round() as usize] };
        println!(
            "  viewport {name:<7} repaints/s {:>6.1}  frame interval ms p50 {:>6.2}  p99 {:>6.2}  max {:>6.2}",
            self.frames as f64 / secs,
            pct(0.5),
            pct(0.99),
            v.last().copied().unwrap_or(f64::NAN)
        );
    }
}

struct Bench {
    args: Args,
    started: Instant,
    sources: Vec<FrameSource>,
    views: Vec<VideoView>,
    popout: Option<Arc<Mutex<VideoView>>>,
    main_timing: Timing,
    popout_timing: Arc<Mutex<Timing>>,
    /// `(frames_presented, frames_decoded)` per source at measurement start.
    base: Option<Vec<(u64, u64)>>,
    done: bool,
}

impl Bench {
    fn measuring(&self) -> bool {
        let t = self.started.elapsed().as_secs_f64();
        t >= self.args.warmup && t < self.args.warmup + self.args.seconds
    }

    fn finish(&mut self) {
        let secs = self.args.seconds;
        let base = self.base.clone().unwrap_or_default();
        println!(
            "render_bench: mode {:?}{} present {:?} source {}x{} {} @ {} fps, {:.0}s measured",
            self.args.mode,
            if self.args.shared { " (shared decoder)" } else { "" },
            self.args.present,
            self.args.width,
            self.args.height,
            self.args.codec.name(),
            self.args.fps,
            secs
        );
        for (i, s) in self.sources.iter().enumerate() {
            let st = s.stats();
            let (p0, d0) = base.get(i).copied().unwrap_or((0, 0));
            println!(
                "  stream {i}: decoder {:<10} decoded/s {:>6.1}  presented/s {:>6.1} (rolling presented_fps {:>6.1})  AUs dropped {}",
                st.info.decoder_name.as_deref().unwrap_or("?"),
                (st.frames_decoded - d0) as f64 / secs,
                (st.frames_presented - p0) as f64 / secs,
                st.presented_fps,
                st.access_units_dropped
            );
        }
        self.main_timing.report("main", secs);
        if self.popout.is_some() {
            self.popout_timing.lock().unwrap().report("popout", secs);
        }
    }
}

impl eframe::App for Bench {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let measuring = self.measuring();
        if measuring && self.base.is_none() {
            self.base = Some(self.sources.iter().map(|s| (s.stats().frames_presented, s.stats().frames_decoded)).collect());
        }
        self.main_timing.tick(measuring);

        egui::CentralPanel::default().frame(egui::Frame::NONE.fill(egui::Color32::from_gray(30))).show(ctx, |ui| {
            let full = ui.max_rect();
            match self.args.mode {
                Mode::Single | Mode::Popout => {
                    self.views[0].paint(ui, full, Fit::Contain);
                }
                Mode::Quad => {
                    let half = full.size() / 2.0;
                    for (i, view) in self.views.iter_mut().enumerate() {
                        let min = full.min + egui::vec2((i % 2) as f32 * half.x, (i / 2) as f32 * half.y);
                        view.paint(ui, egui::Rect::from_min_size(min, half).shrink(2.0), Fit::Contain);
                    }
                }
            }
        });

        if let Some(view) = &self.popout {
            let view = view.clone();
            let timing = self.popout_timing.clone();
            let (warmup, seconds, started) = (self.args.warmup, self.args.seconds, self.started);
            ctx.show_viewport_deferred(
                egui::ViewportId::from_hash_of("render_bench_popout"),
                egui::ViewportBuilder::default().with_title("render_bench pop-out").with_inner_size([960.0, 540.0]),
                move |ctx, _class| {
                    let t = started.elapsed().as_secs_f64();
                    timing.lock().unwrap().tick(t >= warmup && t < warmup + seconds);
                    egui::CentralPanel::default().frame(egui::Frame::NONE).show(ctx, |ui| {
                        view.lock().unwrap().paint(ui, ui.max_rect(), Fit::Contain);
                    });
                },
            );
        }

        if !self.done && self.started.elapsed().as_secs_f64() >= self.args.warmup + self.args.seconds {
            self.done = true;
            self.finish();
            if !self.args.hold {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    gstreamer::init()?;
    let args = parse_args()?;
    eprintln!("encoding {} frames of {} with {} ...", args.frames, args.pattern, encoder_for(args.codec).0);
    let aus = encode(&args)?;
    eprintln!("  {} access units, {:.1} MB", aus.len(), aus.iter().map(|a| a.data.len()).sum::<usize>() as f64 / 1e6);

    let n_decoders = if args.mode == Mode::Quad && !args.shared { 4 } else { 1 };
    let decoders = (0..n_decoders).map(|i| StreamDecoder::new(i as u8 + 1)).collect::<Result<Vec<_>>>()?;
    let sources: Vec<FrameSource> = decoders.iter().map(|d| d.frames()).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (stop, fps) = (stop.clone(), args.fps);
        std::thread::Builder::new().name("feeder".into()).spawn(move || feed(decoders, aus, fps, stop))?
    };

    let views: Vec<VideoView> = match args.mode {
        Mode::Single | Mode::Popout => vec![VideoView::new(sources[0].clone())],
        Mode::Quad => (0..4).map(|i| VideoView::new(sources[i % sources.len()].clone())).collect(),
    };
    let popout = (args.mode == Mode::Popout).then(|| Arc::new(Mutex::new(VideoView::new(sources[0].clone()))));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1920.0, 1080.0]).with_title("render_bench"),
        renderer: eframe::Renderer::Wgpu,
        wgpu_options: egui_wgpu::WgpuConfiguration { present_mode: args.present, ..Default::default() },
        ..Default::default()
    };
    let result = eframe::run_native(
        "render_bench",
        options,
        Box::new(move |cc| {
            let rs = cc.wgpu_render_state.as_ref().ok_or("no wgpu render state")?;
            eprintln!(
                "adapter: {:?}, target format {:?}",
                rs.adapter.get_info().name,
                rs.target_format
            );
            VideoRenderer::register(rs);
            Ok(Box::new(Bench {
                args,
                started: Instant::now(),
                sources,
                views,
                popout,
                main_timing: Timing::default(),
                popout_timing: Arc::new(Mutex::new(Timing::default())),
                base: None,
                done: false,
            }))
        }),
    );
    stop.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    result.map_err(|e| anyhow!("eframe: {e}"))
}
