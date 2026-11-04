#!/usr/bin/env python3
"""Regenerate the PNGs in this directory from a Noto Color Emoji font (CBDT bitmaps).

    cd client/assets/emoji && python3 -I extract.py /usr/share/fonts/noto/NotoColorEmoji.ttf

Writes `<hex codepoint>.png`: the font's own 136x128 bitmap, byte for byte (no resize, no edit).
The list must match `EMOJI` in `client/src/widgets/emoji.rs` and `const EMOJI` in the HTML replica.
"""
import sys
from fontTools.ttLib import TTFont

EMOJI = "😀😎🤓🥳🤖👻👽🎃💀🤡🐶🐱🦊🐻🐼🐸🐙🦄🐧🦉🍕🍔🍩🍣🍉☕🌮🍺🎮🎧🎬🎸⚽🚀🌈⭐🔥⚡🌙🌵"
assert len(EMOJI) == 40 and all(ord(c) > 0x2000 for c in EMOJI)

font = TTFont(sys.argv[1])
cmap = font.getBestCmap()
strike = font["CBDT"].strikeData[0]
for c in EMOJI:
    data = strike[cmap[ord(c)]].imageData
    assert data[:8] == b"\x89PNG\r\n\x1a\n"
    with open(f"{ord(c):x}.png", "wb") as f:
        f.write(data)
