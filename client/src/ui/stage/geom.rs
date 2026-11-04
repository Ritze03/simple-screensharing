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

// ---------------------------------------------------------------------------
// Webcam in picture (task I8): the replica's `snapPip` / `layoutPip` / `movePip`
// ---------------------------------------------------------------------------

/// How far a PIP sits from the edges of the visible picture on its spot (px; less when the picture is small).
pub const PIP_INSET: f32 = 12.0;
/// Smallest a PIP gets (px wide), unless the visible picture is smaller.
pub const PIP_MIN: f32 = 72.0;

/// A webcam's size and place inside its parent's picture, one per webcam and view (the replica's `pipPos`).
/// `w` is a fraction of the picture's width. The place is one of eight [`pip_at`] spots (`spot`, column and row 0..=2,
/// the middle excluded: the corners and the middle of each side), which is what survives layout changes, tile
/// resizes and fullscreen. Only while a drag is in progress is it free (`spot` is `None`, `x`/`y` are fractions of
/// the picture): on release it goes to the nearest spot ([`snap_pip`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PipPos {
    pub w: f32,
    pub spot: Option<(u8, u8)>,
    pub x: f32,
    pub y: f32,
}

impl Default for PipPos {
    /// A quarter of the picture's width (.24), bottom right.
    fn default() -> Self {
        Self { w: 0.24, spot: Some((2, 2)), x: 0.0, y: 0.0 }
    }
}

/// A corner of a PIP: the one that carries its resize handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Corner {
    pub right: bool,
    pub bottom: bool,
}

/// A PIP laid out in its parent tile (all rects relative to the tile's top-left): the PIP itself, the whole picture
/// (`pic`: bigger than the tile in Crop) and the part of it you can see (`vis`), the PIP's aspect ratio and where
/// its handle is.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PipGeo {
    pub rect: Rect,
    pub pic: Rect,
    pub vis: Rect,
    pub ar: f32,
    pub corner: Corner,
}

/// Where a PIP of `size` starts along an axis of the visible picture (`origin`, `len`): column or row `i` of the
/// three (start, middle, end). The inset shrinks when the picture is barely bigger than the PIP.
pub fn pip_at(origin: f32, len: f32, size: f32, i: u8) -> f32 {
    let m = PIP_INSET.min((len - size) / 2.0).max(0.0);
    match i {
        0 => origin + m,
        1 => origin + (len - size) / 2.0,
        _ => origin + len - size - m,
    }
}

/// The picture of a stream of aspect `ar` in a `tw` x `th` tile, and what shows of it: all of it when contained
/// (Split, Floating), the centre when it fills the tile (Crop, `Fit::Cover`).
pub fn picture(tw: f32, th: f32, ar: f32, crop: bool) -> (Rect, Rect) {
    let pw = if crop { tw.max(th * ar) } else { tw.min(th * ar) };
    let ph = pw / ar;
    let pic = Rect::from_min_size(pos2((tw - pw) / 2.0, (th - ph) / 2.0), vec2(pw, ph));
    let vis = pic.intersect(Rect::from_min_size(pos2(0.0, 0.0), vec2(tw, th)));
    (pic, vis)
}

/// Place a webcam of aspect `a` inside a `tw` x `th` tile showing a stream of aspect `ar`. Size and place are
/// fractions of the **picture**, so they survive a tile resize and every layout; what is shown is clamped to the
/// visible part, so Crop does not lose the Split position (the stored value stays). *Why the handle faces the
/// middle:* a PIP pushed into a corner could otherwise only shrink, and in Floating its handle would land on the
/// window's own. `ar` of 0 or less counts as 16:9.
pub fn layout_pip(tw: f32, th: f32, ar: f32, a: f32, crop: bool, p: &PipPos) -> PipGeo {
    let ar = if ar > 0.0 { ar } else { 16.0 / 9.0 };
    let (pic, vis) = picture(tw, th, ar, crop);
    let w = (p.w * pic.width()).max(PIP_MIN).min(vis.width()).min(vis.height() * a);
    let h = w / a;
    let (x, y) = match p.spot {
        Some((i, j)) => (pip_at(vis.min.x, vis.width(), w, i), pip_at(vis.min.y, vis.height(), h, j)),
        None => (
            (pic.min.x + p.x * pic.width()).min(vis.max.x - w).max(vis.min.x),
            (pic.min.y + p.y * pic.height()).min(vis.max.y - h).max(vis.min.y),
        ),
    };
    let rect = Rect::from_min_size(pos2(x, y), vec2(w, h));
    let corner = Corner { right: rect.center().x <= vis.center().x, bottom: rect.center().y <= vis.center().y };
    PipGeo { rect, pic, vis, ar: a, corner }
}

