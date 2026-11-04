//! The stage's geometry as pure functions (a port of the replica's `bestRows` / `gridRects` /
//! `layoutArena` / float maths, `superdoc/ui/replica/index.html`), so it can be tested without egui
//! running. Everything is relative to the arena's top-left corner; `w` x `h` is the arena size.
//!
//! - **Split / Crop** share one grid ([`best_rows`], [`Grid`]): rows of tiles filled in order, the row
//!   count that shows the most picture area wins. Split draws each picture whole ([`contain`]) in its cell,
//!   Crop fills the cell (the video is cropped around its centre). Resize bars ([`Bar`]) sit **midway
//!   between the two visible pictures**, not on the cell edge, and [`drag_bar`] finds the split that puts
//!   the bar under the pointer.
//! - **Floating** windows ([`FRect`], fractions of the arena) always keep their stream's aspect ratio
//!   ([`fit_float`]); [`resize_float`] moves the corner to the nearest point of the window's diagonal.

use egui::{Rect, pos2, vec2};

/// Gap between tiles (`--tile-gap`); also the resize bars' resting width.
pub const GAP: f32 = crate::theme::TILE_GAP;
/// A bar's hit area is its gap plus 3 px each side.
pub const BAR_HIT: f32 = GAP + 6.0;
/// Smallest share of a pair of cells either side of a bar may take.
const MIN_FR: f32 = 0.12;
/// Narrowest a floating window gets while resized (px).
pub const FLOAT_MIN_W: f32 = 160.0;

/// The largest rect of aspect ratio `ar` (w/h) centred in `cell`.
pub fn contain(cell: Rect, ar: f32) -> Rect {
    let (mut w, mut h) = (cell.width(), cell.width() / ar);
    if h > cell.height() {
        h = cell.height();
        w = h * ar;
    }
    Rect::from_center_size(cell.center(), vec2(w.max(0.0), h.max(0.0)))
}

/// Tiles per row for `ars.len()` tiles of aspect ratios `ars` in `w` x `h`: the row count whose pictures
/// add up to the most area (a tie within 1 px² keeps fewer rows).
pub fn best_rows(ars: &[f32], w: f32, h: f32) -> Vec<usize> {
    let n = ars.len();
    let mut best: Option<(Vec<usize>, f32)> = None;
    for rows in 1..=n {
        let mut counts = Vec::with_capacity(rows);
        let mut left = n;
        for i in 0..rows {
            let c = left.div_ceil(rows - i);
            counts.push(c);
            left -= c;
        }
        let rh = (h - GAP * (rows as f32 - 1.0)) / rows as f32;
        let (mut area, mut idx) = (0.0, 0);
        for &c in &counts {
            let cw = (w - GAP * (c as f32 - 1.0)) / c as f32;
            for _ in 0..c {
                let q = contain(Rect::from_min_size(pos2(0.0, 0.0), vec2(cw, rh)), ars[idx]);
                area += q.width() * q.height();
                idx += 1;
            }
        }
        if best.as_ref().is_none_or(|b| area > b.1 + 1.0) {
            best = Some((counts, area));
        }
    }
    best.map(|b| b.0).unwrap_or_default()
}

/// The grid's rows and the share of the free height / width each row / column takes.
#[derive(Clone, Debug, PartialEq)]
pub struct Grid {
    pub counts: Vec<usize>,
    pub row_fr: Vec<f32>,
    pub col_fr: Vec<Vec<f32>>,
}

impl Grid {
    /// Equal rows, equal columns.
    pub fn even(counts: Vec<usize>) -> Self {
        let rows = counts.len().max(1) as f32;
        Self { row_fr: vec![1.0 / rows; counts.len()], col_fr: counts.iter().map(|&c| vec![1.0 / c as f32; c]).collect(), counts }
    }

