//! Slot layout: [`HBox`] rows of fixed + fill slots, equal-width columns, grid column
//! math, and the debug slot overlay. See the module doc of `widgets` for the model.

use egui::{Align, Color32, FontId, Id, Layout, Rect, Response, Sense, Stroke, StrokeKind, Ui, UiBuilder, pos2, vec2};

use crate::theme;

/// One cell of an [`HBox`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slot {
    /// A fixed-width slot (`add_sized` / `allocate_exact_size` in the spec).
    Fixed(f32),
    /// Takes the remaining width; several fills share it equally.
    Fill,
}

/// Shorthand for [`Slot::Fixed`].
pub const fn fixed(w: f32) -> Slot {
    Slot::Fixed(w)
}

/// Shorthand for [`Slot::Fill`].
pub const FILL: Slot = Slot::Fill;

/// Pure slot math: the `(x offset, width)` of each slot in a row whose content box is
/// `width` wide, with `gap` between neighbouring slots. Fill slots share what the fixed
/// slots and gaps leave, never less than 0 (an overfull row overflows to the right and
/// is clipped by its cells, it never squeezes a fixed slot).
pub fn slot_spans(width: f32, gap: f32, slots: &[Slot]) -> Vec<(f32, f32)> {
    let n = slots.len();
    if n == 0 {
        return Vec::new();
    }
    let fixed_sum: f32 = slots.iter().map(|s| if let Slot::Fixed(w) = s { *w } else { 0.0 }).sum();
    let n_fill = slots.iter().filter(|s| matches!(s, Slot::Fill)).count();
    let free = (width - fixed_sum - gap * (n as f32 - 1.0)).max(0.0);
    let fill_w = if n_fill > 0 { free / n_fill as f32 } else { 0.0 };
    let mut x = 0.0;
    slots
        .iter()
        .map(|s| {
            let w = match s {
                Slot::Fixed(w) => *w,
                Slot::Fill => fill_w,
            };
            let span = (x, w);
            x += w + gap;
            span
        })
        .collect()
}

/// Builder for one hbox row. See the `widgets` module doc for the model.
#[must_use]
pub struct HBox<'a> {
    name: &'static str,
    height: f32,
    gap: f32,
    pad_l: f32,
    pad_r: f32,
    width: Option<f32>,
    slots: &'a [Slot],
    sense: Sense,
}

impl<'a> HBox<'a> {
    /// A row named `name` (shown by the debug overlay) of exactly `height` points.
    pub fn new(name: &'static str, height: f32) -> Self {
        Self { name, height, gap: theme::S2, pad_l: 0.0, pad_r: 0.0, width: None, slots: &[], sense: Sense::hover() }
    }
    /// Gap between slots (default `S2` = 8).
    pub fn gap(mut self, gap: f32) -> Self {
        self.gap = gap;
        self
    }
    /// Inner horizontal padding before the first and after the last slot.
    pub fn pad(mut self, left: f32, right: f32) -> Self {
        self.pad_l = left;
        self.pad_r = right;
        self
    }
    /// Row width (default: `ui.available_width()`).
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }
    /// The slots, left to right.
    pub fn slots(mut self, slots: &'a [Slot]) -> Self {
        self.slots = slots;
        self
    }
    /// What the whole row senses (default hover). `Sense::click()` makes the row a
    /// button; interactive widgets inside its cells still win their own clicks.
    pub fn sense(mut self, sense: Sense) -> Self {
        self.sense = sense;
        self
    }

    /// Allocate the row in `ui` and compute its cells.
    pub fn show(self, ui: &mut Ui) -> HRow {
        let avail = ui.available_width();
        let width = self.width.unwrap_or(if avail.is_finite() { avail } else { 0.0 });
        let (rect, response) = ui.allocate_exact_size(vec2(width, self.height), self.sense);
        let inner_w = (width - self.pad_l - self.pad_r).max(0.0);
        let cells: Vec<Rect> = slot_spans(inner_w, self.gap, self.slots)
            .into_iter()
            .map(|(x, w)| Rect::from_min_size(pos2(rect.left() + self.pad_l + x, rect.top()), vec2(w, self.height)))
            .collect();
        if debug_slots(ui.ctx()) {
            log_and_outline(ui, self.name, rect, &cells, self.slots);
        }
        HRow { rect, response, cells }
    }
}

