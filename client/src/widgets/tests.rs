//! Headless layout tests: run widgets in an `egui::Context` without a window and check
//! the slot rects the debug log records.

use egui::{CentralPanel, Frame, Pos2, RawInput, Rect, Ui, vec2};

use super::*;
use crate::theme;

/// Run `f` in a frameless central panel `width` wide and return the logged slots.
fn run_ui(width: f32, mut f: impl FnMut(&mut Ui)) -> Vec<LoggedSlot> {
    let ctx = egui::Context::default();
    theme::apply(&ctx);
    set_debug_slots(&ctx, true);
    let mut log = Vec::new();
    for _ in 0..2 {
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(width, 900.0))), ..Default::default() };
        let _ = ctx.run(input, |ctx| {
            CentralPanel::default().frame(Frame::NONE).show(ctx, |ui| {
                f(ui);
                log = slot_log(ctx);
            });
        });
    }
    log
}

fn rows<'a>(log: &'a [LoggedSlot], name: &str) -> Vec<Vec<&'a LoggedSlot>> {
    let mut out: Vec<Vec<&LoggedSlot>> = Vec::new();
    for s in log.iter().filter(|s| s.row == name) {
        if s.index == 0 {
            out.push(Vec::new());
        }
        out.last_mut().unwrap().push(s);
    }
    out
}

#[test]
fn slot_spans_fill_between_fixed() {
    let s = slot_spans(400.0, 8.0, &[fixed(120.0), FILL, fixed(72.0)]);
    assert_eq!(s, vec![(0.0, 120.0), (128.0, 192.0), (328.0, 72.0)]);
    // Two fills share equally.
    let s = slot_spans(208.0, 8.0, &[FILL, FILL]);
    assert_eq!(s, vec![(0.0, 100.0), (108.0, 100.0)]);
    // Overfull: fill collapses to 0, fixed slots keep their width.
    let s = slot_spans(100.0, 8.0, &[fixed(120.0), FILL, fixed(72.0)]);
    assert_eq!(s[1].1, 0.0);
    assert_eq!(s[2].1, 72.0);
}

#[test]
fn grid_column_count() {
    assert_eq!(grid_cols(100.0), 1);
    assert_eq!(grid_cols(259.0), 1);
    assert_eq!(grid_cols(260.0), 1);
    assert_eq!(grid_cols(532.0), 2);
    assert_eq!(grid_cols(531.0), 1);
    assert_eq!(grid_cols(804.0), 3);
    assert_eq!(grid_cols(1076.0), 4);
    assert_eq!(grid_cols(3000.0), 4);
}

#[test]
fn grid_count_has_hysteresis() {
    let ctx = egui::Context::default();
    let id = egui::Id::new("g");
    let mut seen = Vec::new();
    for w in [600.0, 533.0, 531.0, 529.0, 527.0, 531.0, 535.0, 537.0] {
        let _ = ctx.run(RawInput::default(), |ctx| {
            CentralPanel::default().show(ctx, |ui| seen.push(grid_cols_stable(ui, id, w)));
        });
    }
    // Starts at 2, holds 2 until clearly below 532 (−4), then holds 1 until clearly above.
    assert_eq!(seen, vec![2, 2, 2, 2, 1, 1, 1, 2]);
}

#[test]
fn ctrl_row_slots_are_fixed_at_any_width() {
    for width in [300.0, 700.0] {
        let log = run_ui(width, |ui| {
            let mut v = 5u32;
            ctrl_row_value(ui, "Bitrate", None, |ui| readout(ui, "x", egui::Align::Max), |ui| value_box(ui, "6000"));
            slider_row(ui, "Total maximum", Some("tip"), &mut v, 0..=10, 1.0, 0, "");
            ctrl_row(ui, "Codec", None, |ui| ui.label("c"));
        });
        let rs = rows(&log, "ctrl_row");
        assert_eq!(rs.len(), 3, "width {width}");
        for r in &rs {
            assert_eq!(r[0].rect.width(), theme::LABEL_W, "label slot at width {width}");
            assert_eq!(r[0].rect.left(), 0.0);
            assert_eq!(r[0].row_rect.height(), theme::CTL_H);
            assert_eq!(r[1].rect.left(), theme::LABEL_W + theme::S2, "control starts after label + gap");
        }
        for r in &rs[..2] {
            assert_eq!(r.len(), 3);
            assert_eq!(r[2].rect.width(), theme::VALUE_W, "value slot at width {width}");
            assert_eq!(r[2].rect.right(), width, "value slot is right-bound");
            assert_eq!(r[1].rect.width(), width - theme::LABEL_W - theme::VALUE_W - 2.0 * theme::S2);
        }
        // The 2-slot row's fill reaches the right edge.
        assert_eq!(rs[2][1].rect.right(), width);
    }
}

