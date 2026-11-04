// The stage's geometry as pure functions: a line-for-line port of the native `client/src/ui/stage/geom.rs` (which is itself
// the replica's `bestRows` / `gridRects` / `layoutArena` / float maths), so both clients lay out identically and the same
// tests apply (dev/tests.js "stage geometry"). Rects are `{x, y, w, h}` relative to the arena's top-left; `w` x `h` is the
// arena size.
//
// - Split / Crop share one grid (`bestRows`, `Grid`): rows of tiles filled in order, the row count that shows the most
//   picture area wins. Split draws each picture whole (`contain`) in its cell, Crop fills the cell. Resize bars sit midway
//   between the two VISIBLE pictures, never on the hidden cell edge, and `dragBar` finds the split that puts the bar under the
//   pointer.
// - Floating windows (fractions of the arena) always keep their stream's aspect ratio (`fitFloat`); `resizeFloat` moves the
//   corner to the nearest point of the window's diagonal.

export const GAP = 8; // gap between tiles (also the bars' resting width)
export const BAR_HIT = GAP + 6; // a bar's hit area is its gap plus 3 px each side
export const MIN_FR = 0.12; // smallest share of a pair of cells either side of a bar
export const FLOAT_MIN_W = 160; // narrowest a floating window gets while resized (px)

/** The largest rect of aspect ratio `ar` (w/h) centred in `cell`. */
export function contain(cell, ar) {
  let w = cell.w;
  let h = cell.w / ar;
  if (h > cell.h) {
    h = cell.h;
    w = h * ar;
  }
  w = Math.max(w, 0);
  h = Math.max(h, 0);
  return { x: cell.x + (cell.w - w) / 2, y: cell.y + (cell.h - h) / 2, w, h };
}

/** Tiles per row for `ars.length` tiles of aspect ratios `ars` in `w` x `h`: the row count whose pictures add up to the most
 *  area (a tie within 1 px^2 keeps fewer rows). */
export function bestRows(ars, w, h) {
  const n = ars.length;
  let best = null;
  for (let rows = 1; rows <= n; rows++) {
    const counts = [];
    for (let i = 0, left = n; i < rows; i++) {
      const c = Math.ceil(left / (rows - i));
      counts.push(c);
      left -= c;
    }
    const rh = (h - GAP * (rows - 1)) / rows;
    let area = 0;
    let idx = 0;
    for (const c of counts) {
      const cw = (w - GAP * (c - 1)) / c;
      for (let j = 0; j < c; j++, idx++) {
        const q = contain({ x: 0, y: 0, w: cw, h: rh }, ars[idx]);
        area += q.w * q.h;
      }
    }
    if (!best || area > best.area + 1) best = { counts, area };
  }
  return best ? best.counts : [];
}

/** Equal rows, equal columns: `{ counts, rowFr, colFr }` (shares of the free height / width). */
export function evenGrid(counts) {
  const rows = Math.max(counts.length, 1);
  return { counts: [...counts], rowFr: counts.map(() => 1 / rows), colFr: counts.map((c) => Array(c).fill(1 / c)) };
}
export const cloneGrid = (g) => ({ counts: [...g.counts], rowFr: [...g.rowFr], colFr: g.colFr.map((r) => [...r]) });

/** Every cell, row by row. */
export function gridCells(grid, w, h) {
  const availH = Math.max(h - GAP * (grid.counts.length - 1), 0);
  const out = [];
  let y = 0;
  grid.counts.forEach((c, i) => {
    const rh = grid.rowFr[i] * availH;
    const availW = Math.max(w - GAP * (c - 1), 0);
    let x = 0;
    for (let j = 0; j < c; j++) {
      const cw = grid.colFr[i][j] * availW;
      out.push({ x, y, w: cw, h: rh });
      x += cw + GAP;
    }
    y += rh + GAP;
  });
  return out;
}

/** A resize bar: `{ t: "v", row, col }` between columns `col` and `col + 1` of `row`; `{ t: "h", row }` between `row` and `row + 1`. */
export const barKey = (b) => (b.t === "v" ? `v${b.row}.${b.col}` : `h${b.row}`);
export const sameBar = (a, b) => a.t === b.t && a.row === b.row && (a.t === "h" || a.col === b.col);

/** Lay `grid` out in `w` x `h` for tiles of aspect ratios `ars`: `pics` = one rect per tile (the visible picture in Split, the
 *  whole cell in Crop), `bars` = `[{ bar, rect }]` hit rects (BAR_HIT thick; a vertical bar is as tall as its row, a horizontal
 *  one as wide as the arena). */
