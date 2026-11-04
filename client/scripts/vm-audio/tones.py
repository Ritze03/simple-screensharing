#!/usr/bin/env python3
# HOST side, no audio is played: how loud each of the given tones is, per window of a recording (Goertzel, pure python).
# usage: tones.py FILE.wav [--win SECONDS] [--events "T:label,T:label"] FREQ [FREQ ...]
#   prints, per window, the amplitude of every FREQ in dBFS (-99 = nothing) and the labels of the events that fall in it.
#   Used by task I7 to show that a recording of "what a viewer hears" has 440 Hz and not 880 Hz (an excluded app).
import math, struct, sys, wave

args = sys.argv[1:]
path = args.pop(0)
win_s = 1.0
events = []
while args and args[0].startswith("--"):
    opt = args.pop(0)
    if opt == "--win":
        win_s = float(args.pop(0))
    elif opt == "--events":
        events = [(float(t), l) for t, l in (e.split(":", 1) for e in args.pop(0).split(",") if ":" in e)]
freqs = [float(a) for a in args]
w = wave.open(path)
sr, ch, n = w.getframerate(), w.getnchannels(), w.getnframes()
fmt = {2: "h", 4: "i"}[w.getsampwidth()] if w.getsampwidth() in (2, 4) else None
raw = w.readframes(n)
if fmt:
    d = struct.unpack("<%d%s" % (n * ch, fmt), raw)[::ch]
    scale = float(1 << (8 * w.getsampwidth() - 1))
else:  # float32 wav is not readable by the wave module; the VM recordings are s16
    sys.exit("unsupported sample width")
win = int(sr * win_s)


def amp(s, f):
    k = 2.0 * math.cos(2.0 * math.pi * f / sr)
    a = b = 0.0
    for x in s:
        a, b = x + k * a - b, a
    power = a * a + b * b - k * a * b
    return 2.0 * math.sqrt(max(power, 0.0)) / len(s) / scale


print("rate", sr, "dur %.1fs" % (n / sr), "freqs", freqs)
for i in range(0, n - win + 1, win):
    s = d[i:i + win]
    rms = math.sqrt(sum(x * x for x in s) / len(s)) / scale
    t = i / sr
    cells = []
    for f in freqs:
        a = amp(s, f)
        cells.append("%4.0f Hz %6.1f dB" % (f, 20 * math.log10(a) if a > 1e-5 else -99.0))
    mark = " ".join(l for te, l in events if t <= te < t + win_s)
    print("%6.1f  rms %6.1f dB  %s  %s" % (t, 20 * math.log10(rms) if rms > 0 else -99.0, "  ".join(cells), mark))
