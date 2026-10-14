//! Test helpers shared by `decode`'s unit tests: encode short SMPTE-bar clips
//! with software encoders (so tests don't depend on a GPU) in exactly the
//! stream formats the sender produces.

use anyhow::{anyhow, Result};
use gstreamer::prelude::*;
use gstreamer::{BufferFlags, ClockTime, Element, Pipeline, State};
use gstreamer_app::AppSink;

use super::codec::VideoCodec;

/// One encoded access unit.
#[derive(Clone)]
pub(crate) struct Au {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_us: u64,
}

/// Route `tracing` output to the test harness' captured stderr (visible with
/// `--nocapture`, or on failure).
pub(crate) fn init_logging() {
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::DEBUG).with_test_writer().try_init();
}

/// `true` if the GStreamer plugin for `element` is installed.
pub(crate) fn have(element: &str) -> bool {
    gstreamer::init().ok();
    gstreamer::ElementFactory::find(element).is_some()
}

/// Software encoder element description + output caps, as `capture.rs`
/// configures the equivalent VA encoders.
fn software_encoder(codec: VideoCodec, key_int: u32) -> (String, &'static str, &'static str) {
    match codec {
        VideoCodec::H264 => (
            format!("x264enc tune=zerolatency speed-preset=ultrafast key-int-max={key_int}"),
            "x264enc",
            "video/x-h264,stream-format=byte-stream,alignment=au",
        ),
        VideoCodec::H265 => (
            format!("x265enc tune=zerolatency speed-preset=ultrafast key-int-max={key_int}"),
            "x265enc",
            "video/x-h265,stream-format=byte-stream,alignment=au",
        ),
        VideoCodec::Av1 => (
            format!("av1enc usage-profile=realtime cpu-used=8 lag-in-frames=0 keyframe-max-dist={key_int}"),
            "av1enc",
            "video/x-av1,stream-format=obu-stream,alignment=tu",
        ),
    }
}

/// Encode `num_buffers` frames of SMPTE bars with the software encoder for
/// `codec`. `Ok(None)` if that encoder isn't installed (callers skip).
pub(crate) fn encode_clip(codec: VideoCodec, width: u32, height: u32, num_buffers: u32, key_int: u32) -> Result<Option<Vec<Au>>> {
    encode_clip_format(codec, width, height, num_buffers, key_int, "I420")
}

/// [`encode_clip`] with an explicit raw input format (e.g. `Y444`, which makes
/// x264enc emit High 4:4:4 Predictive — a profile no VA decoder supports).
pub(crate) fn encode_clip_format(
    codec: VideoCodec,
    width: u32,
    height: u32,
    num_buffers: u32,
    key_int: u32,
    raw_format: &str,
) -> Result<Option<Vec<Au>>> {
    let (enc, factory, caps) = software_encoder(codec, key_int);
    if !have(factory) {
        return Ok(None);
    }
    let desc = format!(
        "videotestsrc num-buffers={num_buffers} pattern=smpte is-live=false \
         ! video/x-raw,width={width},height={height},framerate=30/1 \
         ! videoconvert ! video/x-raw,format={raw_format} ! {enc} ! {caps} ! appsink name=out sync=false"
    );
    let pipeline = gstreamer::parse::launch(&desc)?
        .downcast::<Pipeline>()
        .map_err(|_| anyhow!("not a pipeline"))?;
    let sink = pipeline
        .by_name("out")
        .and_then(|e: Element| e.downcast::<AppSink>().ok())
        .ok_or_else(|| anyhow!("no appsink"))?;
    pipeline.set_state(State::Playing)?;
    let mut aus: Vec<Au> = Vec::new();
    loop {
        match sink.try_pull_sample(ClockTime::from_seconds(10)) {
            Some(sample) => {
                let buffer = sample.buffer().ok_or_else(|| anyhow!("sample without buffer"))?;
                let map = buffer.map_readable()?;
                aus.push(Au {
                    data: map.as_slice().to_vec(),
                    keyframe: !buffer.flags().contains(BufferFlags::DELTA_UNIT),
                    pts_us: aus.len() as u64 * 33_333,
                });
            }
            None if sink.is_eos() => break,
            None => return Err(anyhow!("{factory} stalled after {} access units", aus.len())),
        }
    }
    pipeline.set_state(State::Null)?;
    Ok(Some(aus))
}

/// Encode `num_buffers` frames of SMPTE bars with the VA hardware encoder for
/// `codec`, exactly as `capture.rs` would (NV12 in, `obu-stream`/`byte-stream`
/// out). `Ok(None)` if the VA encoder element is missing. Unlike the software
/// encoders this exercises the real sender's stream properties, such as
/// `vaav1enc` declaring a 1082-line sequence header for 1080 lines.
pub(crate) fn encode_clip_va(codec: VideoCodec, width: u32, height: u32, num_buffers: u32) -> Result<Option<Vec<Au>>> {
    let (factory, caps) = match codec {
        VideoCodec::H264 => ("vah264enc", "video/x-h264,stream-format=byte-stream,alignment=au"),
        VideoCodec::H265 => ("vah265enc", "video/x-h265,stream-format=byte-stream,alignment=au"),
        VideoCodec::Av1 => ("vaav1enc", "video/x-av1,stream-format=obu-stream,alignment=tu"),
    };
    if !have(factory) {
        return Ok(None);
    }
    let desc = format!(
        "videotestsrc num-buffers={num_buffers} pattern=smpte is-live=false \
         ! video/x-raw,format=NV12,width={width},height={height},framerate=30/1 \
         ! {factory} ! {caps} ! appsink name=out sync=false"
    );
    let pipeline = gstreamer::parse::launch(&desc)?.downcast::<Pipeline>().map_err(|_| anyhow!("not a pipeline"))?;
    let sink = pipeline.by_name("out").and_then(|e: Element| e.downcast::<AppSink>().ok()).ok_or_else(|| anyhow!("no appsink"))?;
    if pipeline.set_state(State::Playing).is_err() {
        return Ok(None);
    }
    let mut aus: Vec<Au> = Vec::new();
    loop {
        match sink.try_pull_sample(ClockTime::from_seconds(10)) {
            Some(sample) => {
                let buffer = sample.buffer().ok_or_else(|| anyhow!("sample without buffer"))?;
                let map = buffer.map_readable()?;
                aus.push(Au {
                    data: map.as_slice().to_vec(),
                    keyframe: !buffer.flags().contains(BufferFlags::DELTA_UNIT),
                    pts_us: aus.len() as u64 * 33_333,
                });
            }
            None if sink.is_eos() => break,
            None => return Err(anyhow!("{factory} stalled after {} access units", aus.len())),
        }
    }
    pipeline.set_state(State::Null)?;
    Ok(Some(aus))
}
