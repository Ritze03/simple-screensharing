//! Codec sniffing: decides whether a reassembled video access unit is
//! H.264, H.265 or AV1 purely from its bytes.
//!
//! `Why sniff at all:` the wire protocol (`protocol.rs`'s `AnnounceStream` /
//! `StreamInfo` and the media header) carries **no codec tag**, and that is
//! deliberate — a sender can switch codec by editing a destination, and a tag
//! that can go stale is worse than none. The bitstream cannot go stale, so the
//! receiver reads the codec out of it. This is the Rust port of the
//! discrimination the Electron sidecar's `video_server.rs` does to derive
//! WebCodecs codec strings; only the *codec* decision is kept here (profile /
//! level strings are a browser concern).
//!
//! # What the sender emits (`capture.rs`)
//!
//! * H.264 / H.265: `stream-format=byte-stream,alignment=au` — Annex-B, every
//!   access unit starts with a `00 00 01` / `00 00 00 01` start code.
//! * AV1: `stream-format=obu-stream,alignment=tu` — raw OBUs, `has_size_field=1`
//!   on each, **no** start codes, first byte is a temporal delimiter (`0x12`).
//!
//! # The decision procedure
//!
//! 1. **Annex-B vs OBU** is structural, not a guess. An AV1 OBU header byte
//!    is `forbidden(1)=0 | type(4) | extension(1) | has_size(1) | reserved(1)=0`;
//!    a leading `0x00` byte would mean `obu_type = 0`, which is reserved, so no
//!    AV1 stream starts with a zero byte. Every Annex-B access unit does start
//!    with zero bytes (the start code). So `au[0] == 0` ⇔ Annex-B.
//! 2. **H.264 vs H.265** within Annex-B looks at *parameter-set* NAL units
//!    only, because those are the only NALs whose type is unambiguous:
//!    * H.264 SPS: header byte `0b0_RR_00111` with `nal_ref_idc` (`RR`) ≠ 0 —
//!      `0x27`, `0x47`, `0x67` (the spec forbids `nal_ref_idc == 0` on an SPS).
//!    * H.265 VPS/SPS/PPS: types 32/33/34 → first header byte `0x40..=0x45`,
//!      second byte exactly `0x01` (`nuh_layer_id = 0`, `TemporalId = 0` — the
//!      spec requires parameter sets to carry `TemporalId = 0`).
//!
//!    The two byte sets are disjoint (H.264 SPS has low five bits `00111`,
//!    HEVC param sets are `0100_0xx0`/`0100_010x`), so a match is never
//!    ambiguous. Slice NALs are deliberately **not** used: H.264's `0x65`
//!    (IDR) and HEVC's `0x26` (IDR_W_RADL) can collide with each other under
//!    the other codec's reading, and the first thing a decoder needs from a
//!    keyframe is its parameter sets anyway.
//! 3. **AV1** is recognised by walking the OBU chain from byte 0 (each OBU
//!    header validated: forbidden/reserved bits clear, `has_size_field` set,
//!    known type, size fits) until a Sequence Header OBU (type 1) is reached.
//!
//! `sniff` therefore returns `Some` only for an access unit that carries the
//! stream's parameter sets / sequence header — i.e. a decodable keyframe. A
//! delta frame, or a keyframe whose parameter sets weren't repeated inline,
//! yields `None` and the caller waits for the next keyframe.

/// A video codec the decoder can handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VideoCodec {
    H264,
    H265,
    Av1,
}

impl VideoCodec {
    /// Short human-readable name, for logs and stats.
    pub fn name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H.264",
            VideoCodec::H265 => "H.265",
            VideoCodec::Av1 => "AV1",
        }
    }
}

/// How much of an access unit [`sniff`] looks at. Parameter sets lead a
/// keyframe (VPS/SPS/PPS, or the sequence header right after the temporal
/// delimiter), so a short prefix is enough; bounding the scan keeps the cost
/// independent of keyframe size (a 1080p keyframe can be several hundred KB).
const SNIFF_SCAN_LIMIT: usize = 16 * 1024;

/// H.265 `nal_unit_type`s of VPS / SPS / PPS (spec Table 7-1).
const HEVC_NAL_VPS: u8 = 32;
const HEVC_NAL_PPS: u8 = 34;

/// AV1 `obu_type`s (spec §6.2.2).
const AV1_OBU_SEQUENCE_HEADER: u8 = 1;

/// Decide which codec `access_unit` belongs to. See the module docs for the
/// procedure. `None` means "not recognisable as a decodable keyframe".
pub fn sniff(access_unit: &[u8]) -> Option<VideoCodec> {
    let au = &access_unit[..access_unit.len().min(SNIFF_SCAN_LIMIT)];
    match au.first() {
        None => None,
        Some(0) => sniff_annex_b(au),
        Some(_) => sniff_av1(au),
    }
}

