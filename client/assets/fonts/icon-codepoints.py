#!/usr/bin/env python3
"""Print `("name", '\\u{XXXX}'),` lines for Material Symbols icon names.

Usage: python3 -I client/assets/fonts/icon-codepoints.py add cast close ...
Paste the output into `ICONS` in `client/src/theme.rs` (keep it sorted) to make a new
icon available through `theme::icon("name")`. Needs fontTools (`pip install fonttools`).
Glyph names in MaterialSymbolsOutlined.ttf are the icon names themselves; egui has no
ligature support, so icons are addressed by their private-use codepoint instead.
"""
import os
import sys

from fontTools.ttLib import TTFont

font = TTFont(os.path.join(os.path.dirname(os.path.abspath(__file__)), "MaterialSymbolsOutlined.ttf"))
rev = {}
for cp, glyph in sorted(font.getBestCmap().items()):
    if cp >= 0xE000:
        rev.setdefault(glyph, cp)
missing = []
for name in sorted(set(sys.argv[1:])):
    if name in rev:
        print(f'    ("{name}", \'\\u{{{rev[name]:x}}}\'),')
    else:
        missing.append(name)
if missing:
    sys.exit("unknown icon(s): " + ", ".join(missing))
