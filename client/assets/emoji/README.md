# Emoji avatar pictures

The 40 emoji of the avatar picker (`client/src/widgets/emoji.rs`, `EMOJI`), embedded with
`include_bytes!` because egui cannot draw colour emoji and the AppImage / Flatpak / Windows builds cannot
rely on a system emoji font. One file per emoji, named by its code point (`1f98a.png` = U+1F98A).

- **Source:** Noto Color Emoji (googlefonts/noto-emoji, Copyright 2013 Google LLC), version 2.057 as shipped
  by Arch's `noto-fonts-emoji`. Each PNG is the font's own 136x128 bitmap (the 109 ppem strike of its CBDT
  table) extracted byte for byte with `extract.py`; nothing is resized or edited.
- **Licence:** SIL Open Font License 1.1, text in `OFL-NotoColorEmoji.txt` (copied from the project's own
  `LICENSE`, https://github.com/googlefonts/noto-emoji). It permits redistribution and modification as long
  as the licence text travels with the files, which is what that file is for. The font has no Reserved
  Font Name.
- **Why not the repo's `png/` directory:** the project no longer ships one (404 on `main`), only fonts
  and tooling; the font is the official source of the bitmaps.
- **Changing the set:** edit the list in `extract.py`, `EMOJI` + `PICTURES` in `emoji.rs` and the replica's
  `const EMOJI` together, re-run `extract.py` (see its docstring). A test checks every file name against its
  code point and decodes every picture.