    /// Every cell, row by row.
    pub fn cells(&self, w: f32, h: f32) -> Vec<Rect> {
        let avail_h = (h - GAP * (self.counts.len() as f32 - 1.0)).max(0.0);
        let mut out = Vec::new();
        let mut y = 0.0;
        for (i, &c) in self.counts.iter().enumerate() {
            let rh = self.row_fr[i] * avail_h;
            let avail_w = (w - GAP * (c as f32 - 1.0)).max(0.0);
            let mut x = 0.0;
            for j in 0..c {
                let cw = self.col_fr[i][j] * avail_w;
                out.push(Rect::from_min_size(pos2(x, y), vec2(cw, rh)));
                x += cw + GAP;
            }
            y += rh + GAP;
        }
        out
    }
}

/// A resize bar: `V` between columns `col` and `col + 1` of `row`; `H` between `row` and `row + 1`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bar {
    V { row: usize, col: usize },
    H { row: usize },
}

/// Where the pictures and the bars are.
pub struct Placed {
    /// One rect per tile, in order: the visible picture (Split) or the whole cell (Crop).
    pub pics: Vec<Rect>,
    /// Each bar's hit rect ([`BAR_HIT`] thick). A `V` bar is as tall as its row, an `H` bar as wide as the arena.
    pub bars: Vec<(Bar, Rect)>,
}

/// Lay `grid` out in `w` x `h` for tiles of aspect ratios `ars`.
pub fn place(grid: &Grid, ars: &[f32], w: f32, h: f32, crop: bool) -> Placed {
    let cells = grid.cells(w, h);
    let pics: Vec<Rect> = cells.iter().zip(ars).map(|(c, &ar)| if crop { *c } else { contain(*c, ar) }).collect();
    let mut bars = Vec::new();
    let mut first = 0;
    for (i, &c) in grid.counts.iter().enumerate() {
        for j in 0..c.saturating_sub(1) {
            let (a, b) = (pics[first + j], pics[first + j + 1]);
            let cell = cells[first + j];
            // Midway between the pictures you see, not the cells: a letterboxed picture leaves the cell edge off-centre.
            let x = (a.right() + b.left() - BAR_HIT) / 2.0;
            bars.push((Bar::V { row: i, col: j }, Rect::from_min_size(pos2(x, cell.top()), vec2(BAR_HIT, cell.height()))));
        }
        let next = first + c;
        if let Some(&n) = grid.counts.get(i + 1) {
            let bottom = pics[first..next].iter().map(|r| r.bottom()).fold(f32::MIN, f32::max);
            let top = pics[next..next + n].iter().map(|r| r.top()).fold(f32::MAX, f32::min);
            bars.push((Bar::H { row: i }, Rect::from_min_size(pos2(0.0, (bottom + top - BAR_HIT) / 2.0), vec2(w, BAR_HIT))));
        }
        first = next;
    }
    Placed { pics, bars }
}

/// The coordinate a bar is dragged along (x of a `V` bar, y of an `H` bar).
pub fn bar_center(bar: Bar, rect: Rect) -> f32 {
    match bar {
        Bar::V { .. } => rect.center().x,
        Bar::H { .. } => rect.center().y,
    }
}

/// `start` with `bar` moved so that it is centred on `want` (same axis as [`bar_center`]), or as close as
/// the limits allow. The bar follows the pictures, which move slower than the cell edge (a picture only grows
/// while its cell limits it), so this searches the split that puts the bar there instead of moving the edge by
/// the pointer's distance.
pub fn drag_bar(start: &Grid, bar: Bar, want: f32, ars: &[f32], w: f32, h: f32, crop: bool) -> Grid {
    let mut g = start.clone();
    let tot = match bar {
        Bar::V { row, col } => start.col_fr[row][col] + start.col_fr[row][col + 1],
        Bar::H { row } => start.row_fr[row] + start.row_fr[row + 1],
    };
    let set = |g: &mut Grid, f: f32| match bar {
        Bar::V { row, col } => {
            g.col_fr[row][col] = f;
            g.col_fr[row][col + 1] = tot - f;
        }
        Bar::H { row } => {
            g.row_fr[row] = f;
            g.row_fr[row + 1] = tot - f;
        }
    };
    let (mut lo, mut hi) = (MIN_FR.min(tot / 2.0), (tot - MIN_FR).max(tot / 2.0));
    for _ in 0..24 {
        let f = (lo + hi) / 2.0;
        set(&mut g, f);
        let here = place(&g, ars, w, h, crop).bars.iter().find(|(b, _)| *b == bar).map_or(want, |(b, r)| bar_center(*b, *r));
        if here < want {
            lo = f;
        } else {
            hi = f;
        }
    }
    set(&mut g, (lo + hi) / 2.0);
    g
}

