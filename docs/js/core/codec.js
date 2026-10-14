// Codec sniffing and WebCodecs codec-string derivation, straight from the bitstream.
//
// The wire protocol carries NO codec tag (deliberately: a tag can go stale when a sender switches
// codec, the bitstream cannot). Ported from `client/src/decode/codec.rs` (sniffing) and
// `client/src-tauri/src/video_server.rs` (codec strings; used by the Electron frontend).
//
// What native senders emit (client/src/capture.rs):
//   H.264 / H.265  Annex-B (`00 00 01` / `00 00 00 01` start codes), one access unit per frame,
//                  SPS/PPS(/VPS) repeated on keyframes
//   AV1            raw OBU stream (has_size_field=1 on every OBU, NO start codes), one temporal
//                  unit per frame, starting with a temporal delimiter (`12 00`); keyframes carry
//                  a Sequence Header OBU

/** How much of an access unit is scanned: parameter sets lead a keyframe. */
const SNIFF_SCAN_LIMIT = 16 * 1024;

export const CODECS = Object.freeze({ H264: "h264", H265: "h265", AV1: "av1" });

const isH264Sps = (b0) => (b0 & 0x9f) === 0x07 && (b0 & 0x60) !== 0;
function isH265ParamSet(b0, b1) {
  if (b0 & 0x81) return false; // forbidden bit, or layer-id msb
  const t = (b0 >> 1) & 0x3f;
  return t >= 32 && t <= 34 && b1 === 0x01;
}

function readLeb128(data, at) {
  let value = 0;
  for (let i = 0; i < 8; i++) {
    const byte = data[at + i];
    if (byte === undefined) return null;
    value += (byte & 0x7f) * 2 ** (7 * i);
    if (!(byte & 0x80)) return { value, len: i + 1 };
  }
  return null;
}

/**
 * Which codec is this access unit? Returns "h264" | "h265" | "av1", or null when the unit carries no
 * parameter sets / sequence header (a delta frame, or a keyframe whose parameter sets were not repeated).
 */
export function sniffCodec(accessUnit) {
  const au = accessUnit.length > SNIFF_SCAN_LIMIT ? accessUnit.subarray(0, SNIFF_SCAN_LIMIT) : accessUnit;
  if (au.length === 0) return null;
  return au[0] === 0 ? sniffAnnexB(au) : sniffAv1(au);
}

function sniffAnnexB(au) {
  let i = 0;
  while (i + 2 < au.length) {
    if (au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1) {
      const nal = i + 3;
      const b0 = au[nal];
      if (b0 !== undefined) {
        if (isH264Sps(b0)) return CODECS.H264;
        const b1 = au[nal + 1];
        if (b1 !== undefined && isH265ParamSet(b0, b1)) return CODECS.H265;
      }
      i = nal;
    } else {
      i++;
    }
  }
  return null;
}

function sniffAv1(au) {
  let offset = 0;
  while (offset < au.length) {
    const header = au[offset];
    const forbidden = (header & 0x80) !== 0;
    const type = (header >> 3) & 0x0f;
    const ext = ((header >> 2) & 1) === 1;
    const hasSize = ((header >> 1) & 1) === 1;
    const reserved = (header & 1) === 1;
    const known = (type >= 1 && type <= 8) || type === 15;
    if (forbidden || reserved || !known || !hasSize) return null;
    const sizeAt = offset + 1 + (ext ? 1 : 0);
    const leb = readLeb128(au, sizeAt);
    if (!leb) return null;
    const payloadStart = sizeAt + leb.len;
    if (type === 1) {
      const first = au[payloadStart];
      if (first === undefined) return null;
      return leb.value >= 1 && first >> 5 <= 2 ? CODECS.AV1 : null;
    }
    offset = payloadStart + leb.value;
  }
  return null;
}

// ---------------------------------------------------------------------
// Annex-B NAL iteration
// ---------------------------------------------------------------------