/// An allocated hbox row: its rect, its response (per its sense) and its cell rects.
pub struct HRow {
    pub rect: Rect,
    pub response: Response,
    pub cells: Vec<Rect>,
}

impl HRow {
    /// Rect of cell `i`.
    pub fn cell(&self, i: usize) -> Rect {
        self.cells[i]
    }

    /// Run `add` in a child `Ui` confined (max rect + clip) to cell `i`, its content
    /// vertically centred and horizontally aligned by `align` (`Min` left, `Center`,
    /// `Max` right). `ui.available_width()` inside is the cell width.
    pub fn show<R>(&self, ui: &mut Ui, i: usize, align: Align, add: impl FnOnce(&mut Ui) -> R) -> R {
        let cell = self.cells[i];
        let layout = match align {
            Align::Min => Layout::left_to_right(Align::Center),
            Align::Center => Layout::left_to_right(Align::Center).with_main_align(Align::Center),
            Align::Max => Layout::right_to_left(Align::Center),
        };
        let mut child = ui.new_child(UiBuilder::new().id_salt((self.response.id, i)).max_rect(cell).layout(layout));
        child.shrink_clip_rect(cell);
        add(&mut child)
    }

    /// Paint single-line `text` into cell `i` (truncated with "…" to the cell width),
    /// vertically centred, aligned by `align`.
    pub fn text(&self, ui: &Ui, i: usize, text: &str, font: FontId, color: Color32, align: Align) {
        super::paint_text(ui, self.cells[i], text, font, color, align);
    }

    /// Paint an icon centred in cell `i`.
    pub fn icon(&self, ui: &Ui, i: usize, name: &str, size: f32, color: Color32) {
        super::paint_icon(ui, self.cells[i], name, size, color);
    }
}

/// Equal-width contained columns (the Forza guide's `theme::columns`): `n` columns of
/// `(avail − gap·(n−1)) / n`, each a child `Ui` clipped to its column; the parent
/// advances by the tallest. Use for page splits, e.g. the stream-settings modal body.
pub fn columns<R>(ui: &mut Ui, n: usize, gap: f32, add: impl FnOnce(&mut [Ui]) -> R) -> R {
    let n = n.max(1);
    let total = ui.available_width().max(0.0);
    let slots = vec![Slot::Fill; n];
    let spans = slot_spans(total, gap, &slots);
    let top_left = ui.cursor().min;
    let bottom = ui.max_rect().bottom().max(top_left.y);
    let salt = ui.next_auto_id();
    let mut cols: Vec<Ui> = spans
        .iter()
        .enumerate()
        .map(|(i, (x, w))| {
            let rect = Rect::from_min_max(pos2(top_left.x + x, top_left.y), pos2(top_left.x + x + w, bottom));
            let mut child = ui.new_child(UiBuilder::new().id_salt((salt, "col", i)).max_rect(rect).layout(Layout::top_down(Align::Min)));
            child.set_width(*w);
            // Clip horizontally only (half the gap of slack each side for hover washes);
            // vertically the parent's clip stands — `max_rect` is not the content height
            // inside a ScrollArea.
            let x = rect.x_range().expand(gap * 0.5);
            let y = child.clip_rect().y_range();
            child.shrink_clip_rect(Rect::from_x_y_ranges(x, y));
            child
        })
        .collect();
    let r = add(&mut cols);
    let h = cols.iter().map(|c| c.min_rect().bottom() - top_left.y).fold(0.0_f32, f32::max);
    let rect = Rect::from_min_size(top_left, vec2(total, h));
    if debug_slots(ui.ctx()) {
        let cells: Vec<Rect> = cols.iter().map(|c| Rect::from_x_y_ranges(c.max_rect().x_range(), rect.y_range())).collect();
        log_and_outline(ui, "columns", rect, &cells, &slots);
    }
    ui.advance_cursor_after_rect(rect);
    r
}

/// Card/tile grid column count for an available width:
/// `clamp(floor((avail + GRID_GAP) / GRID_PITCH), 1, GRID_MAX_COLS)` — i.e. every column
/// is at least `GRID_PITCH − GRID_GAP` = 260 wide.
pub fn grid_cols(avail: f32) -> usize {
    (((avail + theme::GRID_GAP) / theme::GRID_PITCH).floor().max(1.0) as usize).min(theme::GRID_MAX_COLS)
}

