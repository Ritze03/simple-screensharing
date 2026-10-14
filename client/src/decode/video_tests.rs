//! Tests for the codec-agnostic, VA-first video decode path: codec detection,
//! hardware/software selection, NV12 output, mid-stream codec switching,
//! hardware-failure fallback and the decode/present statistics.
//!
//! Streams are encoded with *software* encoders (`test_util`) so the inputs
//! don't depend on a GPU; whether the VA *decoder* is used is detected from
//! the installed elements and asserted either way.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::codec::VideoCodec;
use super::frame::{DecodedFrame, FrameSource, Shared};
use super::test_util::{encode_clip, encode_clip_format, have, Au};
use super::video::{Backend, Worker};
use super::StreamDecoder;
use crate::net::DecryptedMediaFrame;

const W: u32 = 320;
const H: u32 = 240;
const STREAM: u8 = 3;

fn va_element(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "vah264dec",
        VideoCodec::H265 => "vah265dec",
        VideoCodec::Av1 => "vaav1dec",
    }
}

fn sw_element(codec: VideoCodec) -> &'static str {
    match codec {
        VideoCodec::H264 => "avdec_h264",
        VideoCodec::H265 => "avdec_h265",
        VideoCodec::Av1 => "av1dec",
    }
}

fn clip(codec: VideoCodec, frames: u32, key_int: u32) -> Option<Vec<Au>> {
    gstreamer::init().expect("gstreamer init");
    super::test_util::init_logging();
    match encode_clip(codec, W, H, frames, key_int).expect("encode failed") {
        Some(aus) => Some(aus),
        None => {
            eprintln!("SKIP: no software encoder for {}", codec.name());
            None
        }
    }
}

fn media(au: &Au) -> DecryptedMediaFrame {
    DecryptedMediaFrame {
        conn_id: 1,
        stream_id: STREAM,
        pts: au.pts_us,
        keyframe: au.keyframe,
        stalled: false,
        payload: au.data.clone(),
    }
}

/// Poll `cond` until true or `secs` elapse.
fn wait_for(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    cond()
}

/// SMPTE bar sanity on a decoded frame, tolerant of codec loss and of the
/// 601/709 choice: each of the 7 top bars has the right hue.
fn assert_smpte_bars(frame: &DecodedFrame) {
    let rgba = frame.to_rgba();
    let (w, h) = (frame.width as usize, frame.height as usize);
    let at = |x: usize, y: usize| {
        let i = (y * w + x) * 4;
        (i32::from(rgba[i]), i32::from(rgba[i + 1]), i32::from(rgba[i + 2]))
    };
    let hi = |v: i32| v > 130;
    let lo = |v: i32| v < 90;
    let bar_w = w / 7;
    // SMPTE order: gray, yellow, cyan, green, magenta, red, blue.
    let expect: [(bool, bool, bool); 7] = [
        (true, true, true),
        (true, true, false),
        (false, true, true),
        (false, true, false),
        (true, false, true),
        (true, false, false),
        (false, false, true),
    ];
    for (i, (er, eg, eb)) in expect.into_iter().enumerate() {
        // Sample well inside the bar, in the tall top region.
        let (r, g, b) = at(i * bar_w + bar_w / 2, h / 4);
        let ok = |want: bool, v: i32| if want { hi(v) } else { lo(v) };
        assert!(
            ok(er, r) && ok(eg, g) && ok(eb, b),
            "bar {i}: got rgb({r},{g},{b}), wanted r={er} g={eg} b={eb} (high/low)"
        );
    }
}

