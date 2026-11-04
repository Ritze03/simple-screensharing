#!/usr/bin/env python3
"""Drag a webcam in picture to every one of its 8 spots with the real pointer and say where it landed (task I8).

Needs the rig (hs-rig.sh up) and a client whose Layout menu has "Webcam in picture" on with a PIP showing
(the dump has `pip.0`; see client/scripts/README.md). Each drag is a real press, motion and release through
`hs-drive.sh drag`; after it the client's dump must say `pipinfo 0 spot=<column>,<row>` for the wanted spot.

usage: SCRATCH=<rig scratch> hs-pip-spots.py CLIENT OUTPUT TILE_RECT PIP_RECT [SHOT_PREFIX]
       e.g. hs-pip-spots.py 2 HEADLESS-3 tile.0 pip.0 /path/spot     (writes /path/spot-<col><row>.png)
Exit status 1 when a PIP did not land where it was dragged.
"""
import os
import subprocess
import sys
import time

here = os.path.dirname(os.path.abspath(__file__))
scratch = os.environ["SCRATCH"]
client, output, tile_name, pip_name = sys.argv[1:5]
prefix = sys.argv[5] if len(sys.argv) > 5 else None
ui = f"{scratch}/ui{client}.txt"
drive = f"{here}/hs-drive.sh"


def rect(name):
    for line in open(ui):
        p = line.split()
        if p[:2] == ["rect", name]:
            return tuple(float(x) for x in p[2:6])
    raise SystemExit(f"the dump has no rect {name}")


def pipinfo():
    return next((l.strip() for l in open(ui) if l.startswith("pipinfo")), "?")


bad = 0
for j in (0, 1, 2):
    for i in (0, 1, 2):
        if (i, j) == (1, 1):
            continue
        t, p = rect(tile_name), rect(pip_name)
        pw, ph = p[2] - p[0], p[3] - p[1]
        # where the pointer ends: the middle of the PIP when it sits on that spot (12 px from the edges)
        tx = t[0] + 12 + pw / 2 if i == 0 else (t[0] + t[2]) / 2 if i == 1 else t[2] - 12 - pw / 2
        ty = t[1] + 12 + ph / 2 if j == 0 else (t[1] + t[3]) / 2 if j == 1 else t[3] - 12 - ph / 2
        sx, sy = (p[0] + p[2]) / 2, (p[1] + p[3]) / 2
        subprocess.run([drive, "drag", client, f"{sx:.0f}", f"{sy:.0f}", f"{tx:.0f}", f"{ty:.0f}", "0", "12"], check=True, timeout=60)
        time.sleep(1.0)
        got = pipinfo()
        ok = f"spot={i},{j} " in got + " "
        bad += not ok
        print(f"want {i},{j}: {'OK' if ok else 'FAIL'}  {got}")
        if prefix:
            subprocess.run([drive, "shot", f"{prefix}-{i}{j}.png", output], check=True, timeout=60)
sys.exit(1 if bad else 0)