/// Walk the Annex-B NAL units in `au` and return the codec of the first
/// parameter-set NAL that identifies one.
fn sniff_annex_b(au: &[u8]) -> Option<VideoCodec> {
    let mut i = 0usize;
    // `i + 2 < len`: need the three start-code bytes in range.
    while i + 2 < au.len() {
        // Start codes are `00 00 01` (the 4-byte form just has an extra
        // leading zero, which this loop skips as it advances by one).
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            let nal = i + 3;
            if let Some(&b0) = au.get(nal) {
                if is_h264_sps_header(b0) {
                    return Some(VideoCodec::H264);
                }
                if let Some(&b1) = au.get(nal + 1) {
                    if is_h265_param_set_header(b0, b1) {
                        return Some(VideoCodec::H265);
                    }
                }
            }
            i = nal;
        } else {
            i += 1;
        }
    }
    None
}

/// H.264 SPS: `forbidden_zero_bit = 0`, `nal_ref_idc != 0`, `type = 7`.
fn is_h264_sps_header(b0: u8) -> bool {
    b0 & 0x9f == 0x07 && b0 & 0x60 != 0
}

/// H.265 VPS/SPS/PPS: `forbidden_zero_bit = 0`, type in 32..=34,
/// `nuh_layer_id = 0` (msb in byte 0, low five bits in byte 1) and
/// `nuh_temporal_id_plus1 = 1`.
fn is_h265_param_set_header(b0: u8, b1: u8) -> bool {
    if b0 & 0x81 != 0 {
        return false; // forbidden bit, or layer-id msb set
    }
    let nal_type = (b0 >> 1) & 0x3f;
    (HEVC_NAL_VPS..=HEVC_NAL_PPS).contains(&nal_type) && b1 == 0x01
}

