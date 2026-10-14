//! Painting helpers the widgets share: single-line truncated text, icons, CSS-like
//! linear gradients, 45° hatching, dashed outlines and glows. They only paint into a
//! rect the caller already allocated; none of them allocate space.

use std::sync::Arc;

use egui::epaint::{CornerRadiusF32, Mesh, RectShape, TessellationOptions, Tessellator};
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Align, Color32, CornerRadius, FontId, Galley, Painter, Pos2, Rect, Shape, Stroke, Ui, pos2, vec2};

use crate::theme;

/// Lay out `text` on one line in `font`/`color`, truncated with "…" to `max_w`
/// (`f32::INFINITY` = never truncate), with `tracking` extra points between letters
/// (CSS `letter-spacing`).
pub fn text_galley(ui: &Ui, text: &str, font: FontId, color: Color32, max_w: f32, tracking: f32) -> Arc<Galley> {
    let mut job = LayoutJob::single_section(
        text.to_owned(),
        TextFormat { font_id: font, color, extra_letter_spacing: tracking, ..Default::default() },
    );
    job.wrap = TextWrapping { max_width: max_w.max(0.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

/// Width `text` needs on one line (no truncation).
pub fn text_width(ui: &Ui, text: &str, font: FontId, tracking: f32) -> f32 {
    text_galley(ui, text, font, Color32::WHITE, f32::INFINITY, tracking).size().x
}

/// Paint `galley` vertically centred in `rect`, horizontally aligned by `align`.
/// Returns the galley's rect.
pub fn paint_galley(ui: &Ui, rect: Rect, galley: Arc<Galley>, align: Align) -> Rect {
    let size = galley.size();
    let x = match align {
        Align::Min => rect.left(),
        Align::Center => rect.center().x - size.x / 2.0,
        Align::Max => rect.right() - size.x,
    };
    let pos = pos2(x, rect.center().y - size.y / 2.0);
    // Clip horizontally to the slot (vertically keep the line box): text never paints
    // into a neighbouring slot, even the "…" of a zero-width fill.
    let clip = Rect::from_x_y_ranges(rect.x_range(), (pos.y - 2.0)..=(pos.y + size.y + 2.0)).intersect(ui.clip_rect());
    ui.painter().with_clip_rect(clip).galley(pos, galley, Color32::PLACEHOLDER);
    Rect::from_min_size(pos, size)
}

/// Paint single-line `text`, truncated to `rect`'s width, vertically centred.
pub fn paint_text(ui: &Ui, rect: Rect, text: &str, font: FontId, color: Color32, align: Align) -> Rect {
    let g = text_galley(ui, text, font, color, rect.width(), 0.0);
    paint_galley(ui, rect, g, align)
}

/// Paint Material Symbols icon `name` at `size`, centred in `rect`.
pub fn paint_icon(ui: &Ui, rect: Rect, name: &str, size: f32, color: Color32) {
    let g = ui.painter().layout_no_wrap(theme::icon(name), theme::icon_font(size), color);
    paint_galley(ui, rect, g, Align::Center);
}

/// Paint `rect` (rounded by `radius`) with a CSS `linear-gradient(<angle_deg>, from, to)`:
/// 180° runs top→bottom, 90° left→right, 140° towards the bottom-right. Implemented by
/// tessellating the rounded rect and colouring each vertex by its position along the
/// gradient axis, so the corners stay anti-aliased.
pub fn paint_gradient(painter: &Painter, rect: Rect, radius: CornerRadius, angle_deg: f32, from: Color32, to: Color32) {
    let mut mesh = Mesh::default();
    let ppp = painter.ctx().pixels_per_point();
    let mut tess = Tessellator::new(ppp, TessellationOptions::default(), [1, 1], vec![]);
    tess.tessellate_rect(&RectShape::filled(rect, radius, Color32::WHITE), &mut mesh);
    let a = angle_deg.to_radians();
    let dir = vec2(a.sin(), -a.cos());
    // CSS gradient line length: the projection of the box onto the direction.
    let half = (rect.width() * dir.x.abs() + rect.height() * dir.y.abs()) / 2.0;
    for v in &mut mesh.vertices {
        let t = if half > 0.0 { ((v.pos - rect.center()).dot(dir) / (2.0 * half) + 0.5).clamp(0.0, 1.0) } else { 0.0 };
        let c = lerp_color(from, to, t);
        let k = v.color.a() as f32 / 255.0; // feathering alpha at the AA edge
        v.color = c.gamma_multiply(k);
    }
    painter.add(Shape::mesh(mesh));
}

/// Linear interpolation between two (premultiplied) colours.
pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color32::from_rgba_premultiplied(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()), l(a.a(), b.a()))
}

/// Fill a rounded `rect` with CSS `repeating-linear-gradient(45deg, a 0 w, b w 2w)`
/// stripes (the amber "degraded" hatch). Each stripe is clipped to the rounded outline
/// polygon, so nothing leaks past the rounded corners.
pub fn paint_hatch(painter: &Painter, rect: Rect, radius: CornerRadius, stripe_w: f32, a: Color32, b: Color32) {
    painter.rect_filled(rect, radius, b);
    let mut outline = Vec::new();
    egui::epaint::tessellator::path::rounded_rectangle(&mut outline, rect, CornerRadiusF32::from(radius));
    // The 45° gradient axis points up-right, so stripes run down-right (1, 1); the
    // period measured along the x axis is `2·w·√2`.
    let period = 2.0 * stripe_w * std::f32::consts::SQRT_2;
    let band = stripe_w * std::f32::consts::SQRT_2;
    let h = rect.height();
    let mut x = rect.left() - h - period;
    while x < rect.right() + period {
        let quad = vec![pos2(x, rect.top()), pos2(x + band, rect.top()), pos2(x + band + h, rect.bottom()), pos2(x + h, rect.bottom())];
        let clipped = clip_convex(&quad, &outline);
        if clipped.len() >= 3 {
            painter.add(Shape::convex_polygon(clipped, a, Stroke::NONE));
        }
        x += period;
    }
}

/// Sutherland–Hodgman: clip convex `subject` by convex `clip` (either winding).
fn clip_convex(subject: &[Pos2], clip: &[Pos2]) -> Vec<Pos2> {
    if clip.len() < 3 {
        return subject.to_vec();
    }
    // Orientation of the clip polygon.
    let area: f32 = clip.iter().zip(clip.iter().cycle().skip(1)).map(|(p, q)| p.x * q.y - q.x * p.y).sum();
    let sign = if area >= 0.0 { 1.0 } else { -1.0 };
    let mut out = subject.to_vec();
    for i in 0..clip.len() {
        let (a, b) = (clip[i], clip[(i + 1) % clip.len()]);
        let inside = |p: Pos2| sign * ((b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x)) >= 0.0;
        let input = std::mem::take(&mut out);
        if input.is_empty() {
            break;
        }
        for j in 0..input.len() {
            let (p, q) = (input[j], input[(j + 1) % input.len()]);
            let (pin, qin) = (inside(p), inside(q));
            if pin {
                out.push(p);
            }
            if pin != qin {
                let d1 = (b.x - a.x) * (p.y - a.y) - (b.y - a.y) * (p.x - a.x);
                let d2 = (b.x - a.x) * (q.y - a.y) - (b.y - a.y) * (q.x - a.x);
                let t = d1 / (d1 - d2);
                out.push(p + (q - p) * t);
            }
        }
    }
    out
}

/// A dashed outline around a rounded `rect` (CSS `border: 1px dashed`).
pub fn paint_dashed_rect(painter: &Painter, rect: Rect, radius: CornerRadius, stroke: Stroke, dash: f32, gap: f32) {
    let mut path = Vec::new();
    let r = rect.shrink(stroke.width / 2.0);
    egui::epaint::tessellator::path::rounded_rectangle(&mut path, r, CornerRadiusF32::from(radius));
    if let Some(first) = path.first().copied() {
        path.push(first);
    }
    painter.extend(Shape::dashed_line(&path, stroke, dash, gap));
}

/// A soft drop glow under `rect` (CSS `box-shadow: x y blur color`).
pub fn paint_glow(painter: &Painter, rect: Rect, radius: CornerRadius, offset_y: i8, blur: u8, color: Color32) {
    let shadow = egui::Shadow { offset: [0, offset_y], blur, spread: 0, color };
    painter.add(shadow.as_shape(rect, radius));
}

/// Multiply a colour's opacity (for disabled widgets: CSS `opacity: .45`).
pub fn fade(c: Color32, enabled: bool) -> Color32 {
    if enabled { c } else { c.gamma_multiply(DISABLED_OPACITY) }
}

/// CSS `:disabled { opacity: .45 }`.
pub const DISABLED_OPACITY: f32 = 0.45;
