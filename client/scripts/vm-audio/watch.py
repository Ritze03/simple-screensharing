#!/usr/bin/env python3
# polls pw-dump every 0.3 s and prints changes to nodes (state) and links, with a time offset
import json, subprocess, sys, time
t0 = time.time(); prev = {}
while True:
    try:
        d = json.loads(subprocess.run(["pw-dump"], capture_output=True, text=True).stdout)
    except Exception:
        time.sleep(0.3); continue
    names = {}
    cur = {}
    for o in d:
        if o["type"] == "PipeWire:Interface:Node":
            p = o["info"]["props"]; names[o["id"]] = p.get("node.name", "?")
            cur["node %d %s" % (o["id"], p.get("node.name"))] = o["info"]["state"] + " drv=%s" % p.get("node.driver-id")
    for o in d:
        if o["type"] == "PipeWire:Interface:Link":
            p = o["info"]["props"]
            cur["link %d %s:%s -> %s:%s" % (o["id"], names.get(p.get("link.output.node")), p.get("link.output.port"), names.get(p.get("link.input.node")), p.get("link.input.port"))] = o["info"]["state"]
        if o["type"] == "PipeWire:Interface:Port":
            p = o["info"]["props"]
            if p.get("port.direction") == "out" and "Firefox" in names.get(p.get("node.id"), ""):
                cur["port %d %s" % (o["id"], p.get("port.name"))] = ""
    for k in sorted(set(cur) | set(prev)):
        if k.startswith("node") and ("Midi" in k or "Driver" in k): continue
        if prev.get(k) != cur.get(k):
            print("%6.1f %-70s %s -> %s" % (time.time() - t0, k, prev.get(k), cur.get(k)), flush=True)
    prev = cur
    time.sleep(0.3)
