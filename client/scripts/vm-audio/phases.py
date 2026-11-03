#!/usr/bin/env python3
# usage: phases.py viewer.wav events.txt   (task #128)
# Bad 0.5 s windows per phase between events (same classification as analyze.py: a window is clean when its
# zero-crossing frequency is within 15 Hz of 440 and its largest sample step is under 0.12 of the peak; a
# clean sine is 440 Hz / 0.058; silent windows are counted apart). The window holding the event is left out.
import sys, wave, struct, math
w = wave.open(sys.argv[1]); n = w.getnframes(); sr = w.getframerate()
d = struct.unpack("<%dh" % (n * w.getnchannels()), w.readframes(n))[::w.getnchannels()]
ev = [(float(l.split(None, 1)[0]), l.split(None, 1)[1].strip()) for l in open(sys.argv[2])]
win = sr // 2
cls = []
for i in range(0, n - win, win):
    s = d[i:i + win]
    rms = math.sqrt(sum(x * x for x in s) / len(s)) / 32768
    db = 20 * math.log10(rms) if rms > 0 else -99
    zc = sum(1 for a, b in zip(s, s[1:]) if a < 0 <= b) * 2.0
    pk = max(abs(x) for x in s) or 1
    step = max(abs(a - b) for a, b in zip(s, s[1:])) / pk
    cls.append("S" if db < -50 else "C" if abs(zc - 440) <= 15 and step < 0.12 else "B")
bounds = [(0.0, "start")] + ev + [(len(cls) / 2, "end")]
out = []
for (t0, name), (t1, _) in zip(bounds, bounds[1:]):
    seg = cls[int(t0 * 2) + (1 if t0 > 0 else 0):int(t1 * 2)]
    out.append("%s: %d bad/%d (%d silent)" % (name, seg.count("B"), len(seg), seg.count("S")))
print(" | ".join(out))