fn check_codec(codec: VideoCodec, software_only: bool) {
    let Some(aus) = clip(codec, 30, 15) else { return };
    let mut dec = if software_only {
        StreamDecoder::new_software_only(STREAM).unwrap()
    } else {
        StreamDecoder::new(STREAM).unwrap()
    };
    let source = dec.frames();
    for au in &aus {
        dec.feed(&media(au));
    }
    assert!(
        wait_for(20, || source.frames_decoded() >= aus.len() as u64 - 2),
        "{} (software_only={software_only}): only {} of {} frames decoded",
        codec.name(),
        source.frames_decoded(),
        aus.len()
    );

    let stats = dec.stats();
    assert_eq!(stats.info.codec, Some(codec), "codec should have been sniffed");
    let name = stats.info.decoder_name.clone().expect("decoder name");
    if software_only || !have(va_element(codec)) {
        assert_eq!(name, sw_element(codec));
        assert!(!stats.info.hardware);
    } else {
        // VA element installed: it must be the one in use — a silent software
        // fallback would make the whole rework pointless.
        assert_eq!(name, va_element(codec), "VA decoder available but {name} is running");
        assert!(stats.info.hardware);
    }
    assert_eq!(dec.codec(), Some(codec));
    assert_eq!(dec.decoder_name().as_deref(), Some(name.as_str()));

    // Decoded fps is counted at the decoder output, with nothing consuming.
    assert!(stats.decoded_fps > 0.0, "decoded_fps must count without any latest_frame() poll");
    assert_eq!(stats.presented_fps, 0.0, "nothing presented yet");
    assert_eq!(stats.frames_presented, 0);

    // NV12 output properties.
    let frame = source.latest().expect("a decoded frame");
    assert_eq!((frame.width, frame.height), (W, H));
    assert_eq!(frame.codec, codec);
    assert!(frame.y_stride >= W as usize && frame.uv_stride >= W as usize);
    assert!(frame.y().len() >= frame.y_stride * (H as usize - 1) + W as usize);
    assert!(frame.uv().len() >= frame.uv_stride * (H as usize / 2 - 1) + W as usize);
    assert_smpte_bars(&frame);

    // Legacy adapter yields once per new frame and feeds presented_fps.
    let legacy = dec.latest_frame().expect("legacy frame");
    assert_eq!(legacy.size, [W as usize, H as usize]);
    assert_eq!(dec.stats().frames_presented, 1);
    assert!(dec.presented_fps() > 0.0);
    assert!(dec.latest_frame().is_none() || source.frame_counter() > frame.seq, "same frame must not be yielded twice");
}

#[test]
fn h264_hardware_preferred() {
    check_codec(VideoCodec::H264, false);
}

#[test]
fn h264_software_only() {
    check_codec(VideoCodec::H264, true);
}

#[test]
fn h265_hardware_preferred() {
    check_codec(VideoCodec::H265, false);
}

#[test]
fn h265_software_only() {
    check_codec(VideoCodec::H265, true);
}

#[test]
fn av1_hardware_preferred() {
    check_codec(VideoCodec::Av1, false);
}

#[test]
fn av1_software_only() {
    check_codec(VideoCodec::Av1, true);
}

/// The sender can switch codec by editing a destination: one decoder must
/// follow H.264 -> H.265 -> AV1 -> H.264, rebuilding its pipeline each time,
/// without being told.
#[test]
fn codec_switch_mid_stream_rebuilds_the_pipeline() {
    let order = [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1, VideoCodec::H264];
    let mut clips = Vec::new();
    for c in order {
        match clip(c, 20, 10) {
            Some(aus) => clips.push((c, aus)),
            None => return,
        }
    }
    let mut dec = StreamDecoder::new(STREAM).unwrap();
    let source = dec.frames();
    for (codec, aus) in &clips {
        let before = source.frame_counter();
        for au in aus {
            dec.feed(&media(au));
        }
        assert!(
            wait_for(20, || source.latest().is_some_and(|f| f.codec == *codec && f.seq > before)),
            "no {} frame after switching to it",
            codec.name()
        );
        assert_eq!(dec.codec(), Some(*codec));
        let name = dec.decoder_name().unwrap();
        assert!(name == va_element(*codec) || name == sw_element(*codec), "unexpected decoder {name}");
    }
}

/// Until a keyframe that sniffs as a known codec arrives, nothing is decoded
/// and no codec is reported.
#[test]
fn unrecognised_or_non_key_access_units_are_dropped_until_a_codec_is_known() {
    let Some(aus) = clip(VideoCodec::H264, 20, 10) else { return };
    let mut dec = StreamDecoder::new(STREAM).unwrap();

    // Delta frames only (keyframe flag clear): dropped by the keyframe gate.
    for au in aus.iter().filter(|a| !a.keyframe).take(5) {
        dec.feed(&media(au));
    }
    // A keyframe-flagged blob that isn't any codec: dropped by the sniffer.
    dec.feed(&DecryptedMediaFrame {
        conn_id: 1,
        stream_id: STREAM,
        pts: 0,
        keyframe: true,
        stalled: false,
        payload: vec![0x55; 4096],
    });
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(dec.codec(), None);
    assert_eq!(dec.frames().frames_decoded(), 0);

    // A real keyframe then starts decoding.
    dec.feed(&media(&aus[0]));
    assert!(wait_for(10, || dec.frames().frames_decoded() >= 1));
    assert_eq!(dec.codec(), Some(VideoCodec::H264));
}