/// The spot nearest to where a dragged PIP is.
pub fn nearest_spot(g: &PipGeo) -> (u8, u8) {
    let mut best = (f32::INFINITY, (2, 2));
    for i in 0..3u8 {
        for j in 0..3u8 {
            if (i, j) == (1, 1) {
                continue;
            }
            let at = pos2(pip_at(g.vis.min.x, g.vis.width(), g.rect.width(), i), pip_at(g.vis.min.y, g.vis.height(), g.rect.height(), j));
            let d = (at - g.rect.min).length();
            if d < best.0 {
                best = (d, (i, j));
            }
        }
    }
    best.1
}

/// Release of a drag: a PIP that was moved freely goes to the nearest spot. One that already has a spot (a click,
/// a resize) stays.
pub fn snap_pip(g: &PipGeo, p: &mut PipPos) {
    if p.spot.is_none() {
        p.spot = Some(nearest_spot(g));
    }
}

/// `p` for a PIP at `rect` in the picture `g.pic`.
fn store(g: &PipGeo, rect: Rect, p: &mut PipPos) {
    *p = PipPos { x: (rect.min.x - g.pic.min.x) / g.pic.width(), y: (rect.min.y - g.pic.min.y) / g.pic.height(), w: rect.width() / g.pic.width(), spot: p.spot };
}

/// The PIP `g` as it was at the press, dragged by `d` px: free (no spot), kept inside the visible picture, so
/// dragging past an edge stops at the edge, in Crop too.
pub fn move_pip(g: &PipGeo, d: egui::Vec2, p: &mut PipPos) {
    let (w, h) = (g.rect.width(), g.rect.height());
    let x = (g.rect.min.x + d.x).min(g.vis.max.x - w).max(g.vis.min.x);
    let y = (g.rect.min.y + d.y).min(g.vis.max.y - h).max(g.vis.min.y);
    p.spot = None;
    store(g, Rect::from_min_size(pos2(x, y), vec2(w, h)), p);
}

