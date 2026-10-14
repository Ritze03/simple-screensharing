// Minimal postcard (https://postcard.jamesmunns.com/wire-format) writer/reader:
// exactly the subset the relay protocol uses (u8, varint integers, strings,
// fixed byte arrays, sequences, enum discriminants).
//
// u64 values (ConnId) are BigInt; every other integer is a Number.

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8", { fatal: true });

export class PostcardWriter {
  constructor() {
    this.buf = new Uint8Array(64);
    this.len = 0;
  }
  #reserve(n) {
    if (this.len + n <= this.buf.length) return;
    let cap = this.buf.length * 2;
    while (cap < this.len + n) cap *= 2;
    const next = new Uint8Array(cap);
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }
  u8(v) {
    this.#reserve(1);
    this.buf[this.len++] = v & 0xff;
    return this;
  }
  /** Unsigned LEB128 varint; accepts Number or BigInt. */
  varint(v) {
    let n = BigInt(v);
    if (n < 0n) throw new RangeError("postcard varint must be unsigned");
    do {
      let b = Number(n & 0x7fn);
      n >>= 7n;
      if (n) b |= 0x80;
      this.u8(b);
    } while (n);
    return this;
  }
  bytesFixed(bytes) {
    this.#reserve(bytes.length);
    this.buf.set(bytes, this.len);
    this.len += bytes.length;
    return this;
  }
  string(s) {
    const b = textEncoder.encode(s);
    this.varint(b.length);
    return this.bytesFixed(b);
  }
  finish() {
    return this.buf.slice(0, this.len);
  }
}

export class PostcardReader {
  constructor(bytes) {
    this.b = bytes;
    this.i = 0;
  }
  u8() {
    if (this.i >= this.b.length) throw new Error("postcard: unexpected end of input");
    return this.b[this.i++];
  }
  /** Returns a BigInt (callers narrow with Number() where the schema says u16/u32). */
  varintBig() {
    let n = 0n;
    let shift = 0n;
    for (;;) {
      const byte = this.u8();
      n |= BigInt(byte & 0x7f) << shift;
      if (!(byte & 0x80)) return n;
      shift += 7n;
      if (shift > 70n) throw new Error("postcard: varint too long");
    }
  }
  varint() {
    return Number(this.varintBig());
  }
  bytesFixed(n) {
    if (this.i + n > this.b.length) throw new Error("postcard: unexpected end of input");
    const out = this.b.slice(this.i, this.i + n);
    this.i += n;
    return out;
  }
  string() {
    const n = this.varint();
    return textDecoder.decode(this.bytesFixed(n));
  }
}
