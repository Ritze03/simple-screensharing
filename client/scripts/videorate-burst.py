import time, sys, threading
import gi
gi.require_version("Gst", "1.0")
from gi.repository import Gst, GLib
Gst.init(None)
extra = sys.argv[1] if len(sys.argv) > 1 else ""
desc = ("videotestsrc is-live=true ! video/x-raw,width=64,height=64,framerate=60/1 ! queue leaky=downstream max-size-buffers=2 "
        f"! videorate drop-only=true skip-to-first=true {extra} ! video/x-raw,framerate=30/1 ! identity name=blk ! fakesink sync=false")
print(desc)
p = Gst.parse_launch(desc)
blk = p.get_by_name("blk")
cnt = [0]
gate = threading.Event(); gate.set()
def probe(pad, info):
    gate.wait()
    cnt[0] += 1
    return Gst.PadProbeReturn.OK
blk.get_static_pad("src").add_probe(Gst.PadProbeType.BUFFER, probe)
p.set_state(Gst.State.PLAYING)
def sample(label, secs):
    t0 = time.time(); c0 = cnt[0]
    time.sleep(secs)
    print(f"{label}: {(cnt[0]-c0)/(time.time()-t0):.1f} fps")
sample("steady", 2)
gate.clear(); time.sleep(1.5); gate.set()
for i in range(8):
    sample(f"after-block {i*0.5:.1f}s", 0.5)
p.set_state(Gst.State.NULL)