/// [`grid_cols`] with hysteresis keyed by `id`: the count only changes once the width
/// has moved more than 4 points past the threshold, so a scrollbar appearing (which
/// narrows the area, which could drop a column, which shortens the content, which hides
/// the scrollbar…) can never make the grid oscillate between frames.
pub fn grid_cols_stable(ui: &Ui, id: Id, avail: f32) -> usize {
    const HYST: f32 = 4.0;
    let prev: Option<usize> = ui.ctx().data(|d| d.get_temp(id));
    let raw = grid_cols(avail);
    let cols = match prev {
        Some(p) if p != raw => {
            // Only switch if the width is clearly on the other side of the boundary.
            if grid_cols(avail - HYST) == raw && grid_cols(avail + HYST) == raw { raw } else { p }
        }
        _ => raw,
    };
    ui.ctx().data_mut(|d| d.insert_temp(id, cols));
    cols
}

// ---------------------------------------------------------------------------
// Debug overlay + slot log
// ---------------------------------------------------------------------------

/// One logged slot (only recorded while [`set_debug_slots`] is on).
#[derive(Clone, Debug)]
pub struct LoggedSlot {
    /// Row name given to [`HBox::new`] (e.g. `"ctrl_row"`).
    pub row: &'static str,
    /// The whole row rect.
    pub row_rect: Rect,
    /// Slot index within the row.
    pub index: usize,
    /// Slot rect.
    pub rect: Rect,
    /// Whether the slot is fixed (else fill).
    pub fixed: bool,
}

#[derive(Clone, Default)]
struct SlotLog {
    pass: u64,
    slots: Vec<LoggedSlot>,
}

fn debug_id() -> Id {
    Id::new("widgets::debug_slots")
}
fn log_id() -> Id {
    Id::new("widgets::slot_log")
}

/// Turn the slot outline overlay (and slot logging) on or off for `ctx`.
pub fn set_debug_slots(ctx: &egui::Context, on: bool) {
    ctx.data_mut(|d| d.insert_temp(debug_id(), on));
}

/// Whether the slot overlay is on.
pub fn debug_slots(ctx: &egui::Context) -> bool {
    ctx.data(|d| d.get_temp(debug_id()).unwrap_or(false))
}

/// Slots logged during the current pass (empty unless [`set_debug_slots`] is on).
pub fn slot_log(ctx: &egui::Context) -> Vec<LoggedSlot> {
    ctx.data(|d| d.get_temp::<SlotLog>(log_id())).map(|l| l.slots).unwrap_or_default()
}

/// Outline a vbox rect in the debug overlay (no-op when it's off). Containers call it.
pub fn debug_vbox(ui: &Ui, rect: Rect) {
    if debug_slots(ui.ctx()) {
        ui.ctx().debug_painter().rect_stroke(rect, 0.0, Stroke::new(1.0_f32, theme::OV_VBOX), StrokeKind::Inside);
    }
}

fn log_and_outline(ui: &Ui, name: &'static str, rect: Rect, cells: &[Rect], slots: &[Slot]) {
    let ctx = ui.ctx();
    let pass = ctx.cumulative_pass_nr();
    ctx.data_mut(|d| {
        let log = d.get_temp_mut_or_default::<SlotLog>(log_id());
        if log.pass != pass {
            log.pass = pass;
            log.slots.clear();
        }
        for (i, (c, s)) in cells.iter().zip(slots).enumerate() {
            log.slots.push(LoggedSlot { row: name, row_rect: rect, index: i, rect: *c, fixed: matches!(s, Slot::Fixed(_)) });
        }
    });
    let p = ctx.debug_painter().with_clip_rect(ui.clip_rect());
    p.rect_stroke(rect, 0.0, Stroke::new(1.0_f32, theme::OV_HBOX), StrokeKind::Inside);
    for (c, s) in cells.iter().zip(slots) {
        let col = if matches!(s, Slot::Fixed(_)) { theme::OV_SLOT } else { theme::OV_FILL };
        p.rect_stroke(c.shrink(1.0), 0.0, Stroke::new(1.0_f32, col), StrokeKind::Inside);
    }
}