#[test]
fn rows_of_a_type_align_and_share_height() {
    for width in [theme::SIDEBAR_W - 2.0 * theme::S3 - 2.0 * theme::S1, 900.0] {
        let log = run_ui(width, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for (name, num) in [("Movie Night", Some(3)), ("A much longer room name that truncates", Some(12)), ("Old", None)] {
                room_row(
                    ui,
                    &RoomRow { lead: RoomLead::Dot(DotState::Joined), name, live: LiveSlot::Off, num, unit: Unit::None, selected: false },
                );
                sharer_row(ui, &SharerRow { icon: "cast", tone: IconTone::Live, name, num: Some(240), unit: "fps" });
                person_row(ui, &PersonRow { name, colors: theme::avatar_colors(name), you: None, live: true, live_label: "Live", watching: true, idle: false, stale: false });
                dest_row(ui, &DestRow { current: false, name, note: "this room", remove_tip: "Stop", pill: Pill::live("Live"), kbps: "6000 kbps", removable: true });
                pop_item(ui, &PopItem::new("speaker", name).meter(2));
            }
        });
        for (name, h) in [
            ("room_row", theme::ROW_H),
            ("sharer_row", theme::SUBROW_H),
            ("person_row", theme::ROW_H),
            ("dest_row", theme::DEST_ROW_H),
            ("pop_item", theme::ROW_H),
        ] {
            let rs = rows(&log, name);
            assert_eq!(rs.len(), 3, "{name}");
            for r in &rs {
                assert_eq!(r[0].row_rect.height(), h, "{name} height");
                assert_eq!(r[0].row_rect.width(), width, "{name} width");
                for (a, b) in r.iter().zip(&rs[0]) {
                    assert_eq!(a.rect.x_range(), b.rect.x_range(), "{name} slot {} misaligned", a.index);
                }
            }
        }
        // Room row number/unit slots are right-bound: [.. 28][22] then 8 px padding.
        let rr = &rows(&log, "room_row")[0];
        assert_eq!(rr[4].rect.right(), width - theme::S2);
        assert_eq!(rr[3].rect.width(), 28.0);
        // Sharer number column lines up with the room number column.
        let sr = &rows(&log, "sharer_row")[0];
        assert_eq!(sr[3].rect.x_range(), rr[3].rect.x_range());
    }
}

#[test]
fn btn_row_buttons_are_equal_width() {
    let log = run_ui(500.0, |ui| {
        btn_row(ui, &[Btn::secondary("Also send to…").icon("add"), Btn::secondary("Move").icon("swap_horiz")]);
        btn_row(ui, &[Btn::primary("A"), Btn::secondary("B"), Btn::danger("C")]);
    });
    let rs = rows(&log, "btn_row");
    assert_eq!(rs[0][0].rect.width(), rs[0][1].rect.width());
    assert_eq!(rs[0][0].rect.width(), (500.0 - 8.0) / 2.0);
    assert!((rs[1][0].rect.width() - (500.0 - 16.0) / 3.0).abs() < 1e-3);
}

#[test]
fn grid_rows_pad_short_rows() {
    let log = run_ui(900.0, |ui| {
        grid_rows(ui, "g", 5, |ui, _| {
            ui.allocate_exact_size(vec2(ui.available_width(), 40.0), egui::Sense::hover());
        });
    });
    let rs = rows(&log, "columns");
    // 900 → 3 columns; 5 items → 2 rows, second padded with an empty column.
    assert_eq!(rs.len(), 2);
    assert!(rs.iter().all(|r| r.len() == 3));
    let w = rs[0][0].rect.width();
    assert!(rs.iter().flatten().all(|s| (s.rect.width() - w).abs() < 1e-3));
}

