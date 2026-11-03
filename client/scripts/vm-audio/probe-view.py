#!/usr/bin/env python3
# Runs INSIDE a test VM (task #128): the native viewer's playback tail (opusdec ... autoaudiosink, sync=true)
# fed by a live 440 Hz source, so the graph can be changed under a running *playback* stream.
# usage: probe-view.py SECONDS MODE   (plays into the default sink; PULSE_SINK selects another)
#   MODE default : what decode.rs builds (autoaudiosink = pulsesink here, its clock is the pipeline clock)
#        sysclock: the same with the pipeline on the monotonic system clock (the sink slaves to it)
# Prints once a second the pipeline clock against CLOCK_MONOTONIC and the sink's dropped/late counters.
import sys, time
import gi
gi.require_version("Gst", "1.0")
from gi.repository import Gst, GLib

secs = float(sys.argv[1]); mode = sys.argv[2] if len(sys.argv) > 2 else "default"
Gst.init(None)
p = Gst.parse_launch(
    "audiotestsrc is-live=true wave=sine freq=440 volume=0.5 samplesperbuffer=960 ! audio/x-raw,rate=48000,channels=2 "
    "! opusenc ! opusdec plc=true ! audioconvert ! audioresample ! audio/x-raw,format=F32LE,layout=interleaved "
    "! volume ! audioconvert ! autoaudiosink name=out sync=true")
if mode == "sysclock":
    p.use_clock(Gst.SystemClock.obtain())
p.set_state(Gst.State.PLAYING)
t0 = time.monotonic(); off0 = None
bus = p.get_bus()
def tick():
    global off0
    clk = p.get_clock()
    c = clk.get_time() if clk else 0
    mono = int(time.monotonic() * 1e9)
    if off0 is None: off0 = c - mono
    msgs = []
    m = bus.pop()
    while m:
        if m.type in (Gst.MessageType.CLOCK_LOST, Gst.MessageType.WARNING, Gst.MessageType.ERROR, Gst.MessageType.QOS):
            msgs.append(m.type.first_value_nick)
        m = bus.pop()
    sink = None
    out = p.get_by_name("out")
    it = out.iterate_recurse()
    while True:
        res, e = it.next()
        if res != Gst.IteratorResult.OK:
            break
        if e.get_factory() and "sink" in e.get_factory().get_name():
            sink = e
    stats = sink.get_property("stats") if sink and sink.find_property("stats") else None
    st = "" if stats is None else "rendered=%s dropped=%s" % (stats.get_value("rendered"), stats.get_value("dropped"))
    print("t=%5.1f clock=%s clk-mono=%+8.1fms %s %s %s" % (time.monotonic() - t0, clk.get_name() if clk else None,
          ((c - mono) - off0) / 1e6, sink.get_factory().get_name() if sink else "?", st, " ".join(sorted(set(msgs)))), flush=True)
    if time.monotonic() - t0 > secs:
        loop.quit(); return False
    return True
loop = GLib.MainLoop()
GLib.timeout_add(1000, tick)
loop.run()
p.set_state(Gst.State.NULL)
