#!/usr/bin/env python3
"""portal-probe.py: how fast does a real xdg-desktop-portal screencast deliver
buffers, and how much of that survives a given downstream chain?

Opens its OWN ScreenCast portal session, then runs
    pipewiresrc fd=<portal fd> path=<node> do-timestamp=true ! <TAIL>
and prints, once per second, buffers/s at pipewiresrc's source pad plus the
memory type of the buffers (dma-buf vs system memory) and the negotiated caps.

Why: the egui client's capture pipeline is `pipewiresrc ! videoconvert ! ...`
and measured only ~55 fps from a 280 Hz output, while wlr-screencopy itself
delivers 276 fps (wlr-capture-rate.sh). Running the same source into different
tails separates "portal/xdpw produces N fps" from "videoconvert / the copy out of
GPU memory limits to N fps".

Run it against the HEADLESS rig with hs-probe.sh: the rig's portal has no picker, the session
is granted at once. (On the user's real desktop the picker would appear; agents must not do
that, see superdoc/claude-instructions/testing-and-verification.md.)

Usage: portal-probe.py [--secs N] [--tail "video/x-raw ! fakesink sync=false"] [--keepalive MS]
  e.g. --tail "video/x-raw ! fakesink sync=false"          (no copy: source rate)
       --tail "videoconvert ! fakesink sync=false"         (what the app does first)
       --tail "videoconvert ! video/x-raw,format=NV12 ! fakesink sync=false"
  Do NOT use a bare "fakesink": its caps are ANY, so pipewiresrc's PipeWire node gets media.class
  Stream/Input/Unknown and WirePlumber 0.5 refuses to link it to the Video/Source ("stream error:
  target not found"). Any tail that starts with a video caps filter or a video element is fine.
  Also pass --src "min-buffers=8": with the portal's default 2-buffer pool a plain fakesink tail
  gets one frame and then "out of buffers" (see features/stream-control.md).
"""
import argparse
import os
import sys
import time

import gi

gi.require_version("Gst", "1.0")
from gi.repository import Gio, GLib, Gst  # noqa: E402

Gst.init(None)
BUS = "org.freedesktop.portal.Desktop"
PATH = "/org/freedesktop/portal/desktop"
IFACE = "org.freedesktop.portal.ScreenCast"