/** Yield `{ start, end }` byte ranges of NAL units (after their start code). */
export function* annexBNals(au) {
  let i = 0;
  let nalStart = -1;
  while (i + 2 < au.length) {
    if (au[i] === 0 && au[i + 1] === 0 && au[i + 2] === 1) {
      if (nalStart >= 0) {
        // trim trailing zero bytes (the leading zero of a 4-byte start code)
        let end = i;
        while (end > nalStart && au[end - 1] === 0) end--;
        yield { start: nalStart, end };
      }
      nalStart = i + 3;
      i += 3;
    } else {
      i++;
    }
  }
  if (nalStart >= 0 && nalStart < au.length) yield { start: nalStart, end: au.length };
}

/** Remove emulation-prevention bytes (`00 00 03` -> `00 00`). */
export function stripEmulationPrevention(bytes) {
  const out = [];
  let zeros = 0;
  for (const b of bytes) {
    if (zeros >= 2 && b === 0x03) {
      zeros = 0;
      continue;
    }
    out.push(b);
    zeros = b === 0 ? zeros + 1 : 0;
  }
  return Uint8Array.from(out);
}

const hex2 = (n) => n.toString(16).padStart(2, "0");

// ---------------------------------------------------------------------
// H.264: avc1.PPCCLL from the SPS
// ---------------------------------------------------------------------

export function avcCodecString(au) {
  for (const { start, end } of annexBNals(au)) {
    if ((au[start] & 0x1f) === 7 && end - start >= 4) {
      return `avc1.${hex2(au[start + 1])}${hex2(au[start + 2])}${hex2(au[start + 3])}`;
    }
  }
  return null;
}

// ---------------------------------------------------------------------
// H.265: hev1.<profile>.<compat>.<tier><level>[.constraints] from the SPS
// ---------------------------------------------------------------------

function reverseBits32(v) {
  let r = 0;
  for (let i = 0; i < 32; i++) {
    r = (r * 2 + ((v >>> i) & 1)) >>> 0;
  }
  return r >>> 0;
}

export function hevcCodecString(au) {
  for (const { start, end } of annexBNals(au)) {
    const b0 = au[start];
    if (b0 & 0x80) continue;
    if (((b0 >> 1) & 0x3f) !== 33) continue; // SPS only (VPS has a different layout)
    const rbsp = stripEmulationPrevention(au.subarray(start + 2, end));
    if (rbsp.length < 13) continue;
    const pb = rbsp[1];
    const space = ["", "A", "B", "C"][(pb >> 6) & 3];
    const tier = (pb >> 5) & 1 ? "H" : "L";
    const profileIdc = pb & 0x1f;
    const compat = ((rbsp[2] << 24) | (rbsp[3] << 16) | (rbsp[4] << 8) | rbsp[5]) >>> 0;
    const compatRev = reverseBits32(compat);
    const constraints = rbsp.subarray(6, 12);
    const level = rbsp[12];
    let s = `hev1.${space}${profileIdc}.${compatRev.toString(16)}.${tier}${level}`;
    let last = -1;
    constraints.forEach((b, i) => {
      if (b !== 0) last = i;
    });
    for (let i = 0; i <= last; i++) s += `.${constraints[i].toString(16)}`;
    return s;
  }
  return null;
}

// ---------------------------------------------------------------------
// AV1: av01.<profile>.<level><tier>.<bitdepth> from the Sequence Header OBU
// ---------------------------------------------------------------------

class BitReader {
  constructor(data) {
    this.data = data;
    this.pos = 0;
  }
  f(n) {
    let v = 0;
    for (let i = 0; i < n; i++) {
      const byte = this.data[this.pos >> 3];
      if (byte === undefined) throw new RangeError("av1: sequence header truncated");
      v = v * 2 + ((byte >> (7 - (this.pos & 7))) & 1);
      this.pos++;
    }
    return v;
  }
  uvlc() {
    let zeros = 0;
    for (;;) {
      if (this.f(1) === 1) break;
      if (++zeros >= 32) return 2 ** 32 - 1;
    }
    return this.f(zeros) + 2 ** zeros - 1;
  }
}

