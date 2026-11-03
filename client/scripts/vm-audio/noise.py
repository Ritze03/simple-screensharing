#!/usr/bin/env python3
# usage: noise.py FILE.wav [events.txt]   (s16 or f32 mono/stereo WAV, as pw-record writes it; VFMT=f32 in soak.sh)
# Per second: RMS dBFS and peak (in LSB of 16 bit) -- the noise floor of a stretch where nothing should play (#129).
# With events.txt, a summary of RMS between each "PAUSE k" and the following "RESUME k" (first 1.5 s skipped).
import sys, wave, struct, math
fn = sys.argv[1]
raw = open(fn, "rb").read()
# minimal RIFF parse (pw-record's float WAV has format tag 3, which the wave module refuses)
pos = 12; fmt = None; data = b""
while pos + 8 <= len(raw):
    cid, sz = raw[pos:pos+4], struct.unpack("<I", raw[pos+4:pos+8])[0]
    body = raw[pos+8:pos+8+sz]
    if cid == b"fmt ": fmt = struct.unpack("<HHIIHH", body[:16])
    elif cid == b"data": data = body if sz < 0xfffffff0 else raw[pos+8:]
    pos += 8 + sz + (sz & 1)
tag, ch, sr, _, _, bits = fmt
n = len(data) // (bits // 8)
x = struct.unpack("<%d%s" % (n, "f" if tag == 3 else "h"), data[:n * (bits // 8)])
if tag != 3: x = [v / 32768 for v in x]
x = x[::ch]
def db(v): return 20 * math.log10(v) if v > 0 else -200
ev = []
if len(sys.argv) > 2:
    for l in open(sys.argv[2]):
        t, e = l.split(None, 1); ev.append((float(t), e.strip()))
print("rate", sr, "ch", ch, "bits", bits, "tag", tag, "dur %.1fs" % (len(x) / sr))
def stat(a):
    r = math.sqrt(sum(v * v for v in a) / len(a)); p = max(abs(v) for v in a)
    return db(r), p * 32768
for i in range(0, len(x) - sr, sr):
    r, p = stat(x[i:i+sr]); print("%4ds %7.1f dBFS rms  peak %9.3f LSB" % (i // sr, r, p))
p0 = None
for t, e in ev:
    if e.startswith("PAUSE"): p0 = t
    elif e.startswith("RESUME") and p0 is not None:
        a = x[int((p0 + 1.5) * sr):int(t * sr)]
        if a: r, p = stat(a); print("%s..%s: rms %.1f dBFS peak %.3f LSB" % (p0, t, r, p))
        p0 = None