/// A renderer polling `frame_counter` uploads only on change and sees
/// strictly increasing sequence numbers; `FrameSource` outlives the decoder.
#[test]
fn frame_source_hands_out_each_frame_once_and_outlives_the_decoder() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<DecodedFrame>();
    assert_send_sync::<FrameSource>();

    let Some(aus) = clip(VideoCodec::H264, 15, 15) else { return };
    let source = {
        let mut dec = StreamDecoder::new(STREAM).unwrap();
        let source = dec.frames();
        assert_eq!(source.frame_counter(), 0);
        assert!(source.latest().is_none());
        for au in &aus {
            dec.feed(&media(au));
        }
        assert!(wait_for(15, || source.frames_decoded() >= aus.len() as u64 - 1));
        source
    }; // decoder dropped here
    let a = source.latest().expect("last frame survives the decoder");
    assert!(source.latest_if_newer(a.seq).is_none(), "nothing newer than the newest");
    assert!(Arc::ptr_eq(&a, &source.latest().unwrap()), "latest() must not copy the frame");
    assert_eq!(source.frame_counter(), a.seq);
    source.note_presented();
    assert_eq!(source.stats().frames_presented, 1);
}

/// Hardware failure must not lose the picture: the access units pushed while
/// the HW decoder was still on probation are replayed into the software
/// pipeline built in its place.
#[test]
fn hardware_failure_falls_back_to_software_and_replays() {
    let Some(aus) = clip(VideoCodec::H264, 10, 10) else { return };
    gstreamer::init().unwrap();
    let shared = Shared::new();
    let mut worker = Worker::new(shared.clone(), true);

    for au in aus.iter().take(3) {
        worker.handle_access_unit(au.pts_us, au.keyframe, au.data.clone());
    }
    let first = worker.backend().expect("a pipeline should be running");
    if have("vah264dec") {
        assert_eq!(first, Backend::Hardware, "VA decoder is installed and should be tried first");
    }

    worker.fall_back_to_software("test-induced failure");
    assert_eq!(worker.backend(), Some(Backend::Software));
    assert_eq!(shared.info_snapshot().decoder_name.as_deref(), Some("avdec_h264"));
    assert!(!shared.info_snapshot().hardware);

    // If HW had been active, its frames are not what we wait on: the new
    // pipeline must produce its own from the replayed keyframe + deltas.
    let base = shared.decoded_total();
    if first == Backend::Hardware {
        assert!(wait_for(10, || shared.decoded_total() >= base + 3), "replayed access units never decoded in software");
    }
    // ...and it keeps decoding live input afterwards.
    let base = shared.decoded_total();
    for au in aus.iter().skip(3) {
        worker.handle_access_unit(au.pts_us, au.keyframe, au.data.clone());
    }
    assert!(wait_for(10, || shared.decoded_total() >= base + 5), "software pipeline stopped decoding");

    // A later, unrelated codec still gets its HW decoder tried (failure is
    // remembered per codec, not globally).
    assert!(worker.hw_failed_contains(VideoCodec::H264));
    assert!(!worker.hw_failed_contains(VideoCodec::H265));
}

/// A stream the VA decoder cannot handle (High 4:4:4 Predictive) must come out
/// of the software decoder instead of leaving the viewer with nothing: the
/// pipeline refuses the stream at negotiation, the HW decoder is demoted for
/// this codec, and the replayed keyframe decodes in software.
#[test]
fn a_stream_the_va_decoder_rejects_is_decoded_in_software() {
    if !have("vah264dec") || !have("x264enc") {
        eprintln!("SKIP: needs both vah264dec and x264enc");
        return;
    }
    gstreamer::init().unwrap();
    super::test_util::init_logging();
    let aus = encode_clip_format(VideoCodec::H264, W, H, 20, 10, "Y444").unwrap().unwrap();
    let mut dec = StreamDecoder::new(STREAM).unwrap();
    let source = dec.frames();
    for au in &aus {
        dec.feed(&media(au));
    }
    assert!(wait_for(15, || source.frames_decoded() >= 10), "4:4:4 stream produced no frames at all");
    assert_eq!(dec.decoder_name().as_deref(), Some("avdec_h264"));
    assert!(!dec.stats().info.hardware);
    let frame = source.latest().unwrap();
    assert_eq!((frame.width, frame.height), (W, H));
    assert_smpte_bars(&frame);
}

