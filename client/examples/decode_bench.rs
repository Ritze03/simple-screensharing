//! Decode throughput benchmark for `simple_screensharing_client::decode`.
//!
//! ```text
//! cargo run --release -p simple-screensharing-client --example decode_bench -- \
//!     [--frames 600] [--width 1920] [--height 1080] [--fps 240] \
//!     [--bitrate 30000] [--pattern zone-plate|ball|snow|smpte] \
//!     [--codecs h264,h265,av1] [--passes 3] [--no-sw] [--no-hw]
//! ```
//!
//! For each codec it
//!
//! 1. encodes `--frames` frames of 1080p `videotestsrc` (default
//!    `pattern=zone-plate`: animated full-frame detail, so every inter frame
//!    carries real residual data; `ball` is a tiny moving object on black and
//!    encodes to almost nothing, `snow` is the worst case) with the **VA hardware
//!    encoder** (`vah264enc` / `vah265enc` / `vaav1enc`) using exactly the
//!    stream formats `capture.rs` emits, and collects the access units;
//! 2. feeds them through the real [`StreamDecoder`] (codec sniffing, pipeline,
//!    NV12 output — the production code path, not a bare GStreamer pipeline)
//!    once with hardware decode allowed and once software-only;
//! 3. reports decoded frames per second, measured at the decoder output.
//!
//! The feed is **closed-loop**: access units are pushed as fast as possible,
//! but no more than `WINDOW` ahead of what has been decoded. `Why:` the decoder
//! deliberately drops the *oldest* queued access units when it falls behind
//! (low latency over completeness), so an open-loop "as fast as possible" feed
//! would measure the drop rate, not the decode rate. The bench also prints how
//! many frames the decoder still held back at the end of each pass
//! (`held`): a non-zero value is decoder-internal latency in frames.

use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use gstreamer::prelude::*;
use gstreamer::{BufferFlags, Element, Pipeline, State};
use gstreamer_app::AppSink;
use simple_screensharing_client::decode::{StreamDecoder, VideoCodec};
use simple_screensharing_client::net::DecryptedMediaFrame;

/// Max access units in flight between `feed()` and the decoder output.
const WINDOW: u64 = 4;
const STREAM_ID: u8 = 1;

struct Args {
    frames: u32,
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    pattern: String,
    codecs: Vec<VideoCodec>,
    passes: usize,
    hw: bool,
    sw: bool,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        frames: 600,
        width: 1920,
        height: 1080,
        fps: 240,
        bitrate_kbps: 30_000,
        pattern: "zone-plate".into(),
        codecs: vec![VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1],
        passes: 3,
        hw: true,
        sw: true,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--frames" => a.frames = value()?.parse()?,
            "--width" => a.width = value()?.parse()?,
            "--height" => a.height = value()?.parse()?,
            "--fps" => a.fps = value()?.parse()?,
            "--bitrate" => a.bitrate_kbps = value()?.parse()?,
            "--pattern" => a.pattern = value()?,
            "--passes" => a.passes = value()?.parse()?,
            "--no-hw" => a.hw = false,
            "--no-sw" => a.sw = false,
            "--codecs" => {
                a.codecs = value()?
                    .split(',')
                    .map(|c| match c {
                        "h264" => Ok(VideoCodec::H264),
                        "h265" => Ok(VideoCodec::H265),
                        "av1" => Ok(VideoCodec::Av1),
                        other => bail!("unknown codec {other}"),
                    })
                    .collect::<Result<_>>()?;
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

fn encode(codec: VideoCodec, args: &Args) -> Result<Vec<AccessUnit>> {
    let (enc, caps) = encoder_for(codec);
    let desc = format!(
        "videotestsrc num-buffers={n} pattern={pattern} {extra} is-live=false \
         ! video/x-raw,width={w},height={h},framerate={fps}/1 \
         ! videoconvert ! video/x-raw,format=NV12 \
         ! {enc} bitrate={br} key-int-max=240 \
         ! {caps} ! appsink name=out sync=false",
        n = args.frames,
        pattern = args.pattern,
        extra = if args.pattern == "zone-plate" { "kx2=10 ky2=10 kt=2" } else { "" },
        w = args.width,
        h = args.height,
        fps = args.fps,
        br = args.bitrate_kbps,
    );
    let pipeline = gstreamer::parse::launch(&desc)
        .with_context(|| format!("failed to build the {enc} encode pipeline (is the VA encoder available?)"))?
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
                    pts_us: buffer.pts().map(|p| p.useconds()).unwrap_or(aus.len() as u64 * 1_000_000 / u64::from(args.fps)),
                    keyframe: !buffer.flags().contains(BufferFlags::DELTA_UNIT),
                    data: map.as_slice().to_vec(),
                });
            }
            None if sink.is_eos() => break,
            None => {
                let err = pipeline.bus().and_then(|b| b.pop_filtered(&[gstreamer::MessageType::Error]));
                bail!("encoder stalled after {} access units{}", aus.len(), err.map(|m| format!(": {m:?}")).unwrap_or_default())
            }
        }
    }
    let err = pipeline.bus().and_then(|b| b.pop_filtered(&[gstreamer::MessageType::Error]));
    pipeline.set_state(State::Null)?;
    if aus.is_empty() {
        bail!("{enc} produced no access units{}", err.map(|m| format!(": {m:?}")).unwrap_or_default());
    }
    Ok(aus)
}

