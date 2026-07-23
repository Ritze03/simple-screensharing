//! Periodic preview/thumbnail sender (spec §7.1).
//!
//! Deliberately capture-agnostic: this module has zero GStreamer/portal
//! dependency and never touches `capture.rs`. It is fed a generic
//! [`FrameStream`] of already-decoded RGB8 frames (whatever the eventual
//! capture pipeline produces, converted upstream) so that the preview
//! sending logic — downscale, JPEG-encode, encrypt, pack, send — can be
//! built, tested, and landed independently of the capture backend.
//!
//! # Nonce sharing
//!
//! Per spec §5.1, every video/audio/preview blob on a connection draws from
//! the *same* shared, atomically-incrementing counter, never a
//! stream-local one. [`spawn_preview_task`] therefore always draws from
//! `client.nonce_sequence()` — the identical [`crate::net::NonceSequence`]
//! instance a future `capture.rs` uses for video/audio frames — rather than
//! constructing a separate counter of its own.
//!
//! # Wire format
//!
//! One preview "send" is: JPEG bytes (downscaled to at most
//! [`PREVIEW_MAX_WIDTH`] px wide, quality [`PREVIEW_JPEG_QUALITY`]) AEAD-
//! encrypted via [`crate::net::encrypt_preview`] (AAD `[version, stream_id]`,
//! *no* PTS prefix — a preview is a still, not a timed frame, unlike
//! `encrypt_frame`), prefixed with a packed [`crate::protocol::PreviewHeader`]
//! (15 bytes, cleartext), and written whole to a fresh QUIC unidirectional
//! stream (`connection.open_uni()` → `write_all` → `send.finish()`). This
//! exactly matches what `net.rs`'s `handle_preview_stream` (the receiving
//! side, already implemented) unpacks and decrypts.

use std::pin::Pin;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::{Stream, StreamExt};
use image::{ImageBuffer, Rgb};
use tokio::sync::oneshot;

use crate::net::{self, ScreenshareClient};
use crate::protocol::{self, PreviewHeader, StreamId};

/// How often a fresh preview is captured and sent (spec §7.1: "periodically,
/// e.g. every 30 seconds").
pub const PREVIEW_INTERVAL: Duration = Duration::from_secs(30);

/// Previews are downscaled to at most this many pixels wide, preserving
/// aspect ratio, and never upscaled past their source width.
pub const PREVIEW_MAX_WIDTH: u32 = 320;

/// JPEG quality passed to the `image` crate's encoder.
pub const PREVIEW_JPEG_QUALITY: u8 = 75;

/// One tightly-packed RGB8 frame, capture-backend-agnostic. `rgb.len()` must
/// be exactly `width * height * 3`.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// A stream of frames this task samples from every [`PREVIEW_INTERVAL`].
/// Only the most-recently-observed frame at tick time is ever sent — this
/// is a "latest thumbnail" feed, not something that needs to keep up with
/// every frame the source produces.
pub type FrameStream = Pin<Box<dyn Stream<Item = RawFrame> + Send>>;

// ---------------------------------------------------------------------
// Downscale + JPEG encode
// ---------------------------------------------------------------------

/// Downscale `frame` to at most [`PREVIEW_MAX_WIDTH`] px wide (Triangle
/// filter, aspect-ratio preserved, never upscaled) and JPEG-encode the
/// result at [`PREVIEW_JPEG_QUALITY`]. Returns an error (never panics) if
/// `frame.rgb` isn't exactly `width * height * 3` bytes, or either
/// dimension is zero.
fn downscale_and_encode_jpeg(frame: &RawFrame) -> Result<Vec<u8>> {
    if frame.width == 0 || frame.height == 0 {
        anyhow::bail!(
            "malformed preview frame: zero dimension ({}x{})",
            frame.width,
            frame.height
        );
    }
    let expected_len = frame.width as usize * frame.height as usize * 3;
    if frame.rgb.len() != expected_len {
        anyhow::bail!(
            "malformed preview frame: rgb buffer is {} bytes, expected {} for a tightly-packed {}x{} RGB8 image",
            frame.rgb.len(),
            expected_len,
            frame.width,
            frame.height
        );
    }

    let source: ImageBuffer<Rgb<u8>, Vec<u8>> =
        ImageBuffer::from_raw(frame.width, frame.height, frame.rgb.clone())
            .ok_or_else(|| anyhow!("failed to build image buffer from raw RGB frame"))?;

    // Never upscale a source narrower than the target.
    let target_width = frame.width.min(PREVIEW_MAX_WIDTH);
    let target_height = if target_width == frame.width {
        frame.height
    } else {
        ((frame.height as u64 * target_width as u64) / frame.width as u64).max(1) as u32
    };

    let resized;
    let to_encode: &ImageBuffer<Rgb<u8>, Vec<u8>> = if target_width == frame.width {
        &source
    } else {
        resized = image::imageops::resize(
            &source,
            target_width,
            target_height,
            image::imageops::FilterType::Triangle,
        );
        &resized
    };

    let mut jpeg_bytes = Vec::new();
    let mut encoder =
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg_bytes, PREVIEW_JPEG_QUALITY);
    encoder
        .encode_image(to_encode)
        .context("failed to JPEG-encode preview frame")?;
    Ok(jpeg_bytes)
}