/// Feed a VA-encoded 1920x1080 clip into a decoder (told the announced size)
/// and check the frames come out at the visible size. `vaav1enc` on AMD VCN4
/// writes 1082 into the sequence and frame headers for 1080-line input (and
/// `render_and_frame_size_different = 0`), so the VA decoder's caps say
/// 1920x1082 with no crop meta; only the announced size says 1080.
/// H.264/H.265 come out of `vah264dec`/`vah265dec` already at 1080.
fn check_va_1080p(codec: VideoCodec) {
    gstreamer::init().expect("gstreamer init");
    super::test_util::init_logging();
    let Some(aus) = super::test_util::encode_clip_va(codec, 1920, 1080, 30).expect("encode failed") else {
        eprintln!("SKIP: no VA encoder for {}", codec.name());
        return;
    };
    let mut dec = StreamDecoder::new(STREAM).unwrap();
    // What the sender announces (`StreamInfo::width/height`).
    dec.set_display_size(1920, 1080);
    let source = dec.frames();
    for au in &aus {
        dec.feed(&media(au));
    }
    assert!(wait_for(20, || source.frames_decoded() >= aus.len() as u64 - 2), "{}: only {} frames decoded", codec.name(), source.frames_decoded());
    let frame = source.latest().expect("a decoded frame");
    eprintln!("{} decoded via {:?}: {}x{} strides {}/{}", codec.name(), dec.decoder_name(), frame.width, frame.height, frame.y_stride, frame.uv_stride);
    assert_eq!((frame.width, frame.height), (1920, 1080), "{} must decode at the visible size", codec.name());
    assert!(frame.y().len() >= frame.y_stride * 1079 + 1920);
    assert!(frame.uv().len() >= frame.uv_stride * 539 + 1920);
    assert_smpte_bars(&frame);
}

#[test]
fn av1_from_the_va_encoder_decodes_at_1920x1080() {
    check_va_1080p(VideoCodec::Av1);
}

#[test]
fn h264_from_the_va_encoder_decodes_at_1920x1080() {
    check_va_1080p(VideoCodec::H264);
}

#[test]
fn h265_from_the_va_encoder_decodes_at_1920x1080() {
    check_va_1080p(VideoCodec::H265);
}

#[test]
fn visible_rect_prefers_the_decoder_crop_then_the_announced_size() {
    use super::frame::visible_rect;
    assert_eq!(visible_rect(None, None, 1920, 1088), (0, 0, 1920, 1088));
    // Decoder crop wins.
    assert_eq!(visible_rect(Some((0, 0, 1920, 1080)), Some((1280, 720)), 1920, 1088), (0, 0, 1920, 1080));
    assert_eq!(visible_rect(Some((2, 4, 100, 50)), None, 1920, 1088), (2, 4, 100, 50));
    // A broken crop (outside, empty, odd origin, overflow) is ignored.
    assert_eq!(visible_rect(Some((0, 0, 1920, 1090)), None, 1920, 1088), (0, 0, 1920, 1088));
    assert_eq!(visible_rect(Some((0, 0, 0, 10)), None, 1920, 1088), (0, 0, 1920, 1088));
    assert_eq!(visible_rect(Some((1, 0, 100, 100)), None, 1920, 1088), (0, 0, 1920, 1088));
    assert_eq!(visible_rect(Some((u32::MAX, 0, 2, 2)), None, 1920, 1088), (0, 0, 1920, 1088));
    // The announced size crops small padding at the bottom/right only.
    assert_eq!(visible_rect(None, Some((1920, 1080)), 1920, 1082), (0, 0, 1920, 1080));
    assert_eq!(visible_rect(None, Some((1920, 1080)), 1920, 1080), (0, 0, 1920, 1080));
    assert_eq!(visible_rect(None, Some((1900, 1080)), 1920, 1082), (0, 0, 1900, 1080));
    // A frame smaller than announced, or a real resolution change, is left alone.
    assert_eq!(visible_rect(None, Some((1920, 1080)), 1280, 720), (0, 0, 1280, 720));
    assert_eq!(visible_rect(None, Some((1280, 720)), 1920, 1080), (0, 0, 1920, 1080));
    assert_eq!(visible_rect(None, Some((0, 0)), 1920, 1082), (0, 0, 1920, 1082));
}
