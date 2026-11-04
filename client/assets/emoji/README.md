# Emoji avatar font

`NotoColorEmoji.ttf` is embedded with `include_bytes!` (`client/src/widgets/emoji.rs`) because egui cannot draw
colour emoji and the AppImage / Flatpak / Windows builds cannot rely on a system emoji font. Avatars draw any emoji
from it: rustybuzz shapes the string (flags, skin tones, ZWJ sequences and keycaps are GSUB ligatures of its `ccmp`
feature) and the glyph's PNG comes out of the CBDT table.

- **Source:** Noto Color Emoji (googlefonts/noto-emoji, Copyright 2013 Google LLC), version 2.057 as shipped by Arch's
  `noto-fonts-emoji` (`/usr/share/fonts/noto/NotoColorEmoji.ttf`), byte for byte. 10.7 MB, about 4000 glyphs.
- **It must be the CBDT build:** one 109 ppem strike of 136x128 PNGs (`EM_PX` in `emoji.rs`). The COLRv1 build has no
  bitmaps, so nothing would draw (`every_picker_emoji_has_a_picture` fails). To check a new file:
  `python3 -c "from fontTools.ttLib import TTFont as T; print(sorted(T('NotoColorEmoji.ttf').keys()))"` must list `CBDT`, `CBLC`, `GSUB`.
- **Licence:** SIL Open Font License 1.1, text in `OFL-NotoColorEmoji.txt` (copied from the project's own `LICENSE`,
  https://github.com/googlefonts/noto-emoji). It permits redistribution and modification as long as the licence text
  travels with the files, which is what that file is for. The font has no Reserved Font Name.
- **Why the whole font and not the 40 picker bitmaps** (the first version, until 2026-11-04): a person can paste any
  emoji, as in the browser, and a peer's emoji has to draw too. The PNGs and `extract.py` are gone.
- **Changing the picker's set:** edit `EMOJI` in `emoji.rs`, the replica's `const EMOJI` and `docs/js/ui/dom.js` `EMOJI` together.