// ---------------------------------------------------------------------
// Public API: spawn_preview_task / PreviewHandle
// ---------------------------------------------------------------------

/// A running preview-sender task started by [`spawn_preview_task`]. Call
/// [`PreviewHandle::stop`] to shut it down; dropping it without calling
/// `stop` leaves the background task running until the process exits.
pub struct PreviewHandle {
    stop_tx: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl PreviewHandle {
    /// Signal the background task to stop and wait for it to finish.
    pub async fn stop(self) {
        // If the receiver's already gone the task has already exited on its
        // own (e.g. the connection died) — nothing left to signal.
        let _ = self.stop_tx.send(());
        if let Err(e) = self.task.await {
            tracing::warn!("preview task did not shut down cleanly: {e}");
        }
    }
}

/// Spawn a background task that, every [`PREVIEW_INTERVAL`], takes the
/// most-recently-observed frame from `frames`, downscales/JPEG-encodes it,
/// encrypts it (drawing the next nonce from `client`'s shared
/// [`crate::net::NonceSequence`] — see the module docs), and sends it on a
/// fresh QUIC uni-stream as `stream_id`'s preview.
pub fn spawn_preview_task(
    client: ScreenshareClient,
    stream_id: StreamId,
    frames: FrameStream,
) -> PreviewHandle {
    let (stop_tx, stop_rx) = oneshot::channel();
    let task = tokio::spawn(preview_loop(client, stream_id, frames, stop_rx));
    PreviewHandle { stop_tx, task }
}

async fn preview_loop(
    client: ScreenshareClient,
    stream_id: StreamId,
    mut frames: FrameStream,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let mut interval = tokio::time::interval(PREVIEW_INTERVAL);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // First tick fires immediately; skip it so we don't fire before any
    // frame has arrived (send_one_preview no-ops on None anyway, but this
    // avoids a pointless immediate wakeup).
    interval.tick().await;

    let mut latest: Option<RawFrame> = None;
    let mut frames_done = false;

    loop {
        tokio::select! {
            _ = &mut stop_rx => {
                return;
            }
            frame = frames.next(), if !frames_done => {
                match frame {
                    Some(f) => latest = Some(f),
                    None => frames_done = true,
                }
            }
            _ = interval.tick() => {
                if let Some(frame) = latest.as_ref() {
                    if let Err(e) = send_one_preview(&client, stream_id, frame).await {
                        tracing::warn!("failed to send preview for stream {stream_id}: {e:#}");
                    }
                }
            }
        }
    }
}

async fn send_one_preview(client: &ScreenshareClient, stream_id: StreamId, frame: &RawFrame) -> Result<()> {
    let jpeg = downscale_and_encode_jpeg(frame)?;

    let (nonce, counter) = client
        .nonce_sequence()
        .next_nonce(stream_id)
        .map_err(|e| anyhow!("{e}"))?;

    let room_key = client.room_key();
    let ciphertext = net::encrypt_preview(&room_key, stream_id, nonce, &jpeg)?;

    let header = PreviewHeader::new(client.conn_id(), stream_id, counter);
    let mut out = Vec::with_capacity(protocol::PREVIEW_HEADER_LEN + ciphertext.len());
    out.extend_from_slice(&header.pack());
    out.extend_from_slice(&ciphertext);

    let connection = client.connection();
    let mut send = connection
        .open_uni()
        .await
        .context("failed to open preview uni-stream")?;
    send.write_all(&out).await.context("failed to write preview uni-stream")?;
    send.finish().context("failed to finish preview uni-stream")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::{decrypt_preview, encrypt_preview};
    use crate::protocol::ConnId;
    use image::GenericImageView;

    fn solid_frame(width: u32, height: u32) -> RawFrame {
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height {
            for x in 0..width {
                rgb.push((x * 255 / width.max(1)) as u8);
                rgb.push((y * 255 / height.max(1)) as u8);
                rgb.push(128);
            }
        }
        RawFrame { width, height, rgb }
    }

    fn decoded_dimensions(jpeg: &[u8]) -> (u32, u32) {
        let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
            .expect("test JPEG should decode");
        img.dimensions()
    }

    #[test]
    fn downscales_16_9_source_to_target_width() {
        let frame = solid_frame(1920, 1080);
        let jpeg = downscale_and_encode_jpeg(&frame).expect("encode should succeed");
        let (w, h) = decoded_dimensions(&jpeg);
        assert_eq!(w, PREVIEW_MAX_WIDTH);
        // 1920x1080 -> 320x180, aspect ratio preserved.
        assert_eq!(h, 180);
    }

    #[test]
    fn downscales_4_3_source_to_target_width() {
        let frame = solid_frame(1024, 768);
        let jpeg = downscale_and_encode_jpeg(&frame).expect("encode should succeed");
        let (w, h) = decoded_dimensions(&jpeg);
        assert_eq!(w, PREVIEW_MAX_WIDTH);
        // 1024x768 -> 320x240, aspect ratio preserved.
        assert_eq!(h, 240);
    }

    #[test]
    fn does_not_upscale_a_narrower_than_target_source() {
        let frame = solid_frame(200, 100);
        let jpeg = downscale_and_encode_jpeg(&frame).expect("encode should succeed");
        let (w, h) = decoded_dimensions(&jpeg);
        assert_eq!(w, 200, "a source narrower than the target width must not be upscaled");
        assert_eq!(h, 100);
    }

    #[test]
    fn rejects_malformed_input_buffer() {
        let frame = RawFrame {
            width: 100,
            height: 100,
            // Should be 100*100*3 = 30000 bytes; give it far too few.
            rgb: vec![0u8; 10],
        };
        assert!(downscale_and_encode_jpeg(&frame).is_err());
    }

    /// Full wire-format round trip, matching exactly how `net.rs`'s real
    /// `handle_preview_stream` consumes an incoming preview uni-stream:
    /// encode -> encrypt -> pack header -> unpack header -> rebuild nonce
    /// from (conn_id, counter) -> decrypt -> JPEG-decode.
    #[test]
    fn wire_format_round_trips_like_the_receiver() {
        let frame = solid_frame(640, 360);
        let jpeg = downscale_and_encode_jpeg(&frame).expect("encode should succeed");

        let key = [9u8; 32];
        let conn_id: ConnId = 0x1122_3344_5566_7788;
        let stream_id: StreamId = 7;
        let counter: u32 = 42;
        let nonce = build_nonce_for_test(conn_id, counter);

        let ciphertext = encrypt_preview(&key, stream_id, nonce, &jpeg).expect("encrypt_preview");

        let header = PreviewHeader::new(conn_id, stream_id, counter);
        let mut wire = Vec::new();
        wire.extend_from_slice(&header.pack());
        wire.extend_from_slice(&ciphertext);

        // Receiver side: unpack header, rebuild nonce, decrypt, JPEG-decode.
        let (parsed_header, received_ciphertext) = PreviewHeader::unpack(&wire).expect("unpack");
        assert_eq!(parsed_header.stream_type, crate::protocol::STREAM_TYPE_PREVIEW);
        assert_eq!(parsed_header.conn_id, conn_id);
        assert_eq!(parsed_header.stream_id, stream_id);
        assert_eq!(parsed_header.counter, counter);

        let rebuilt_nonce = build_nonce_for_test(parsed_header.conn_id, parsed_header.counter);
        let decrypted = decrypt_preview(&key, parsed_header.stream_id, rebuilt_nonce, received_ciphertext)
            .expect("decrypt_preview");
        assert_eq!(decrypted, jpeg);

        let (w, h) = decoded_dimensions(&decrypted);
        assert_eq!(w, PREVIEW_MAX_WIDTH);
        assert_eq!(h, 180);
    }

    #[test]
    fn rejects_tampered_ciphertext() {
        let frame = solid_frame(320, 180);
        let jpeg = downscale_and_encode_jpeg(&frame).expect("encode should succeed");
        let key = [3u8; 32];
        let nonce = build_nonce_for_test(1, 1);
        let mut ciphertext = encrypt_preview(&key, 0, nonce, &jpeg).expect("encrypt");
        let last = ciphertext.len() - 1;
        ciphertext[last] ^= 0xFF;
        assert!(decrypt_preview(&key, 0, nonce, &ciphertext).is_err());
    }

    #[test]
    fn rejects_wrong_stream_id_aad() {
        let frame = solid_frame(320, 180);
        let jpeg = downscale_and_encode_jpeg(&frame).expect("encode should succeed");
        let key = [3u8; 32];
        let nonce = build_nonce_for_test(1, 1);
        let ciphertext = encrypt_preview(&key, 5, nonce, &jpeg).expect("encrypt");
        assert!(decrypt_preview(&key, 6, nonce, &ciphertext).is_err());
    }

    /// Mirrors `net.rs`'s private `build_nonce` layout (spec §5): `conn_id
    /// BE ++ counter BE`. Inlined here since `build_nonce` is a private
    /// helper of `net.rs` (same approach `decode.rs`'s preview tests take).
    fn build_nonce_for_test(conn_id: ConnId, counter: u32) -> [u8; 12] {
        let mut nonce = [0u8; 12];
        nonce[0..8].copy_from_slice(&conn_id.to_be_bytes());
        nonce[8..12].copy_from_slice(&counter.to_be_bytes());
        nonce
    }
}