struct PassResult {
    fps: f64,
    held: u64,
}

/// Feed `aus` once, closed-loop; returns decoded fps over the pass.
fn run_pass(dec: &mut StreamDecoder, aus: &[AccessUnit]) -> Result<PassResult> {
    let source = dec.frames();
    let base = source.frames_decoded();
    let started = Instant::now();
    let mut fed = 0u64;
    for au in aus {
        // Closed loop: stay at most WINDOW access units ahead of the output.
        let deadline = Instant::now() + Duration::from_secs(5);
        while fed.saturating_sub(source.frames_decoded() - base) >= WINDOW {
            if Instant::now() > deadline {
                bail!("decoder stopped producing frames (fed {fed}, decoded {})", source.frames_decoded() - base);
            }
            std::hint::spin_loop();
        }
        dec.feed(&DecryptedMediaFrame {
            conn_id: 1,
            stream_id: STREAM_ID,
            pts: au.pts_us,
            keyframe: au.keyframe,
            stalled: false,
            payload: au.data.clone(),
        });
        fed += 1;
    }
    // Drain: wait until everything is out or progress stops.
    let mut last_progress = Instant::now();
    let mut last = source.frames_decoded();
    let mut end = Instant::now();
    while source.frames_decoded() - base < fed && last_progress.elapsed() < Duration::from_millis(400) {
        std::thread::sleep(Duration::from_micros(200));
        let now = source.frames_decoded();
        if now != last {
            last = now;
            last_progress = Instant::now();
            end = last_progress;
        }
    }
    let decoded = source.frames_decoded() - base;
    Ok(PassResult { fps: decoded as f64 / end.duration_since(started).as_secs_f64(), held: fed - decoded })
}

fn bench(codec: VideoCodec, aus: &[AccessUnit], hw: bool, passes: usize) -> Result<()> {
    let mut dec = if hw { StreamDecoder::new(STREAM_ID)? } else { StreamDecoder::new_software_only(STREAM_ID)? };
    // Warm-up pass: pipeline creation, VA context setup, first-touch pages.
    run_pass(&mut dec, aus).context("warm-up pass")?;
    let stats = dec.stats();
    let mut results = Vec::new();
    for _ in 0..passes {
        results.push(run_pass(&mut dec, aus)?);
    }
    let mean = results.iter().map(|r| r.fps).sum::<f64>() / results.len() as f64;
    let min = results.iter().map(|r| r.fps).fold(f64::INFINITY, f64::min);
    let held = results.iter().map(|r| r.held).max().unwrap_or(0);
    let per_pass = results.iter().map(|r| format!("{:.0}", r.fps)).collect::<Vec<_>>().join(" / ");
    println!(
        "| {:<5} | {:<2} | {:<11} | {:>7.0} | {:>7.0} | {:>4} | {} |",
        codec.name(),
        if hw { "HW" } else { "SW" },
        stats.info.decoder_name.as_deref().unwrap_or("(none)"),
        mean,
        min,
        held,
        per_pass
    );
    // A "HW" row that silently ran software would make the table lie.
    if hw && !stats.info.hardware {
        eprintln!("WARNING: {} HW run actually used {:?}", codec.name(), stats.info.decoder_name);
    }
    Ok(())
}

fn main() -> Result<()> {
    gstreamer::init()?;
    let args = parse_args()?;
    println!(
        "decode_bench: {}x{} @ {} fps source, pattern {}, {} frames, {} kbps target, {} measured passes, window {}",
        args.width, args.height, args.fps, args.pattern, args.frames, args.bitrate_kbps, args.passes, WINDOW
    );
    println!("| codec | be | decoder     | mean fps | min fps | held | per-pass fps |");
    println!("|-------|----|-------------|---------:|--------:|-----:|--------------|");
    for &codec in &args.codecs {
        let (enc, _) = encoder_for(codec);
        eprintln!("encoding {} with {enc} ...", codec.name());
        let aus = match encode(codec, &args) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("skipping {}: {e:#}", codec.name());
                continue;
            }
        };
        let total: usize = aus.iter().map(|a| a.data.len()).sum();
        eprintln!(
            "  {} access units, {:.1} MB, {:.0} kbps at {} fps",
            aus.len(),
            total as f64 / 1e6,
            total as f64 * 8.0 / 1000.0 / (aus.len() as f64 / f64::from(args.fps)),
            args.fps
        );
        if args.hw {
            bench(codec, &aus, true, args.passes)?;
        }
        if args.sw {
            bench(codec, &aus, false, args.passes)?;
        }
    }
    Ok(())
}