export function place(grid, ars, w, h, crop) {
  const cells = gridCells(grid, w, h);
  const pics = cells.map((c, i) => (crop ? c : contain(c, ars[i])));
  const bars = [];
  let first = 0;
  grid.counts.forEach((c, i) => {
    for (let j = 0; j < c - 1; j++) {
      const a = pics[first + j];
      const b = pics[first + j + 1];
      const cell = cells[first + j];
      // Midway between the pictures you see, not the cells: a letterboxed picture leaves the cell edge off-centre.
      const x = (a.x + a.w + b.x - BAR_HIT) / 2;
      bars.push({ bar: { t: "v", row: i, col: j }, rect: { x, y: cell.y, w: BAR_HIT, h: cell.h } });
    }
    const next = first + c;
    const n = grid.counts[i + 1];
    if (n != null) {
      const bottom = Math.max(...pics.slice(first, next).map((r) => r.y + r.h));
      const top = Math.min(...pics.slice(next, next + n).map((r) => r.y));
      bars.push({ bar: { t: "h", row: i }, rect: { x: 0, y: (bottom + top - BAR_HIT) / 2, w, h: BAR_HIT } });
    }
    first = next;
  });
  return { pics, bars };
}

/** The coordinate a bar is dragged along (x of a vertical bar, y of a horizontal one). */
export const barCenter = (bar, r) => (bar.t === "v" ? r.x + r.w / 2 : r.y + r.h / 2);

/** `start` with `bar` moved so that it is centred on `want` (same axis as `barCenter`), or as close as the limits allow. The bar
 *  follows the pictures, which move slower than the cell edge (a picture only grows while its cell limits it), so this searches
 *  the split that puts the bar there instead of moving the edge by the pointer's distance. */
export function dragBar(start, bar, want, ars, w, h, crop) {
  const g = cloneGrid(start);
  const tot = bar.t === "v" ? start.colFr[bar.row][bar.col] + start.colFr[bar.row][bar.col + 1] : start.rowFr[bar.row] + start.rowFr[bar.row + 1];
  const set = (f) => {
    if (bar.t === "v") {
      g.colFr[bar.row][bar.col] = f;
      g.colFr[bar.row][bar.col + 1] = tot - f;
    } else {
      g.rowFr[bar.row] = f;
      g.rowFr[bar.row + 1] = tot - f;
    }
  };
  let lo = Math.min(MIN_FR, tot / 2);
  let hi = Math.max(tot - MIN_FR, tot / 2);
  for (let i = 0; i < 24; i++) {
    const f = (lo + hi) / 2;
    set(f);
    const here = place(g, ars, w, h, crop).bars.find((b) => sameBar(b.bar, bar));
    if ((here ? barCenter(bar, here.rect) : want) < want) lo = f;
    else hi = f;
  }
  set((lo + hi) / 2);
  return g;
}

/** `f` (`{x, y, w, h}`, fractions of the arena) with the stream's aspect ratio `ar` restored (the width wins), inside the arena:
 *  a window has its stream's ratio whatever layout or frame size it came from, so there are never black bars. */
export function fitFloat(f, ar, w, h) {
  const nw = Math.min(f.w, 1, (h * ar) / w);
  const nh = (nw * w) / ar / h;
  return { x: Math.max(0, Math.min(f.x, 1 - nw)), y: Math.max(0, Math.min(f.y, 1 - nh)), w: nw, h: nh };
}

/** The window `o` dragged by (`dx`, `dy`) px, kept inside the arena. */
export function moveFloat(o, dx, dy, w, h) {
  return { ...o, x: Math.max(0, Math.min(o.x + dx / w, 1 - o.w)), y: Math.max(0, Math.min(o.y + dy / h, 1 - o.h)) };
}

/** The window `o` with its bottom-right corner dragged by (`dx`, `dy`) px. The corner goes to the point of the window's diagonal
 *  nearest the pointer, so width and height both count, growing and shrinking (moving only left or only up shrinks it too). The
 *  top-left stays; the size is capped by the arena and `FLOAT_MIN_W`. */
export function resizeFloat(o, dx, dy, ar, w, h) {
  const want = (o.w * w + dx + (o.h * h + dy) / ar) / (1 + 1 / (ar * ar));
  const nw = Math.max(Math.min(want, (1 - o.x) * w, (1 - o.y) * h * ar), FLOAT_MIN_W);
  return fitFloat({ ...o, w: nw / w }, ar, w, h);
}

/** Native card_keeps (`stage/strip.rs`): whether the pointer `q` keeps a person's hover card open: on the card, on the avatar or in
 *  the gap straight between them (the avatar's own x range), each grown by 3 px. NOT the whole gap under the card: a card is wider
 *  than a person's cell, so the union of card and avatar would cover the neighbours' avatars and every card you pass stayed open
 *  under the next one (found in the native rig, D1). Rects are `{left, top, right, bottom}` (DOMRect works). */
export function cardKeeps(card, av, q) {
  const gap = { left: av.left, right: av.right, top: card.bottom, bottom: av.top };
  return [card, av, gap].some((r) => q.x >= r.left - 3 && q.x <= r.right + 3 && q.y >= r.top - 3 && q.y <= r.bottom + 3);
}