/// Offset (physical px) of the tessellated initial's glyph quad centre from the
/// tessellated disc's centre, for an avatar of `size` whose rect starts at a
/// fractional position `frac`, at `ppp` pixels per point.
fn avatar_ink_offset(ppp: f32, size: f32, frac: f32, initial: &str, old: bool) -> (f32, f32) {
    use egui::epaint::{Primitive, WHITE_UV};
    let ctx = egui::Context::default();
    theme::apply(&ctx);
    ctx.set_pixels_per_point(ppp);
    let mut out = None;
    for _ in 0..2 {
        let input = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(200.0, 200.0))), ..Default::default() };
        out = Some(ctx.run(input, |ctx| {
            CentralPanel::default().frame(Frame::NONE).show(ctx, |ui| {
                let r = Rect::from_min_size(egui::pos2(10.0 + frac, 20.0 + frac), vec2(size, size));
                if old {
                    // The pre-#83 placement: line box centred by `paint_text`.
                    ui.painter().rect_filled(r, egui::CornerRadius::same((size / 2.0) as u8), egui::Color32::RED);
                    let fs = if size <= theme::AVATAR_SM { 10.5 } else { 13.0 };
                    paint_text(ui, r, initial, theme::font(theme::Weight::Bold, fs), egui::Color32::WHITE, egui::Align::Center);
                } else {
                    paint_avatar(ui, r, initial, theme::avatar_colors("Ritze"), false);
                }
            });
        }));
    }
    let out = out.unwrap();
    let (mut disc, mut ink) = (Rect::NOTHING, Rect::NOTHING);
    for p in ctx.tessellate(out.shapes, out.pixels_per_point) {
        if let Primitive::Mesh(m) = p.primitive {
            for v in &m.vertices {
                if v.uv == WHITE_UV { disc.extend_with(v.pos) } else { ink.extend_with(v.pos) }
            }
        }
    }
    assert!(disc.is_positive() && ink.is_positive());
    let d = (ink.center() - disc.center()) * ppp;
    (d.x, d.y)
}

/// Task #83: the initial sits on the disc's centre, within the pixel that
/// whole-pixel text forces (vertically resolved upwards, never low), wherever
/// the avatar lands between pixels and at every scale factor. The old placement
/// (`paint_text` centring the line box, disc and text snapped independently)
/// moved by 1 px with the sub-pixel position (2.5 px for "Ä" at 1.25x).
#[test]
fn avatar_initial_is_centred_on_the_disc_at_any_position_and_scale() {
    let mut worst_old_spread: f32 = 0.0;
    for ppp in [1.0, 1.25, 1.5, 2.0] {
        for size in [theme::AVATAR, theme::AVATAR_SM] {
            for initial in ["R", "M", "W", "Ä", "G"] {
                let (mut lo, mut hi, mut olo, mut ohi) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
                for frac in [0.0, 0.25, 0.5, 0.75] {
                    let (dx, dy) = avatar_ink_offset(ppp, size, frac, initial, false);
                    assert!(dx.abs() <= 1.01 && dy <= 0.01 && dy > -1.01, "ppp {ppp} size {size} {initial:?} frac {frac}: ({dx},{dy})");
                    (lo, hi) = (lo.min(dy), hi.max(dy));
                    let (_, ody) = avatar_ink_offset(ppp, size, frac, initial, true);
                    (olo, ohi) = (olo.min(ody), ohi.max(ody));
                }
                assert!(hi - lo <= 0.51, "ppp {ppp} size {size} {initial:?}: dy spread {lo}..{hi}");
                worst_old_spread = worst_old_spread.max(ohi - olo);
            }
        }
    }
    // Guard the measurement itself: it does see the old drift.
    assert!(worst_old_spread >= 0.99, "old spread {worst_old_spread}");
}
