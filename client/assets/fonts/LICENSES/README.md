# Vendored font licenses (egui client)

These fonts are embedded into the egui binary with `include_bytes!` (see
`client/src/theme.rs`, `install_fonts`). They are copies of the Electron frontend's
`client/frontend/fonts/`, so the native client renders with the same faces.

- **`Geist-Regular.ttf`, `Geist-Bold.ttf`, `GeistMono-Regular.ttf`** — Geist / Geist Mono,
  SIL Open Font License 1.1. See `OFL-Geist.txt`.
- **`MaterialSymbolsOutlined.ttf`** — Material Symbols Outlined (static, weight 400,
  FILL 0), Apache License 2.0 by Google. See `Apache-2.0-MaterialSymbols.txt`.
  Converted losslessly from the frontend's `MaterialSymbolsOutlined.woff2` with fontTools
  (`TTFont('in.woff2'); f.flavor = None; f.save('out.ttf')`), because egui cannot read
  WOFF2. Not subset: the full font is ~940 KB, small next to the rest of the binary, and
  keeping every glyph means adding an icon is a one-line table edit (run
  `../icon-codepoints.py <name>`), not a font rebuild.

Both licenses permit redistribution as long as the license text travels with the font
files, which is what this directory is for.
