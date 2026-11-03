#!/usr/bin/env python3
# usage: summary.py viewer.wav events.txt  -- for each RESUME: seconds until audio is audible and clean (3 s clean in a row), and bad windows after that
import sys, wave, struct, math
w = wave.open(sys.argv[1]); n = w.getnframes(); sr = w.getframerate()
d = struct.unpack("<%dh" % (n * w.getnchannels()), w.readframes(n))[::w.getnchannels()]
ev = [(float(l.split(None, 1)[0]), l.split(None, 1)[1].strip()) for l in open(sys.argv[2])]
win = sr // 10
cls = []
for i in range(0, n - win, win):
    s = d[i:i + win]
    rms = math.sqrt(sum(x * x for x in s) / len(s)) / 32768
    db = 20 * math.log10(rms) if rms > 0 else -99
    zc = sum(1 for a, b in zip(s, s[1:]) if a < 0 <= b) * 10.0
    pk = max(abs(x) for x in s) or 1
    step = max(abs(a - b) for a, b in zip(s, s[1:])) / pk
    if db < -50: cls.append("S")
    elif abs(zc - 440) <= 60 and step < 0.12: cls.append("C")
    else: cls.append("B")
resumes = [t for t, e in ev if e.startswith("RESUME")]
pauses = [t for t, e in ev if e.startswith("PAUSE")]
for k, t in enumerate(resumes):
    end = pauses[k + 1] if k + 1 < len(pauses) else n / sr
    i0 = int(t * 10)
    seg = "".join(cls[i0:int(end * 10)])
    first = seg.find("C" * 30)
    after = seg[first:] if first >= 0 else ""
    print("resume %d @%.1fs: clean(3s) after %s s; windows after that: %d bad / %d" % (k + 1, t, "%.1f" % (first / 10) if first >= 0 else "NEVER", after.count("B"), len(after)))
    steady = seg[30:]  # from 3 s after the resume to the next pause
    print("    steady state (3 s after the resume to the next pause): %d bad / %d windows of 0.1 s" % (steady.count("B"), len(steady)))
    print("   ", seg[:int(min(len(seg), 400))])
# the very first seconds: startup
print("startup 0-8s:", "".join(cls[:int(min(ev[0][0], 8) * 10)]))
