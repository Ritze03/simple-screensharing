#!/usr/bin/env python3
# usage: analyze.py viewer.wav [events.txt]  -- per-0.5s window: rms dBFS, freq (zero crossings), glitch score
import sys, wave, struct, math
if sys.argv[1].endswith(".raw"):
    raw = open(sys.argv[1], "rb").read(); raw = raw[:len(raw) // 4 * 4]
    d = struct.unpack("<%dh" % (len(raw) // 2), raw)[::2]; n = len(d); sr = 48000
else:
    w = wave.open(sys.argv[1]); n = w.getnframes(); sr = w.getframerate(); ch = w.getnchannels()
    d = struct.unpack("<%dh" % (n * ch), w.readframes(n))
    if ch > 1: d = d[::ch]
ev = []
if len(sys.argv) > 2:
    for l in open(sys.argv[2]):
        t, e = l.split(None, 1); ev.append((float(t), e.strip()))
win = sr // 2
print("rate", sr, "dur %.1fs" % (n / sr))
for i in range(0, n - win, win):
    s = d[i:i + win]
    rms = math.sqrt(sum(x * x for x in s) / len(s)) / 32768
    db = 20 * math.log10(rms) if rms > 0 else -99
    zc = sum(1 for a, b in zip(s, s[1:]) if a < 0 <= b)
    f = zc * 2.0
    # max step relative to the peak: for a clean sine of freq 440 step/peak <= 0.058
    pk = max(abs(x) for x in s) or 1
    step = max(abs(a - b) for a, b in zip(s, s[1:])) / pk
    t = i / sr
    mark = [e for (te, e) in ev if t <= te < t + 0.5]
    flag = ""
    if db > -60 and (abs(f - 440) > 15 or step > 0.12): flag = "  <== BAD"
    print("%6.1f  %6.1f dB  %6.0f Hz  step/pk %.3f%s  %s" % (t, db, f, step, flag, " ".join(mark)))