/// A floating window as fractions of the arena.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FRect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl FRect {
    pub fn from_rect(r: Rect, w: f32, h: f32) -> Self {
        Self { x: r.min.x / w, y: r.min.y / h, w: r.width() / w, h: r.height() / h }
    }

    pub fn to_rect(self, w: f32, h: f32) -> Rect {
        Rect::from_min_size(pos2(self.x * w, self.y * h), vec2(self.w * w, self.h * h))
    }
}

/// `f` with the stream's aspect ratio `ar` restored (the width wins), inside the arena. A window has its
/// stream's ratio whatever layout or frame size it came from, so there are never black bars.
pub fn fit_float(mut f: FRect, ar: f32, w: f32, h: f32) -> FRect {
    f.w = f.w.min(1.0).min(h * ar / w);
    f.h = f.w * w / ar / h;
    f.x = f.x.min(1.0 - f.w).max(0.0);
    f.y = f.y.min(1.0 - f.h).max(0.0);
    f
}

/// The window `o` dragged by (`dx`, `dy`) px, kept inside the arena.
pub fn move_float(o: FRect, dx: f32, dy: f32, w: f32, h: f32) -> FRect {
    FRect { x: (o.x + dx / w).min(1.0 - o.w).max(0.0), y: (o.y + dy / h).min(1.0 - o.h).max(0.0), ..o }
}

