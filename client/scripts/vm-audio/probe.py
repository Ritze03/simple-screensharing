#!/usr/bin/env python3
# Runs INSIDE a test VM (task #128): the share's capture chain without the app, instrumented.
# Creates the share sink exactly like audio.rs (pw-cli create-node, same properties), links every output
# stream of APP into it, then runs the sender pipeline (pipewiresrc on the sink monitor -> ... -> opusenc ->
# appsink) and prints once a second:
#   samples/s that left pipewiresrc, Opus packets/s, the largest PTS step and how far the buffer PTS lag the
#   pipeline running time ("lag", grows when basesrc is held back by the pipeline clock), the pipeline clock
#   versus CLOCK_MONOTONIC ("clk-mono", jumps when the clock jumps), and which clock the pipeline uses.
# usage: probe.py APP SECONDS [MODE]   MODE: pwclock (what the app did up to #128) | sysclock (the #128 fix)
import os, signal, subprocess, sys, time, json
import gi
gi.require_version("Gst", "1.0")
from gi.repository import Gst, GLib

app = sys.argv[1]; secs = float(sys.argv[2]); mode = sys.argv[3] if len(sys.argv) > 3 else "pwclock"
name = "ssshare-sink-probe"
Gst.init(None)
cli = subprocess.Popen(["pw-cli"], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, text=True)
cli.stdin.write("create-node adapter { factory.name=support.null-audio-sink node.name=%s "
                "node.description=probe media.class=Audio/Sink audio.rate=48000 audio.channels=2 "
                "audio.position=[ FL FR ] node.virtual=true node.driver=true node.always-process=true "
                "node.pause-on-idle=false session.suspend-timeout-seconds=0 priority.session=0 object.linger=false }\n" % name)
cli.stdin.flush()

def dump():
    return json.loads(subprocess.run(["pw-dump"], capture_output=True, text=True).stdout)

serial = None
for _ in range(50):
    for o in dump():
        p = (o.get("info") or {}).get("props") or {}
        if o["type"].endswith("Node") and p.get("node.name") == name:
            serial = p.get("object.serial")
    if serial: break
    time.sleep(0.1)
assert serial, "sink did not appear"

def link_app():
    outs = subprocess.run(["pw-link", "-o"], capture_output=True, text=True).stdout.split()
    for ch in ("FL", "FR"):
        for port in [p for p in outs if p.startswith(app + ":") and p.endswith("_" + ch)]:
            subprocess.run(["pw-link", port, "%s:playback_%s" % (name, ch)], capture_output=True)
link_app()

src = ('pipewiresrc name=src target-object=%s client-name=%s-cap stream-properties="props,stream.capture.sink=true" '
       'do-timestamp=true' % (serial, name))
p = Gst.parse_launch(src + " ! audio/x-raw,channels=2 ! audioconvert ! audioresample ! audio/x-raw,format=F32LE "
                     "! identity name=probe ! audioconvert ! opusenc name=enc ! appsink name=sink sync=false max-buffers=32 drop=true emit-signals=true")
if mode == "sysclock":
    p.use_clock(Gst.SystemClock.obtain())
st = {"samples": 0, "pkts": 0, "maxstep": 0, "lastpts": None, "lastend": None, "disc": 0, "lag": -10**15, "lagmin": 10**15, "gap": 0, "overlap": 0,
      "opts": None, "ostep_min": 10**12, "ostep_max": 0}
def on_buf(pad, info):
    b = info.get_buffer()
    caps = pad.get_current_caps()
    n = b.get_size() // 8  # F32LE stereo
    st["samples"] += n
    if b.pts != Gst.CLOCK_TIME_NONE:
        if st["lastend"] is not None:
            d = b.pts - st["lastend"]
            if d > 2_000_000: st["gap"] += 1
            if d < -2_000_000: st["overlap"] += 1
        if st["lastpts"] is not None: st["maxstep"] = max(st["maxstep"], b.pts - st["lastpts"])
        st["lastpts"] = b.pts
        st["lastend"] = b.pts + n * 1_000_000_000 // 48000
        rt = p.get_clock().get_time() - p.get_base_time() if p.get_clock() else 0
        st["lag"] = max(st["lag"], rt - b.pts); st["lagmin"] = min(st["lagmin"], rt - b.pts)
    if b.has_flags(Gst.BufferFlags.DISCONT): st["disc"] += 1
    return Gst.PadProbeReturn.OK
p.get_by_name("probe").get_static_pad("src").add_probe(Gst.PadProbeType.BUFFER, on_buf)
def on_sample(sink):
    s = sink.emit("pull-sample"); b = s.get_buffer(); st["pkts"] += 1
    if st["opts"] is not None:
        d = b.pts - st["opts"]; st["ostep_min"] = min(st["ostep_min"], d); st["ostep_max"] = max(st["ostep_max"], d)
    st["opts"] = b.pts
    return Gst.FlowReturn.OK
p.get_by_name("sink").connect("new-sample", on_sample)
bus = p.get_bus()
p.set_state(Gst.State.PLAYING)
t0 = time.monotonic(); last = t0; clk0 = None
loop = GLib.MainLoop()
def tick():
    global last, clk0
    now = time.monotonic()
    clk = p.get_clock()
    c = clk.get_time() if clk else 0
    mono = int(now * 1e9)
    if clk0 is None: clk0 = c - mono
    rel = (c - mono) - clk0
    m = bus.pop()
    msgs = []
    while m:
        if m.type in (Gst.MessageType.CLOCK_LOST, Gst.MessageType.ERROR, Gst.MessageType.WARNING, Gst.MessageType.LATENCY, Gst.MessageType.NEW_CLOCK):
            msgs.append(m.type.first_value_nick)
        m = bus.pop()
    print("t=%5.1f samples/s=%6d pkts/s=%3d maxstep=%5.1fms gaps=%d overl=%d disc=%d lag=%7.1f..%7.1fms clk-mono=%+9.1fms opusstep=%s..%sms clock=%s %s" % (
        now - t0, st["samples"] / (now - last), st["pkts"] / (now - last), st["maxstep"] / 1e6, st["gap"], st["overlap"], st["disc"], st["lagmin"] / 1e6, st["lag"] / 1e6,
        rel / 1e6, st["ostep_min"] // 1_000_000 if st["ostep_min"] < 10**12 else "-", st["ostep_max"] // 1_000_000,
        clk.get_name() if clk else None, " ".join(msgs)), flush=True)
    for k in ("samples", "pkts", "maxstep", "disc", "gap", "overlap", "ostep_max"): st[k] = 0
    st["lag"] = -10**15; st["lagmin"] = 10**15
    st["ostep_min"] = 10**12
    last = now
    link_app()
    if now - t0 > secs: loop.quit(); return False
    return True
GLib.timeout_add(1000, tick)
signal.signal(signal.SIGTERM, lambda *a: loop.quit())
loop.run()
p.set_state(Gst.State.NULL)
cli.stdin.close(); cli.wait()