export function av1CodecString(au) {
  let offset = 0;
  while (offset < au.length) {
    const header = au[offset];
    if (header & 0x80) return null;
    const type = (header >> 3) & 0x0f;
    const ext = (header >> 2) & 1;
    const hasSize = (header >> 1) & 1;
    if (!hasSize) return null;
    const leb = readLeb128(au, offset + 1 + ext);
    if (!leb) return null;
    const ps = offset + 1 + ext + leb.len;
    const pe = ps + leb.value;
    if (pe > au.length) return null;
    if (type === 1) {
      try {
        return av1SequenceHeaderToCodec(au.subarray(ps, pe));
      } catch {
        return null;
      }
    }
    offset = pe;
  }
  return null;
}

function av1SequenceHeaderToCodec(payload) {
  const r = new BitReader(payload);
  const seqProfile = r.f(3);
  r.f(1); // still_picture
  const reduced = r.f(1);
  let level0;
  let tier0 = 0;
  if (reduced) {
    level0 = r.f(5);
  } else {
    if (r.f(1)) {
      // timing_info_present_flag
      r.f(32);
      r.f(32);
      if (r.f(1)) r.uvlc();
      if (r.f(1)) return null; // decoder_model_info_present_flag: not handled
    }
    const initialDisplayDelayPresent = r.f(1);
    const opCnt = r.f(5);
    for (let i = 0; i <= opCnt; i++) {
      r.f(12);
      const lvl = r.f(5);
      const tier = lvl > 7 ? r.f(1) : 0;
      if (initialDisplayDelayPresent && r.f(1)) r.f(4);
      if (i === 0) {
        level0 = lvl;
        tier0 = tier;
      }
    }
  }
  const wBits = r.f(4);
  const hBits = r.f(4);
  r.f(wBits + 1);
  r.f(hBits + 1);
  const frameIdNumbersPresent = reduced ? 0 : r.f(1);
  if (frameIdNumbersPresent) {
    r.f(4);
    r.f(3);
  }
  r.f(1); // use_128x128_superblock
  r.f(1); // enable_filter_intra
  r.f(1); // enable_intra_edge_filter
  if (!reduced) {
    r.f(1); // interintra_compound
    r.f(1); // masked_compound
    r.f(1); // warped_motion
    r.f(1); // dual_filter
    const orderHint = r.f(1);
    if (orderHint) {
      r.f(1); // jnt_comp
      r.f(1); // ref_frame_mvs
    }
    const chooseSct = r.f(1);
    const forceSct = chooseSct ? 2 : r.f(1);
    if (forceSct > 0) {
      if (r.f(1) === 0) r.f(1); // seq_choose_integer_mv / seq_force_integer_mv
    }
    if (orderHint) r.f(3);
  }
  r.f(1); // enable_superres
  r.f(1); // enable_cdef
  r.f(1); // enable_restoration
  const highBitdepth = r.f(1);
  let bitDepth;
  if (seqProfile === 2 && highBitdepth) bitDepth = r.f(1) ? 12 : 10;
  else bitDepth = highBitdepth ? 10 : 8;
  const lvl = String(level0).padStart(2, "0");
  return `av01.${seqProfile}.${lvl}${tier0 === 1 ? "H" : "M"}.${String(bitDepth).padStart(2, "0")}`;
}

/**
 * Derive the WebCodecs codec string for a keyframe access unit.
 * @returns {{codec:"h264"|"h265"|"av1", string:string}|null}
 */
export function deriveCodecString(au) {
  const codec = sniffCodec(au);
  if (!codec) return null;
  const string = codec === CODECS.H264 ? avcCodecString(au) : codec === CODECS.H265 ? hevcCodecString(au) : av1CodecString(au);
  return string ? { codec, string } : null;
}
