# Vendored font licenses

- **`GeistMono-Regular.ttf`** — Geist Mono, licensed under the SIL Open Font License 1.1.
  See `OFL-Geist.txt` in this directory. Sourced locally from a sibling project's asset
  cache (`ritz/resources/assets/GeistMono-Regular.ttf`) rather than downloaded fresh, per
  the same upstream release the design handoff referenced.
- **`MaterialSymbolsOutlined.woff2`** — Material Symbols Outlined, licensed under the
  Apache License 2.0 by Google. Fetched directly from `fonts.gstatic.com` (Google Fonts'
  CDN) at build time — network access was available — then vendored into this repo so
  the app has no runtime dependency on the CDN. See `Apache-2.0-MaterialSymbols.txt` in
  this directory for the full license text (Apache 2.0 §4 requires a copy of the license
  travel with any redistribution, not just a link).

Both licenses permit redistribution (including in a commercial product) as long as the
license text/notice travels with the font files, which is what this directory is for.