class Portal:
    def __init__(self):
        self.conn = Gio.bus_get_sync(Gio.BusType.SESSION, None)
        sender = self.conn.get_unique_name()[1:].replace(".", "_")
        self.req_prefix = f"/org/freedesktop/portal/desktop/request/{sender}/"
        self.n = 0
        self.loop = GLib.MainLoop()

    def _token(self):
        self.n += 1
        return f"sssprobe{os.getpid()}_{self.n}"

    def request(self, method, args_builder):
        """Call a portal method that answers through a Request/Response signal."""
        token = self._token()
        result = {}

        def on_response(_c, _s, _p, _i, _sig, params):
            code, res = params.unpack()
            result["code"], result["res"] = code, res
            self.loop.quit()

        sub = self.conn.signal_subscribe(
            BUS, "org.freedesktop.portal.Request", "Response", self.req_prefix + token,
            None, Gio.DBusSignalFlags.NONE, on_response)
        self.conn.call_sync(BUS, PATH, IFACE, method, args_builder(token), None,
                            Gio.DBusCallFlags.NONE, -1, None)
        GLib.timeout_add_seconds(120, self.loop.quit)
        self.loop.run()
        self.conn.signal_unsubscribe(sub)
        if result.get("code") != 0:
            sys.exit(f"portal {method} failed/cancelled: {result}")
        return result["res"]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--secs", type=int, default=10)
    ap.add_argument("--tail", default="fakesink sync=false")
    ap.add_argument("--keepalive", type=int, default=0)
    ap.add_argument("--watch", default="", help="comma-separated element names in --tail whose src-pad buffers/s are also printed (name them with name=X)")
    ap.add_argument("--src", default="", help="extra pipewiresrc properties, e.g. 'min-buffers=8 max-buffers=16'")
    ap.add_argument("--cursor", type=int, default=0, help="portal cursor_mode: 0 none/default, 1 hidden, 2 embedded (what the app uses), 4 metadata")
    a = ap.parse_args()

    p = Portal()
    st = p._token()
    res = p.request("CreateSession", lambda tok: GLib.Variant("(a{sv})", ({
        "handle_token": GLib.Variant("s", tok), "session_handle_token": GLib.Variant("s", st)},)))
    session = res["session_handle"]
    p.request("SelectSources", lambda tok: GLib.Variant("(oa{sv})", (session, {
        "handle_token": GLib.Variant("s", tok), "types": GLib.Variant("u", 1),
        "multiple": GLib.Variant("b", False), **({"cursor_mode": GLib.Variant("u", a.cursor)} if a.cursor else {})})))
    print("portal: picker answered, starting ...", flush=True)
    res = p.request("Start", lambda tok: GLib.Variant("(osa{sv})", (session, "", {
        "handle_token": GLib.Variant("s", tok)})))
    node = res["streams"][0][0]
    props = res["streams"][0][1]
    print(f"portal: node {node} props {props}", flush=True)
    ret, fdl = p.conn.call_with_unix_fd_list_sync(
        BUS, PATH, IFACE, "OpenPipeWireRemote",
        GLib.Variant("(oa{sv})", (session, {})), GLib.VariantType("(h)"),
        Gio.DBusCallFlags.NONE, -1, None, None)
    fd = fdl.get(ret.unpack()[0])

    ka = f" keepalive-time={a.keepalive}" if a.keepalive else ""
    desc = f"pipewiresrc name=src fd={fd} path={node} do-timestamp=true{ka} {a.src} ! {a.tail}"
    print("pipeline:", desc, flush=True)
    pipe = Gst.parse_launch(desc)
    src = pipe.get_by_name("src")
    count = [0]
    memtype = {}

    def probe(_pad, info):
        count[0] += 1
        buf = info.get_buffer()
        if buf is not None and buf.n_memory():
            m = buf.peek_memory(0)
            t = m.allocator.mem_type if m.allocator else "?"
            memtype[t] = memtype.get(t, 0) + 1
        return Gst.PadProbeReturn.OK

    src.get_static_pad("src").add_probe(Gst.PadProbeType.BUFFER, probe)

    def event_probe(_pad, info):
        ev = info.get_event()
        if ev is not None and ev.type == Gst.EventType.TAG:
            print(f"  TAG event at pipewiresrc: {ev.parse_tag().to_string()}", flush=True)
        return Gst.PadProbeReturn.OK

    src.get_static_pad("src").add_probe(Gst.PadProbeType.EVENT_DOWNSTREAM, event_probe)
    watch = {}
    for nm in [w for w in a.watch.split(",") if w]:
        el = pipe.get_by_name(nm)
        if el is None:
            sys.exit(f"--watch: no element named {nm}")
        watch[nm] = [0, 0]

        def mk(nm):
            def wprobe(_pad, _info):
                watch[nm][0] += 1
                return Gst.PadProbeReturn.OK
            return wprobe
        el.get_static_pad("src").add_probe(Gst.PadProbeType.BUFFER, mk(nm))
    pipe.set_state(Gst.State.PLAYING)
    t0 = time.time()
    last = 0
    rates = []
    while time.time() - t0 < a.secs:
        time.sleep(1.0)
        c = count[0]
        rates.append(c - last)
        extra = ""
        for nm, v in watch.items():
            extra += f"  {nm}={v[0] - v[1]}"
            v[1] = v[0]
        print(f"  t={time.time()-t0:4.1f}s  pipewiresrc buffers/s = {c - last}{extra}", flush=True)
        last = c
    caps = src.get_static_pad("src").get_current_caps()
    print("caps:", caps.to_string() if caps else None)
    print("buffer memory types:", memtype)
    if rates:
        print(f"RESULT avg {sum(rates)/len(rates):.1f} fps, min {min(rates)}, max {max(rates)}")
    pipe.set_state(Gst.State.NULL)
    # closing the session ends the screencast
    p.conn.call_sync(BUS, session, "org.freedesktop.portal.Session", "Close", None, None,
                     Gio.DBusCallFlags.NONE, -1, None)


if __name__ == "__main__":
    main()
