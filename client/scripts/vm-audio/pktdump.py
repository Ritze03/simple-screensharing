#!/usr/bin/env python3
# Reads an SSS_AUDIO_DUMP packet file (client/src/audio_dump.rs: [u64 arrival_us][u64 pts_us][u32 len][payload])
# and (1) prints per second: packets, PTS step min/max, PTS advance vs wall-clock advance, (2) decodes the
# packets back to back (no timing, like a perfect receiver) into OUT.wav for analyze.py.
# usage: pktdump.py FILE.pkt [OUT.wav]      (needs python3-gobject + GStreamer opusdec; no audio device used)
import struct, sys
recs = []
with open(sys.argv[1], "rb") as f:
    while True:
        h = f.read(20)
        if len(h) < 20: break
        t, pts, n = struct.unpack("<QQI", h)
        recs.append((t, pts, f.read(n)))
print("packets", len(recs), "empty", sum(1 for r in recs if not r[2]))
if not recs: sys.exit()
t0, p0 = recs[0][0], recs[0][1]
sec = 0; cnt = 0; smin = 10**12; smax = 0; back = 0; prev = None
for t, pts, d in recs:
    s = (t - t0) // 1_000_000
    while s > sec:
        print("t=%3ds pkts=%3d pts-step=%s..%s ms backwards=%d pts-wall=%+.1f ms" % (
            sec, cnt, "%.1f" % (smin / 1000) if cnt > 1 else "-", "%.1f" % (smax / 1000), back,
            ((last_pts - p0) - (last_t - t0)) / 1000))
        sec += 1; cnt = 0; smin = 10**12; smax = 0; back = 0
    if prev is not None:
        st = pts - prev
        if st < 0: back += 1
        else: smin = min(smin, st); smax = max(smax, st)
    prev = pts; cnt += 1; last_pts = pts; last_t = t
if len(sys.argv) > 2:
    import gi
    gi.require_version("Gst", "1.0"); gi.require_version("GstApp", "1.0")
    from gi.repository import Gst, GstApp
    Gst.init(None)
    pl = Gst.parse_launch("appsrc name=src format=time caps=audio/x-opus,channel-mapping-family=0 ! opusdec "
                          "! audioconvert ! audio/x-raw,format=S16LE,channels=1,rate=48000 ! wavenc ! filesink location=%s" % sys.argv[2])
    src = pl.get_by_name("src"); pl.set_state(Gst.State.PLAYING)
    for i, (t, pts, d) in enumerate(recs):
        if not d:
            continue  # an empty packet (never produced by opusenc so far; counted above)
        b = Gst.Buffer.new_wrapped(d); b.pts = i * 20_000_000; b.duration = 20_000_000
        src.emit("push-buffer", b)
    src.emit("end-of-stream")
    pl.get_bus().timed_pop_filtered(Gst.CLOCK_TIME_NONE, Gst.MessageType.EOS | Gst.MessageType.ERROR)
    pl.set_state(Gst.State.NULL)
    print("decoded to", sys.argv[2])
