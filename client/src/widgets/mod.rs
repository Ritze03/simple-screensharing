//! Reusable egui building blocks for the new UI: layout slots, primitives, the row
//! patterns of the approved mockup, containers, and the `--gallery` screen.
//!
//! Every widget here is *pure view*: it takes plain data (`&str`, numbers, small enums
//! or `&mut` to the one value it edits) and returns a `Response` or a small action enum.
//! None of them touch app state. Look and size come from `crate::theme` tokens only.
//!
//! # Layout model: vbox / hbox / fixed slot / fill slot
//!
//! The UI is a tree of nested containers (spec: the plan's `ui-tree.md`):
//! - **vbox** = `ui.vertical` (or a card/well frame, which only paints + pads),
//! - **hbox** = a row built with [`HBox`],
//! - **slot** = a fixed-width cell of an hbox ([`Slot::Fixed`], `fixed(w)`),
//! - **fill** = the cell that takes the remaining width ([`Slot::Fill`], `FILL`).
//!
//! **egui has no flexbox**, so `ui.horizontal` cannot put a fill between fixed slots
//! (`[120 label][fill control][72 value]`): a horizontal layout hands each widget
//! whatever is left, so the fill would eat the value slot's space. [`HBox`] solves it
//! once with explicit width math instead of `StripBuilder` or right-to-left hacks:
//!
//! 1. it allocates the whole row up front — `available_width × height` — with
//!    `allocate_exact_size`, so the row's size never depends on its contents;
//! 2. [`slot_spans`] (pure, unit-tested) splits that width:
//!    `fill = (row_w − pad_l − pad_r − Σfixed − gap·(n−1)) / n_fill`, never below 0;
//! 3. each cell is drawn in its own child `Ui` whose `max_rect` *and clip rect* are the
//!    cell rect ([`HRow::show`]) — a widget can neither widen its cell nor paint into
//!    the next one; too-long text is truncated with "…" inside its slot.
//!
//! Consequences callers can rely on: rows of one type have identical heights, and
//! their slot x-positions line up across rows for any available width, because the
//! positions are a function of the row width only, never of the content.
//!
//! # How to use it
//!
//! Prefer the row helpers — each row *type* is exactly one function, so two rows of a
//! type cannot differ: [`ctrl_row`], [`ctrl_row_value`], [`slider_row`], [`room_row`],
//! [`sharer_row`], [`person_row`], [`dest_row`], [`pop_item`], [`btn_row`]; containers
//! [`card`], [`well`], [`grid_rows`], [`columns`], [`modal_shell`] / [`modal_frame`],
//! [`modal_footer`]. For a new row shape, build it the same way:
//!
//! ```ignore
//! let row = HBox::new("my_row", ROW_H).gap(S2).pad(S2, S2)
//!     .slots(&[fixed(18.0), FILL, fixed(72.0)])
//!     .sense(egui::Sense::click())
//!     .show(ui);                                   // allocates the row
//! row.show(ui, 0, Align::Center, |ui| status_dot(ui, DotState::Joined));
//! row.text(ui, 1, "Movie Night", font(Weight::Bold, FS_ROW), TEXT, Align::Min);
//! row.show(ui, 2, Align::Max, |ui| value_box(ui, "6000"));
//! ```
//!
//! Never hand-roll a row with `ui.horizontal` + `add_space` — that is exactly what
//! drifts. Vertical stacking is plain `ui.vertical` with `item_spacing.y` set to a token
//! gap (0/4/8/12/16).
//!
//! **Debug overlay:** [`set_debug_slots`] outlines every hbox (amber), fixed slot
//! (green) and fill slot (grey) like the mockup's "Show egui layout" toggle, and logs
//! their rects ([`slot_log`]) for tests / QC.

// This is the foundation API later screens are built on; parts of it have no caller
// until those screens land.
#![allow(dead_code)]

mod containers;
mod controls;
pub mod gallery;
mod layout;
mod paint;
mod rows;
#[cfg(test)]
mod tests;

pub use containers::*;
pub use controls::*;
pub use layout::*;
pub use paint::*;
pub use rows::*;