/// The window `o` with its bottom-right corner dragged by (`dx`, `dy`) px. The corner goes to the point of the
/// window's diagonal nearest the pointer, so width and height both count, growing and shrinking (moving only
/// left or only up shrinks it too). The top-left corner stays; the size is capped by the arena and
/// [`FLOAT_MIN_W`].
pub fn resize_float(o: FRect, dx: f32, dy: f32, ar: f32, w: f32, h: f32) -> FRect {
    let want = (o.w * w + dx + (o.h * h + dy) / ar) / (1.0 + 1.0 / (ar * ar));
    let nw = want.min((1.0 - o.x) * w).min((1.0 - o.y) * h * ar).max(FLOAT_MIN_W);
    fit_float(FRect { w: nw / w, ..o }, ar, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    const AR: f32 = 16.0 / 9.0;

    fn center_of(p: &Placed, bar: Bar) -> f32 {
        p.bars.iter().find(|(b, _)| *b == bar).map(|(b, r)| bar_center(*b, *r)).unwrap()
    }

    #[test]
    fn best_rows_picks_the_row_count_with_the_most_picture() {
        // Two 16:9 in a wide strip: side by side; in a tall column: stacked.
        assert_eq!(best_rows(&[AR, AR], 1600.0, 500.0), vec![2]);
        assert_eq!(best_rows(&[AR, AR], 600.0, 1000.0), vec![1, 1]);
        // Three: two rows, the first with 2 (rows are filled evenly from the top).
        assert_eq!(best_rows(&[AR; 3], 1000.0, 700.0), vec![2, 1]);
        assert_eq!(best_rows(&[AR; 4], 1600.0, 900.0), vec![2, 2]);
        assert_eq!(best_rows(&[AR], 800.0, 300.0), vec![1]);
        assert!(best_rows(&[], 800.0, 300.0).is_empty());
    }

    #[test]
    fn contain_letterboxes_and_centres() {
        let r = contain(Rect::from_min_size(pos2(10.0, 20.0), vec2(400.0, 400.0)), AR);
        assert!((r.width() - 400.0).abs() < 1e-3 && (r.height() - 225.0).abs() < 1e-3);
        assert!((r.center().x - 210.0).abs() < 1e-3 && (r.center().y - 220.0).abs() < 1e-3);
        let r = contain(Rect::from_min_size(pos2(0.0, 0.0), vec2(1000.0, 200.0)), AR);
        assert!((r.height() - 200.0).abs() < 1e-3 && (r.width() - 200.0 * AR).abs() < 1e-3);
    }

    #[test]
    fn cells_tile_the_arena_with_gaps() {
        let g = Grid::even(vec![2, 1]);
        let c = g.cells(1000.0, 500.0);
        assert_eq!(c.len(), 3);
        assert!((c[0].width() - 496.0).abs() < 1e-3 && (c[1].left() - 504.0).abs() < 1e-3);
        assert!((c[0].height() - 246.0).abs() < 1e-3 && (c[2].top() - 254.0).abs() < 1e-3 && (c[2].width() - 1000.0).abs() < 1e-3);
    }

    #[test]
    fn bars_sit_midway_between_the_visible_pictures_not_on_the_cell_edge() {
        // Two 16:9 in a 1800x400 arena: height-limited pictures (711 wide) in 896 wide cells, so a cell
        // edge is not where the bar belongs.
        let ars = [AR, AR];
        let g = Grid::even(vec![2]);
        let p = place(&g, &ars, 1800.0, 400.0, false);
        let (a, b) = (p.pics[0], p.pics[1]);
        assert!(a.width() < g.cells(1800.0, 400.0)[0].width() - 1.0, "letterboxed");
        let c = center_of(&p, Bar::V { row: 0, col: 0 });
        assert!((c - (a.right() + b.left()) / 2.0).abs() < 1e-3);
        // Crop: the pictures are the cells, the bar is the middle of the gap.
        let p = place(&g, &ars, 1800.0, 400.0, true);
        assert!((center_of(&p, Bar::V { row: 0, col: 0 }) - 900.0).abs() < 1e-3);
        // A horizontal bar between rows of differing picture height uses the pictures too.
        let g = Grid::even(vec![1, 1]);
        let p = place(&g, &ars, 400.0, 1000.0, false);
        let mid = (p.pics[0].bottom() + p.pics[1].top()) / 2.0;
        assert!((center_of(&p, Bar::H { row: 0 }) - mid).abs() < 1e-3);
    }

    #[test]
    fn a_dragged_bar_follows_the_pointer_one_to_one_until_the_limit() {
        let ars = [AR, AR];
        for (w, h, crop) in [(1800.0, 400.0, false), (1000.0, 500.0, false), (1800.0, 400.0, true), (1000.0, 500.0, true)] {
            let g = Grid::even(vec![2]);
            let bar = Bar::V { row: 0, col: 0 };
            let c0 = center_of(&place(&g, &ars, w, h, crop), bar);
            for d in [60.0, -60.0, 25.0] {
                let g2 = drag_bar(&g, bar, c0 + d, &ars, w, h, crop);
                let c = center_of(&place(&g2, &ars, w, h, crop), bar);
                assert!((c - (c0 + d)).abs() < 0.5, "{w}x{h} crop={crop} d={d}: bar at {c}, wanted {}", c0 + d);
            }
            // Far past the limit it stops at the limit, never crossing it.
            let g2 = drag_bar(&g, bar, w * 3.0, &ars, w, h, crop);
            assert!(g2.col_fr[0][0] <= 1.0 - MIN_FR + 1e-4 && g2.col_fr[0][1] >= MIN_FR - 1e-4);
            let g3 = drag_bar(&g, bar, -w, &ars, w, h, crop);
            assert!(g3.col_fr[0][0] >= MIN_FR - 1e-4);
        }
        // A row bar too.
        let g = Grid::even(vec![1, 1]);
        let bar = Bar::H { row: 0 };
        let c0 = center_of(&place(&g, &ars, 600.0, 900.0, true), bar);
        let g2 = drag_bar(&g, bar, c0 + 60.0, &ars, 600.0, 900.0, true);
        assert!((center_of(&place(&g2, &ars, 600.0, 900.0, true), bar) - c0 - 60.0).abs() < 0.5);
    }

    #[test]
    fn dragging_a_bar_keeps_the_other_columns() {
        let g = Grid::even(vec![3]);
        let g2 = drag_bar(&g, Bar::V { row: 0, col: 0 }, 200.0, &[AR; 3], 1500.0, 500.0, true);
        assert!((g2.col_fr[0][2] - g.col_fr[0][2]).abs() < 1e-6);
        assert!((g2.col_fr[0].iter().sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn a_floating_window_always_has_its_streams_aspect() {
        let (w, h) = (1200.0, 600.0);
        // A Crop cell (wider than the picture) turned into a window: ratio restored, no black bars.
        let f = fit_float(FRect::from_rect(Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 600.0)), w, h), AR, w, h);
        let r = f.to_rect(w, h);
        assert!((r.width() / r.height() - AR).abs() < 1e-3, "{r:?}");
        // The stream's ratio changes (a frame size change): the width stays, the height follows.
        let f2 = fit_float(f, 4.0 / 3.0, w, h);
        let r2 = f2.to_rect(w, h);
        assert!((r2.width() / r2.height() - 4.0 / 3.0).abs() < 1e-3);
        // Never bigger than the arena, never outside it.
        let big = fit_float(FRect { x: 0.9, y: 0.9, w: 3.0, h: 3.0 }, AR, w, h);
        let r = big.to_rect(w, h);
        assert!(r.right() <= w + 1e-2 && r.bottom() <= h + 1e-2 && r.left() >= -1e-3 && r.top() >= -1e-3);
        assert!((r.width() / r.height() - AR).abs() < 1e-3);
    }

    #[test]
    fn corner_resize_uses_width_and_height_together() {
        let (w, h) = (1600.0, 900.0);
        let o = fit_float(FRect { x: 0.1, y: 0.1, w: 0.4, h: 0.0 }, AR, w, h);
        let ow = o.w * w;
        // Pulling the corner right-and-down grows, left-and-up shrinks.
        assert!(resize_float(o, 100.0, 56.25, AR, w, h).w * w > ow + 90.0);
        assert!(resize_float(o, -100.0, -56.25, AR, w, h).w * w < ow - 90.0);
        // Moving only left, or only up, also shrinks it (the old width-only/height-only rules did not both).
        assert!(resize_float(o, -80.0, 0.0, AR, w, h).w * w < ow - 10.0);
        assert!(resize_float(o, 0.0, -80.0, AR, w, h).w * w < ow - 10.0);
        // Only down or only right grows it.
        assert!(resize_float(o, 0.0, 80.0, AR, w, h).w * w > ow + 10.0);
        // The ratio holds, the top-left stays, the window stays inside the arena and above the minimum.
        let r = resize_float(o, 5000.0, 5000.0, AR, w, h);
        assert!((r.x - o.x).abs() < 1e-6 && (r.y - o.y).abs() < 1e-6);
        let rr = r.to_rect(w, h);
        assert!(rr.right() <= w + 1e-2 && rr.bottom() <= h + 1e-2);
        assert!((rr.width() / rr.height() - AR).abs() < 1e-3);
        let tiny = resize_float(o, -5000.0, -5000.0, AR, w, h);
        assert!((tiny.w * w - FLOAT_MIN_W).abs() < 1e-2);
    }

    #[test]
    fn a_dragged_window_stops_at_the_arena_edge() {
        let (w, h) = (1000.0, 500.0);
        let o = FRect { x: 0.2, y: 0.2, w: 0.3, h: 0.3 };
        let m = move_float(o, 60.0, -30.0, w, h);
        assert!((m.x * w - (0.2 * w + 60.0)).abs() < 1e-3 && (m.y * h - (0.2 * h - 30.0)).abs() < 1e-3);
        let far = move_float(o, 9999.0, -9999.0, w, h);
        assert!((far.x + far.w - 1.0).abs() < 1e-6 && far.y.abs() < 1e-6);
    }
}
