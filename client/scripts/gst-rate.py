#!/usr/bin/env python3
"""gst-rate.py: buffers per second at every pad of the client's capture/encode
pipeline, from a GStreamer `stats` tracer log.

Why this exists: the egui UI does not show the sender's capture or encoder rate
(`CaptureHandle::encoder_fps` has no UI), and to name the limiting stage of a
240 fps share you need the rate at each stage: pipewiresrc (capture), the
converters, the encoder's source pad, the appsink handed to the QUIC sender.

Record a log by starting the SHARING client with
    GST_TRACERS=stats GST_DEBUG=GST_TRACER:7 GST_DEBUG_NO_COLOR=1 GST_DEBUG_FILE=<log>
(launch-test-tmux.sh: CLIENT_ENV_1="GST_TRACERS=stats GST_DEBUG=GST_TRACER:7
GST_DEBUG_NO_COLOR=1 GST_DEBUG_FILE=<log>"). The stats tracer logs one `buffer`
record per buffer pushed on each pad, plus `new-element`/`new-pad` records that
name the numeric indices.

Usage:
    gst-rate.py LOG [--tail SECS] [--window START END] [--grep SUBSTR]
      --tail SECS      measure the last SECS seconds of the log (default 30)
      --window S E     measure log time S..E (seconds since the process started)
      --grep SUBSTR    only pads whose "element:pad" name contains SUBSTR
      --summary        only capture / encode-branch input / encoder output
    Prints one row per source pad that pushed buffers in the window:
      pad, buffers, avg fps over the window, min/max fps over 1 s buckets,
      avg buffer size in KiB.
Use `--json` for machine-readable output.
"""
import argparse
import collections
import json
import re
import sys

KV = re.compile(r"(\w[\w-]*)=\(\w+\)(\"[^\"]*\"|[^,;]*)")


def fields(line):
    out = {}
    for k, v in KV.findall(line.split("GST_TRACER :0:: ", 1)[1]):
        out[k] = v.strip('"')
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("log")
    ap.add_argument("--tail", type=float, default=30.0)
    ap.add_argument("--window", nargs=2, type=float)
    ap.add_argument("--grep", default="")
    ap.add_argument("--json", action="store_true")
    ap.add_argument("--summary", action="store_true", help="only the three stages that matter: capture, unique frames entering the encode branch, encoder output")
    a = ap.parse_args()

    elem_name, pad_name, pad_parent = {}, {}, {}
    # (pad-ix) -> list of (ts_seconds, size)
    pushes = collections.defaultdict(list)
    last_ts = 0.0
    with open(a.log, errors="replace") as f:
        for line in f:
            if "GST_TRACER :0:: " not in line:
                continue
            body = line.split("GST_TRACER :0:: ", 1)[1]
            if body.startswith("new-element,"):
                d = fields(line)
                elem_name[int(d["ix"])] = d["name"]
            elif body.startswith("new-pad,"):
                d = fields(line)
                pad_name[int(d["ix"])] = d["name"]
                pad_parent[int(d["ix"])] = int(d["parent-ix"])
            elif body.startswith("buffer,"):
                d = fields(line)
                ts = int(d["ts"]) / 1e9
                last_ts = max(last_ts, ts)
                pushes[int(d["pad-ix"])].append((ts, int(d["buffer-size"])))

    if a.window:
        t0, t1 = a.window
    else:
        t1 = last_ts
        t0 = max(0.0, t1 - a.tail)
    dur = t1 - t0
    rows = []
    for pad, evs in pushes.items():
        evs = [e for e in evs if t0 <= e[0] < t1]
        if not evs:
            continue
        name = f"{elem_name.get(pad_parent.get(pad, -1), '?')}:{pad_name.get(pad, '?')}"
        if a.grep not in name:
            continue
        buckets = collections.Counter(int(e[0] - t0) for e in evs)
        per_s = [buckets.get(i, 0) for i in range(int(dur))]
        rows.append({
            "pad": name,
            "buffers": len(evs),
            "avg_fps": round(len(evs) / dur, 1),
            "min_fps": min(per_s) if per_s else 0,
            "max_fps": max(per_s) if per_s else 0,
            "avg_kib": round(sum(e[1] for e in evs) / len(evs) / 1024, 1),
        })
    rows.sort(key=lambda r: -r["avg_fps"])
    if a.summary:
        keep = ("pipewiresrc", "branch_valve_", "branch_encoder_")
        rows = [r for r in rows if r["pad"].startswith(keep)]
        label = {"pipewiresrc": "capture (pipewiresrc out)", "branch_valve_": "into encode branch (after its queue)", "branch_encoder_": "encoder output"}
        for r in rows:
            r["pad"] = next(v for k, v in label.items() if r["pad"].startswith(k))
    if a.json:
        json.dump({"t0": t0, "t1": t1, "rows": rows}, sys.stdout, indent=1)
        print()
        return
    print(f"window {t0:.1f}s .. {t1:.1f}s ({dur:.1f} s)")
    print(f"{'pad':38} {'buffers':>8} {'avg fps':>8} {'min':>5} {'max':>5} {'KiB/buf':>9}")
    for r in rows:
        print(f"{r['pad']:38} {r['buffers']:8d} {r['avg_fps']:8.1f} {r['min_fps']:5d} {r['max_fps']:5d} {r['avg_kib']:9.1f}")


if __name__ == "__main__":
    main()