/// The PIP `g` as it was at the press with its handle on `c` dragged by `d` px: the opposite corner stays, the
/// aspect ratio holds, the handle goes to the point of the diagonal nearest the pointer (so width and height both
/// count), and it stays inside the visible picture and above [`PIP_MIN`]. The spot is untouched.
pub fn resize_pip(g: &PipGeo, c: Corner, d: egui::Vec2, p: &mut PipPos) {
    let a = g.ar;
    let (sx, sy) = (if c.right { 1.0 } else { -1.0 }, if c.bottom { 1.0 } else { -1.0 });
    let (w, h) = (g.rect.width(), g.rect.height());
    let (fx, fy) = (if c.right { g.rect.min.x } else { g.rect.max.x }, if c.bottom { g.rect.min.y } else { g.rect.max.y });
    let want = (w + sx * d.x + (h + sy * d.y) / a) / (1.0 + 1.0 / (a * a));
    let cap_x = if c.right { g.vis.max.x - fx } else { fx - g.vis.min.x };
    let cap_y = if c.bottom { g.vis.max.y - fy } else { fy - g.vis.min.y };
    let nw = want.min(cap_x).min(cap_y * a).max(PIP_MIN.min(g.vis.width()).min(g.vis.height() * a));
    let nx = if c.right { fx } else { fx - nw };
    let ny = if c.bottom { fy } else { fy - nw / a };
    store(g, Rect::from_min_size(pos2(nx, ny), vec2(nw, nw / a)), p);
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

    // --- webcam in picture (task I8) ---

    const CAM: f32 = 16.0 / 9.0;

    fn inside(r: Rect, outer: Rect) -> bool {
        r.min.x >= outer.min.x - 1e-3 && r.min.y >= outer.min.y - 1e-3 && r.max.x <= outer.max.x + 1e-3 && r.max.y <= outer.max.y + 1e-3
    }

    #[test]
    fn spots_sit_in_the_corners_and_the_middles_with_an_inset_that_shrinks_in_small_pictures() {
        // 1000 wide picture, 200 wide PIP: start, centre, end.
        assert_eq!(pip_at(0.0, 1000.0, 200.0, 0), PIP_INSET);
        assert_eq!(pip_at(0.0, 1000.0, 200.0, 1), 400.0);
        assert_eq!(pip_at(0.0, 1000.0, 200.0, 2), 1000.0 - 200.0 - PIP_INSET);
        // Barely bigger than the PIP: the inset is half the slack, never negative.
        assert_eq!(pip_at(0.0, 210.0, 200.0, 0), 5.0);
        assert_eq!(pip_at(0.0, 210.0, 200.0, 2), 5.0);
        assert_eq!(pip_at(50.0, 100.0, 100.0, 0), 50.0);
        assert_eq!(pip_at(50.0, 100.0, 150.0, 2), 0.0, "a PIP bigger than the picture hangs over, inset 0");
    }

    /// D1: a PIP starts bottom right, a quarter of the picture wide, 12 px from both edges.
    #[test]
    fn a_new_pip_is_bottom_right_a_quarter_wide() {
        let g = layout_pip(1000.0, 1000.0 / AR, AR, CAM, false, &PipPos::default());
        assert!((g.rect.width() - 240.0).abs() < 1e-3);
        assert!((g.rect.height() - 240.0 / CAM).abs() < 1e-3);
        assert!((g.vis.max.x - g.rect.max.x - PIP_INSET).abs() < 1e-3 && (g.vis.max.y - g.rect.max.y - PIP_INSET).abs() < 1e-3);
        assert_eq!(g.corner, Corner { right: false, bottom: false }, "bottom right: the handle faces the middle, top left");
    }

    /// D1: "stays on its spot across layouts, tile resizes and fullscreen": the same stored position puts the PIP
    /// on the same spot (the same inset from the same edges) in every tile size, in Crop as in Split.
    #[test]
    fn a_spot_survives_tile_resizes_and_layouts() {
        for spot in [(0u8, 0u8), (1, 0), (2, 0), (0, 1), (2, 1), (0, 2), (1, 2), (2, 2)] {
            let p = PipPos { spot: Some(spot), ..PipPos::default() };
            for (tw, th, crop) in [(1000.0, 1000.0 / AR, false), (400.0, 400.0 / AR, false), (500.0, 700.0, true), (1900.0, 1000.0, true), (1920.0, 1080.0, false)] {
                let g = layout_pip(tw, th, AR, CAM, crop, &p);
                let (cx, cy) = (g.rect.center().x, g.rect.center().y);
                let (vcx, vcy) = (g.vis.center().x, g.vis.center().y);
                match spot.0 {
                    0 => assert!((g.rect.min.x - g.vis.min.x - PIP_INSET).abs() < 1e-3, "{spot:?} {tw}x{th}"),
                    1 => assert!((cx - vcx).abs() < 1e-3),
                    _ => assert!((g.vis.max.x - g.rect.max.x - PIP_INSET).abs() < 1e-3),
                }
                match spot.1 {
                    0 => assert!((g.rect.min.y - g.vis.min.y - PIP_INSET).abs() < 1e-3, "{spot:?} {tw}x{th}"),
                    1 => assert!((cy - vcy).abs() < 1e-3),
                    _ => assert!((g.vis.max.y - g.rect.max.y - PIP_INSET).abs() < 1e-3),
                }
                assert!(inside(g.rect, g.vis), "{spot:?} {tw}x{th} crop={crop}: {:?} not in {:?}", g.rect, g.vis);
            }
        }
    }

    /// D1: "stays inside the visible picture, in Crop too". A 16:9 stream in a tall Crop cell is cropped to its
    /// centre: the picture is wider than the tile, the PIP stays in the part you see, and the stored spot is the
    /// same one Split would use.
    #[test]
    fn in_crop_the_pip_stays_in_the_visible_centre_and_the_stored_value_is_kept() {
        let p = PipPos::default();
        let g = layout_pip(400.0, 700.0, AR, CAM, true, &p);
        assert!(g.pic.width() > 400.0 + 1.0, "the picture is cropped at the sides: {:?}", g.pic);
        assert_eq!(g.vis, Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 700.0)));
        assert!(inside(g.rect, g.vis));
        // Width is a fraction of the *picture*, capped by what shows.
        assert!(g.rect.width() <= 400.0 + 1e-3);
        // Back in Split the same stored position gives the unscaled corner again.
        let split = layout_pip(1000.0, 1000.0 / AR, AR, CAM, false, &p);
        assert!((split.vis.max.x - split.rect.max.x - PIP_INSET).abs() < 1e-3);
    }

    #[test]
    fn a_pip_is_never_smaller_than_the_minimum_nor_bigger_than_what_shows() {
        let tiny = layout_pip(200.0, 200.0 / AR, AR, CAM, false, &PipPos { w: 0.01, ..PipPos::default() });
        assert!((tiny.rect.width() - PIP_MIN).abs() < 1e-3);
        let huge = layout_pip(200.0, 200.0 / AR, AR, CAM, false, &PipPos { w: 5.0, ..PipPos::default() });
        assert!((huge.rect.width() - 200.0).abs() < 1e-3, "no bigger than the visible width");
        // A tile smaller than the minimum: the PIP fills what shows, still inside it.
        let small = layout_pip(60.0, 60.0 / AR, AR, CAM, false, &PipPos::default());
        assert!(inside(small.rect, small.vis));
    }

    /// D1: "snaps on release to the nearest of 8 spots".
    #[test]
    fn a_released_pip_goes_to_the_nearest_of_the_eight_spots() {
        let (tw, th) = (1000.0, 1000.0 / AR);
        let base = layout_pip(tw, th, AR, CAM, false, &PipPos::default());
        // Drag from the default bottom-right to several places and release.
        let cases = [
            (vec2(-2000.0, -2000.0), (0, 0)),
            (vec2(-300.0, -2000.0), (1, 0)),
            (vec2(2000.0, -2000.0), (2, 0)),
            (vec2(-2000.0, -200.0), (0, 1)),
            (vec2(0.0, -200.0), (2, 1)),
            (vec2(-2000.0, 2000.0), (0, 2)),
            (vec2(-380.0, 2000.0), (1, 2)),
            (vec2(5.0, 5.0), (2, 2)),
        ];
        for (d, want) in cases {
            let mut p = PipPos::default();
            move_pip(&base, d, &mut p);
            assert_eq!(p.spot, None, "free during the drag");
            let g = layout_pip(tw, th, AR, CAM, false, &p);
            snap_pip(&g, &mut p);
            assert_eq!(p.spot, Some(want), "dragged by {d:?}");
            assert!(inside(layout_pip(tw, th, AR, CAM, false, &p).rect, g.vis));
        }
        // A click (no drag) leaves the spot alone.
        let mut p = PipPos { spot: Some((0, 1)), ..PipPos::default() };
        snap_pip(&layout_pip(tw, th, AR, CAM, false, &p), &mut p);
        assert_eq!(p.spot, Some((0, 1)));
        // The middle of the picture is no spot: a PIP dropped there goes to a side or corner.
        let mut p = PipPos::default();
        let centre = (base.vis.center() - base.rect.center()) + vec2(0.0, 0.0);
        move_pip(&base, centre, &mut p);
        snap_pip(&layout_pip(tw, th, AR, CAM, false, &p), &mut p);
        assert_ne!(p.spot, Some((1, 1)));
    }

    /// D1: "a drag past an edge stays inside the visible picture, in Crop too".
    #[test]
    fn a_drag_stops_at_the_edge_of_the_visible_picture() {
        for (tw, th, crop) in [(1000.0, 1000.0 / AR, false), (400.0, 700.0, true), (1900.0, 600.0, true)] {
            let base = layout_pip(tw, th, AR, CAM, crop, &PipPos::default());
            for d in [vec2(-9999.0, -9999.0), vec2(9999.0, 9999.0), vec2(-9999.0, 9999.0), vec2(37.0, -11.0)] {
                let mut p = PipPos::default();
                move_pip(&base, d, &mut p);
                let g = layout_pip(tw, th, AR, CAM, crop, &p);
                assert!(inside(g.rect, g.vis), "{tw}x{th} crop={crop} d={d:?}: {:?} outside {:?}", g.rect, g.vis);
                assert!((g.rect.size() - base.rect.size()).length() < 1e-2, "a move never resizes");
            }
        }
        // A small move is followed one to one.
        let base = layout_pip(1000.0, 1000.0 / AR, AR, CAM, false, &PipPos::default());
        let mut p = PipPos::default();
        move_pip(&base, vec2(-40.0, -25.0), &mut p);
        let g = layout_pip(1000.0, 1000.0 / AR, AR, CAM, false, &p);
        assert!((g.rect.min - base.rect.min - vec2(-40.0, -25.0)).length() < 1e-2);
    }

    /// D1: "a resize keeps the aspect"; the corner opposite the handle stays; it stays inside; the minimum holds.
    #[test]
    fn a_resize_keeps_the_aspect_and_the_opposite_corner() {
        for (tw, th, crop) in [(1000.0, 1000.0 / AR, false), (500.0, 700.0, true)] {
            let base = layout_pip(tw, th, AR, CAM, crop, &PipPos::default());
            let c = base.corner;
            assert_eq!(c, Corner { right: false, bottom: false });
            let fixed = base.rect.max;
            // Pulling the handle up and left grows the PIP, down and right shrinks it.
            let mut grown = PipPos::default();
            resize_pip(&base, c, vec2(-80.0, -45.0), &mut grown);
            let g = layout_pip(tw, th, AR, CAM, crop, &grown);
            assert!(g.rect.width() > base.rect.width() + 20.0, "{crop}: grows");
            assert!((g.rect.width() / g.rect.height() - CAM).abs() < 1e-3, "aspect holds");
            assert!(inside(g.rect, g.vis));
            assert!((g.rect.max - fixed).length() < 1.0, "the corner opposite the handle stays: {:?} vs {fixed:?}", g.rect.max);
            let mut shrunk = PipPos::default();
            resize_pip(&base, c, vec2(500.0, 500.0), &mut shrunk);
            let g = layout_pip(tw, th, AR, CAM, crop, &shrunk);
            assert!((g.rect.width() - PIP_MIN.min(g.vis.width())).abs() < 1e-2, "down to the minimum, not below");
            // Far past the picture: capped by what shows.
            let mut huge = PipPos::default();
            resize_pip(&base, c, vec2(-9999.0, -9999.0), &mut huge);
            let g = layout_pip(tw, th, AR, CAM, crop, &huge);
            assert!(inside(g.rect, g.vis) && g.rect.width() <= g.vis.width() + 1e-3);
            assert_eq!(huge.spot, Some((2, 2)), "a resize leaves the spot");
        }
    }

    /// The handle is on the corner facing the middle, whichever spot the PIP is on.
    #[test]
    fn the_handle_faces_the_middle_of_the_picture() {
        let at = |s: (u8, u8)| layout_pip(1000.0, 1000.0 / AR, AR, CAM, false, &PipPos { spot: Some(s), ..PipPos::default() }).corner;
        assert_eq!(at((0, 0)), Corner { right: true, bottom: true });
        assert_eq!(at((2, 0)), Corner { right: false, bottom: true });
        assert_eq!(at((0, 2)), Corner { right: true, bottom: false });
        assert_eq!(at((2, 2)), Corner { right: false, bottom: false });
    }
}