/// Read a `leb128` (AV1 spec §4.10.5): value and bytes consumed.
fn read_leb128(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, &byte) in data.iter().enumerate().take(8) {
        value |= u64::from(byte & 0x7f) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// Walk a raw OBU stream from byte 0; `Some(Av1)` once a well-formed Sequence
/// Header OBU is reached through a chain of well-formed OBUs.
fn sniff_av1(au: &[u8]) -> Option<VideoCodec> {
    let mut offset = 0usize;
    while offset < au.len() {
        let header = au[offset];
        let forbidden = header & 0x80 != 0;
        let obu_type = (header >> 3) & 0x0f;
        let extension = (header >> 2) & 1 == 1;
        let has_size = (header >> 1) & 1 == 1;
        let reserved = header & 1 == 1;
        // Types 0 and 9..=14 are reserved in the spec. 0 is exactly the
        // "starts with a zero byte" case that means Annex-B, never AV1.
        let known_type = matches!(obu_type, 1..=8 | 15);
        if forbidden || reserved || !known_type || !has_size {
            return None;
        }
        let size_at = offset + 1 + usize::from(extension);
        let (size, leb_len) = read_leb128(au.get(size_at..)?)?;
        let payload_start = size_at + leb_len;
        if obu_type == AV1_OBU_SEQUENCE_HEADER {
            // `seq_profile` is the first 3 bits; 0..=2 are the defined
            // profiles. Requiring it filters out garbage that merely happens
            // to look like an OBU header. The payload may extend past the
            // scan window / a truncated sample; only its first byte is read.
            let first = *au.get(payload_start)?;
            return (size >= 1 && first >> 5 <= 2).then_some(VideoCodec::Av1);
        }
        offset = payload_start.checked_add(usize::try_from(size).ok()?)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real encoder output, captured on this machine (AMD RX 7900 XTX, GStreamer
    // 1.28.x) with `videotestsrc ! <enc> ! <byte-stream|obu-stream caps> !
    // filesink` — first bytes of the first access unit. Real hardware / library
    // output, not synthesised.

    /// x264enc (software): AUD, SPS, PPS, SEI...
    const X264: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x09, 0x10, 0x00, 0x00, 0x00, 0x01, 0x67, 0xf4, 0x00, 0x0d, 0x90, 0xd9, 0x64, 0x05, 0x07, 0xec,
        0x05, 0xa8, 0x30, 0x30, 0x32, 0x00, 0x00, 0x03, 0x00, 0x02, 0x00, 0x00, 0x03, 0x00, 0x79, 0x1e, 0x28, 0x54, 0x90, 0x00,
        0x00, 0x00, 0x01, 0x68, 0xeb, 0xcc, 0x44, 0x84, 0x40, 0x00, 0x00, 0x01, 0x06, 0x05, 0xff, 0xff,
    ];

    /// vah264enc: SPS (`nal_ref_idc = 1`, header 0x27), PPS (0x28), IDR slice (0x25).
    const VAH264: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x27, 0x64, 0x00, 0x0d, 0xac, 0x56, 0xc1, 0x41, 0xfb, 0xff, 0x00, 0x01, 0x00, 0x01, 0x10, 0x00,
        0x00, 0x03, 0x00, 0x10, 0x00, 0x00, 0x03, 0x03, 0xc9, 0xf0, 0x88, 0x46, 0x48, 0x00, 0x00, 0x00, 0x01, 0x28, 0xee, 0x3c,
        0x30, 0x00, 0x00, 0x00, 0x01, 0x25, 0x88, 0x81, 0x00, 0x16, 0x42, 0x7f,
    ];

    /// vah265enc: VPS (0x40 0x01), SPS (0x42 0x01), PPS (0x44 0x01).
    const VAH265: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00, 0x90, 0x00, 0x00, 0x03,
        0x00, 0x00, 0x03, 0x00, 0x3c, 0x2c, 0x09, 0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x01, 0x60, 0x00, 0x00, 0x03, 0x00,
        0x90, 0x00, 0x00, 0x03, 0x00, 0x00, 0x03, 0x00, 0x3c, 0xa0, 0x0a, 0x08, 0x0f, 0x16, 0x8b, 0x92, 0x42, 0x95, 0xa7, 0xff,
    ];

    /// x265enc (software).
    const X265: &[u8] = &[
        0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0c, 0x01, 0xff, 0xff, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00, 0x98, 0x08, 0x00, 0x00,
        0x03, 0x00, 0x00, 0x3f, 0x92, 0x80, 0x90, 0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x04, 0x08, 0x00, 0x00, 0x03, 0x00,
        0x98, 0x08, 0x00, 0x00, 0x03, 0x00, 0x00, 0x3f, 0x90, 0x01, 0x41, 0x01, 0xe2, 0x29, 0x4b, 0x25,
    ];

    /// vaav1enc: temporal delimiter (`12 00`) then sequence header (`0a 0b ..`).
    const VAAV1: &[u8] = &[
        0x12, 0x00, 0x0a, 0x0b, 0x00, 0x00, 0x00, 0x04, 0x3c, 0xff, 0xbc, 0x02, 0x15, 0x00, 0x40, 0x1a, 0x0f, 0x10, 0x02, 0x40,
        0x02, 0x41, 0xc7, 0x1c, 0x70, 0x50, 0x00, 0x02, 0x08, 0x61, 0x9e, 0x79, 0x22, 0xec, 0x97, 0x80, 0x00, 0xc8, 0xf2, 0x94,
    ];

    /// svtav1enc (software, 10-bit seq header, size 14).
    const SVTAV1: &[u8] = &[
        0x12, 0x00, 0x0a, 0x0e, 0x02, 0x00, 0x00, 0x05, 0x21, 0xe7, 0xfd, 0xe2, 0x57, 0xca, 0x83, 0x03, 0x03, 0x02, 0x32, 0xcf,
        0x1f, 0x10, 0x00, 0x88, 0x20, 0x81, 0x45, 0x08, 0x20, 0x50, 0x3c, 0xf8,
    ];

    /// libaom `av1enc` (software).
    const AOMAV1: &[u8] = &[
        0x12, 0x00, 0x0a, 0x0b, 0x20, 0x00, 0x00, 0x04, 0x3c, 0xff, 0xbf, 0x83, 0xf7, 0xf3, 0x04, 0x32, 0x8c, 0x2b, 0x15, 0xaf,
        0xac, 0x00, 0x40, 0x00, 0x00, 0xf4, 0xb9, 0x19, 0x03, 0xb4, 0x51, 0xcf,
    ];

    #[test]
    fn sniffs_real_h264_streams() {
        assert_eq!(sniff(X264), Some(VideoCodec::H264), "AUD first, then SPS (x264enc)");
        assert_eq!(sniff(VAH264), Some(VideoCodec::H264), "SPS with nal_ref_idc=1 (vah264enc)");
    }

    #[test]
    fn sniffs_real_h265_streams() {
        assert_eq!(sniff(VAH265), Some(VideoCodec::H265), "vah265enc");
        assert_eq!(sniff(X265), Some(VideoCodec::H265), "x265enc");
    }

    #[test]
    fn sniffs_real_av1_streams() {
        assert_eq!(sniff(VAAV1), Some(VideoCodec::Av1), "vaav1enc");
        assert_eq!(sniff(SVTAV1), Some(VideoCodec::Av1), "svtav1enc");
        assert_eq!(sniff(AOMAV1), Some(VideoCodec::Av1), "libaom av1enc");
    }

    /// The same bytes with only the start-code length changed (3-byte instead
    /// of 4-byte) still sniff correctly.
    #[test]
    fn three_byte_start_codes_work() {
        let mut au = VAH264.to_vec();
        au.remove(0);
        assert_eq!(sniff(&au), Some(VideoCodec::H264));
        let mut au = VAH265.to_vec();
        au.remove(0);
        assert_eq!(sniff(&au), Some(VideoCodec::H265));
    }

    /// Delta frames / keyframes without inline parameter sets are not
    /// decodable on their own, so they must not be recognised.
    #[test]
    fn delta_frames_and_bare_slices_are_not_recognised() {
        // H.264 non-IDR slice (0x41), H.264 IDR slice only (0x65).
        assert_eq!(sniff(&[0, 0, 0, 1, 0x41, 0x9a, 0x00, 0x11, 0x22]), None);
        assert_eq!(sniff(&[0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x11, 0x22]), None);
        // H.265 IDR_W_RADL slice only (0x26 0x01), TRAIL_R (0x02 0x01).
        assert_eq!(sniff(&[0, 0, 0, 1, 0x26, 0x01, 0xaf, 0x11, 0x22]), None);
        assert_eq!(sniff(&[0, 0, 0, 1, 0x02, 0x01, 0xd0, 0x11, 0x22]), None);
        // AV1 temporal delimiter + frame OBU (type 6), no sequence header.
        assert_eq!(sniff(&[0x12, 0x00, 0x32, 0x03, 0xaa, 0xbb, 0xcc]), None);
        // AV1 temporal delimiter alone.
        assert_eq!(sniff(&[0x12, 0x00]), None);
    }

    #[test]
    fn junk_and_empty_input_is_not_recognised() {
        assert_eq!(sniff(&[]), None);
        assert_eq!(sniff(&[0]), None);
        assert_eq!(sniff(&[0, 0, 0, 1]), None);
        assert_eq!(sniff(&[0xff; 64]), None);
        assert_eq!(sniff(&[0x80, 0x00, 0x00]), None, "forbidden bit set");
        assert_eq!(sniff(b"GET / HTTP/1.1\r\n\r\n"), None);
        // Truncated mid-way through a NAL header.
        assert_eq!(sniff(&[0, 0, 0, 1, 0x40]), None, "HEVC VPS header byte alone: second byte missing");
    }

    /// The H.264 SPS and HEVC parameter-set header bytes must never be
    /// confused for one another: that disjointness is what makes the sniff
    /// decision unambiguous (module docs, step 2).
    #[test]
    fn h264_and_h265_parameter_set_headers_are_disjoint() {
        for b0 in 0u8..=255 {
            for b1 in [0x01u8, 0x00, 0x09, 0x64, 0xff] {
                assert!(
                    !(is_h264_sps_header(b0) && is_h265_param_set_header(b0, b1)),
                    "byte 0x{b0:02x} 0x{b1:02x} claims to be both an H.264 SPS and an HEVC parameter set"
                );
            }
        }
        // Spot checks of the intended matches.
        for b0 in [0x27, 0x47, 0x67] {
            assert!(is_h264_sps_header(b0));
        }
        assert!(!is_h264_sps_header(0x07), "SPS with nal_ref_idc == 0 is illegal");
        for b0 in [0x40, 0x42, 0x44] {
            assert!(is_h265_param_set_header(b0, 0x01));
            assert!(!is_h265_param_set_header(b0, 0x02), "TemporalId != 0 is illegal on a parameter set");
        }
        assert!(!is_h265_param_set_header(0x46, 0x01), "AUD is not a parameter set");
    }

    /// An AV1 access unit never starts with a zero byte (reserved obu_type 0),
    /// so Annex-B-vs-OBU is decided by the very first byte — a truncated OBU
    /// stream whose sequence header OBU is cut short still works as long as
    /// its first payload byte is there, and a size field that overruns the
    /// buffer is fine for the sequence header (only the first byte is read).
    #[test]
    fn av1_sequence_header_may_be_truncated() {
        // TD, then seq header OBU declaring 11 payload bytes but only 2 present.
        assert_eq!(sniff(&[0x12, 0x00, 0x0a, 0x0b, 0x00, 0x00]), Some(VideoCodec::Av1));
        // TD, seq header whose payload is entirely cut off.
        assert_eq!(sniff(&[0x12, 0x00, 0x0a, 0x0b]), None);
        // seq_profile 7 does not exist: not AV1.
        assert_eq!(sniff(&[0x12, 0x00, 0x0a, 0x0b, 0xe0, 0x00]), None);
    }

    /// A start code appearing later in the buffer than 16 KiB is not looked at.
    #[test]
    fn scan_is_bounded() {
        let mut au = vec![0u8; 0];
        au.extend_from_slice(&[0, 0, 0, 1, 0x06]); // an SEI NAL...
        au.extend(std::iter::repeat_n(0xaa, SNIFF_SCAN_LIMIT)); // ...of 16 KiB
        au.extend_from_slice(&VAH264[..]); // SPS beyond the window
        assert_eq!(sniff(&au), None);
    }
}
