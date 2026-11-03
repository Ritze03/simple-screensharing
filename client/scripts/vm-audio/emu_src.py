#!/usr/bin/env python3
# Runs INSIDE a test VM: a playback stream that behaves like the host's Floorp/YouTube (#125): on pause its
# node goes running -> idle and on resume idle -> running, with the SAME node and the SAME ports (Firefox in
# the VMs re-creates its ports instead, which the poll already handles). A GStreamer pulsesink (a pulse client of
# pipewire-pulse, like Firefox's cubeb) that is set PAUSED / PLAYING corks the stream: node idle, ports kept.
# usage: emu_src.py [NAME] [FREQ]   SIGUSR1 = pause, SIGUSR2 = play, SIGTERM = quit. Runs as app NAME (default Floorp).
import signal, sys
import gi
gi.require_version("Gst", "1.0")
from gi.repository import Gst, GLib, GLibUnix

name = sys.argv[1] if len(sys.argv) > 1 else "Floorp"
freq = sys.argv[2] if len(sys.argv) > 2 else "440"
Gst.init(None)
p = Gst.parse_launch(
    f'audiotestsrc is-live=true wave=sine freq={freq} volume=0.5 ! audio/x-raw,format=F32LE,rate=48000,channels=2 ! '
    f'pulsesink client-name={name} '
    f'stream-properties="props,application.name={name},media.role=Music"')
loop = GLib.MainLoop()
def on_msg(bus, m):
    if m.type == Gst.MessageType.ERROR:
        print("ERROR", m.parse_error(), flush=True)
    elif m.type == Gst.MessageType.STATE_CHANGED and m.src == p:
        print("state", m.parse_state_changed()[:2], flush=True)
bus = p.get_bus(); bus.add_signal_watch(); bus.connect("message", on_msg)
GLibUnix.signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR1, lambda: (p.set_state(Gst.State.PAUSED), True)[1])
GLibUnix.signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR2, lambda: (p.set_state(Gst.State.PLAYING), True)[1])
GLibUnix.signal_add(GLib.PRIORITY_DEFAULT, signal.SIGTERM, lambda: (loop.quit(), False)[1])
p.set_state(Gst.State.PLAYING)
loop.run()
p.set_state(Gst.State.NULL)
