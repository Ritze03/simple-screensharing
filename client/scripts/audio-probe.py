#!/usr/bin/env python3
#
# LEGACY (2026-10-14): works on the user's REAL MangoWC desktop only (records a real app playback stream; NO audio tests are run any more). Agents must not
# use it: since the headless switch all real-app tests run in the private headless sway rig,
# see hs-rig.sh and superdoc/claude-instructions/testing-and-verification.md. Do not use.
"""audio-probe.py: is a given application's playback stream really carrying sound?

Lists PulseAudio/PipeWire sink-inputs (playback streams) whose application name,
binary or pid matches, and for each records SECS seconds of exactly that stream
(parec --monitor-stream on its sink's monitor) and prints peak and RMS in dBFS.
Used to prove the receiving client actually plays the shared audio (stream exists,
not corked, level above digital silence) and to read the source level of Spotify.

Usage: audio-probe.py MATCH [--secs 3] [--pid PID]
  MATCH: substring of application.name / application.process.binary (case-insens.)
"""
import argparse
import json
import math
import struct
import subprocess
import sys


def sh(*cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("match")
    ap.add_argument("--secs", type=float, default=3.0)
    ap.add_argument("--pid", type=int)
    a = ap.parse_args()

    inputs = json.loads(sh("pactl", "-f", "json", "list", "sink-inputs").stdout or "[]")
    sinks = {s["index"]: s["name"] for s in json.loads(sh("pactl", "-f", "json", "list", "sinks").stdout or "[]")}
    hits = []
    for i in inputs:
        p = i.get("properties", {})
        hay = " ".join(str(p.get(k, "")) for k in ("application.name", "application.process.binary", "media.name", "node.name")).lower()
        if a.match.lower() in hay and (a.pid is None or str(a.pid) == str(p.get("application.process.id", ""))):
            hits.append(i)
    if not hits:
        print(f"no sink-input matches {a.match!r}")
        sys.exit(1)
    for i in hits:
        p = i["properties"]
        vol = i.get("volume", {})
        pct = [v.get("value_percent") for v in vol.values()] if isinstance(vol, dict) else vol
        print(f"sink-input #{i['index']}: app={p.get('application.name')!r} media={p.get('media.name')!r} "
              f"pid={p.get('application.process.id')} corked={i.get('corked')} mute={i.get('mute')} vol={pct} "
              f"sink={sinks.get(i['sink'])}")
        sink = sinks.get(i["sink"])
        if not sink:
            continue
        cmd = ["parec", "-d", sink + ".monitor", f"--monitor-stream={i['index']}",
               "--rate=48000", "--channels=2", "--format=s16le", "--raw"]
        proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        try:
            data = b""
            import time
            t0 = time.time()
            while time.time() - t0 < a.secs:
                chunk = proc.stdout.read1(65536) if hasattr(proc.stdout, "read1") else proc.stdout.read(65536)
                if not chunk:
                    break
                data += chunk
        finally:
            proc.terminate()
        n = len(data) // 2
        if n == 0:
            print("   recorded nothing")
            continue
        s = struct.unpack(f"<{n}h", data[: n * 2])
        peak = max(abs(x) for x in s) / 32768
        rms = math.sqrt(sum(x * x for x in s) / n) / 32768
        db = lambda v: -999.0 if v <= 0 else 20 * math.log10(v)
        print(f"   {n/2/48000:.1f} s recorded: peak {db(peak):.1f} dBFS, rms {db(rms):.1f} dBFS "
              f"({'SOUND' if peak > 0.001 else 'SILENCE'})")


if __name__ == "__main__":
    main()
