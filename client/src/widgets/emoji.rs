//! Emoji avatars (task #106, decision D6/D1, any emoji since row 120): the embedded colour emoji font,
//! the colour logic that takes an avatar's background from its emoji, the balance point that centres
//! it, the baked outline, and the picker with its "Paste any emoji" field.
//!
//! egui's text path cannot draw colour emoji and the AppImage / Flatpak / Windows builds cannot rely
//! on a system emoji font, so Noto Color Emoji itself is embedded (`client/assets/emoji/`, OFL 1.1, see
//! its README). A string is shaped with rustybuzz (a flag, a skin tone, a ZWJ sequence or a keycap is a
//! GSUB ligature) and drawn from the glyph's CBDT bitmap. The string is a peer's to choose: anything
//! that is not one grapheme shaped to one bitmap (see [`is_drawable`]) has no picture and shows the
//! initial, like no emoji.
//!
//! Nothing of this runs per frame. An emoji is shaped, decoded and measured once ([`prepare`]), and
//! baked once per drawn size ([`bake`]) into one texture at device resolution; both stay in a cache
//! of at most [`MAX_CACHED`] emoji, so a peer cycling through strings cannot grow it.
//!
//! `Why` the balance point and not the crop's centre: the crop box's centre is the middle of the
//! shape's extremes, so a slice of pizza (nothing in its lower left) or a crescent sits visibly off
//! the disc with its box perfectly centred (the user's report on 🍕, row 120). The mass centre alone
//! overshoots on those (🌙, 🎸) and the smallest enclosing circle alone follows thin parts (🎮's
//! handles); their midpoint looked right on all 40 (contact sheet, `ui/STYLING-GUIDE.md`).
//!
//! `Why` the outline is baked: a half-size texture sampled down about 3 to 1 without mipmaps, drawn five times, gave
//! a broken stair-step line; dilating the alpha at the size it is shown gives the even line the
//! replica's `drop-shadow` chain has, in one quad.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use egui::{Align, Color32, Context, CornerRadius, Id, Pos2, Rect, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2, pos2, vec2};
use rustybuzz::ttf_parser::{GlyphId, RasterImageFormat};
use unicode_segmentation::UnicodeSegmentation;

use super::{PopItem, paint_text, pop_item, text_field};
use crate::theme::{self, Weight, font};

/// The picker's emoji, in picker order, 8 per row in three groups (rows 0 | 1-5 | 6, see `GROUP_ROWS`): the
/// user's own list (2026-11-05), the contract with the browser's `docs/js/ui/dom.js` `EMOJI`. Any other single
/// emoji still works as an avatar (the paste field).
pub const EMOJI: [&str; 56] = [
    "🤪", "🤤", "🤠", "🫪", "🤓", "🤔", "🥶", "😡",
    "🐒", "🦁", "🐷", "🐑", "🐹", "🦥", "🐼", "🐻‍❄️",
    "🐻", "🐿️", "🦫", "🦔", "🦒", "🐮", "🐱", "🫏",
    "🐔", "🐓", "🐥", "🐣", "🐦", "🐧", "🕊️", "🦜",
    "🐸", "🐢", "🦎", "🐍", "🐬", "🫍", "🦭", "🐙",
    "🦀", "🐌", "🐛", "🐜", "🐝", "🪰", "🪱", "🦤",
    "🥒", "🍟", "🍕", "🌭", "🥙", "🍿", "🍔", "🍝",
];
/// Rows after which the picker leaves a small gap (the end of a group).
const GROUP_ROWS: [usize; 2] = [1, 6];

/// Noto Color Emoji: CBDT bitmaps only, one 109 ppem strike. Parsed on first use.
static FONT: &[u8] = include_bytes!("../../assets/emoji/NotoColorEmoji.ttf");
static FACE: LazyLock<Option<rustybuzz::Face<'static>>> = LazyLock::new(|| rustybuzz::Face::from_slice(FONT, 0));

/// The wire's cap on an emoji in bytes (`protocol-spec.md` §9.2): a longer one cannot be sent, and
/// a longer one from a settings file is not looked at.
pub const EMOJI_MAX_BYTES: usize = 16;
/// Noto's bitmaps are the 109 ppem strike: a font size of `s` px draws a bitmap pixel `s / 109` px
/// wide. This is what makes `font-size: 0.62 x avatar` of the replica and our picture agree.
const EM_PX: f32 = 109.0;
/// The emoji's font size as a fraction of the avatar's size (replica `.avatar .emo`).
pub const EMOJI_AVATAR_FRACTION: f32 = 0.62;
/// The emoji's font size in the picker's cells (replica `.emoji-grid button`).
const PICKER_EMOJI_PX: f32 = 20.0;
/// A pixel belongs to the crop from this alpha on: a soft edge stays in the picture.
const VISIBLE_ALPHA: u8 = 16;
/// A pixel counts for the balance point from this alpha on: a soft shadow or glow does not move it.
const SOLID_ALPHA: u8 = 128;
/// Emoji kept by the cache (40 of the picker, a room's members); the least recently drawn goes first.
const MAX_CACHED: usize = 128;
/// Baked sizes kept per emoji. The app draws about five; a change of scale adds as many again.
const MAX_SIZES: usize = 8;

/// The bitmap (a PNG in the font) of `emoji`: `None` unless the string shapes to exactly one glyph
/// that has one. Default-ignorable leftovers (a variation selector) have no advance and do not count;
/// a ZWJ sequence the font has no ligature for stays two glyphs, so it has no picture.
fn bitmap(emoji: &str) -> Option<&'static [u8]> {
    let face = FACE.as_ref()?;
    let mut buffer = rustybuzz::UnicodeBuffer::new();
    buffer.push_str(emoji);
    buffer.guess_segment_properties();
    let shaped = rustybuzz::shape(face, &[], buffer);
    let mut visible = shaped.glyph_infos().iter().zip(shaped.glyph_positions()).filter(|(_, pos)| pos.x_advance != 0);
    let glyph = visible.next()?.0.glyph_id;
    if glyph == 0 || visible.next().is_some() {
        return None;
    }
    let image = face.glyph_raster_image(GlyphId(u16::try_from(glyph).ok()?), u16::MAX)?;
    (image.format == RasterImageFormat::PNG).then_some(image.data)
}

/// Whether `emoji` can be drawn, whatever its length: one grapheme that is not plain ASCII (the
/// font has bitmaps of the digits, `#` and `*` for its keycaps, none of them an emoji on their own)
/// with a bitmap.
pub fn is_drawable(emoji: &str) -> bool {
    !emoji.is_ascii() && emoji.graphemes(true).count() == 1 && bitmap(emoji).is_some()
}

/// What the "Paste any emoji" field holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FieldInput {
    /// Nothing but white space.
    Empty,
    /// This emoji: the last drawable grapheme, so pasting over or after the old one both work.
    Emoji(String),
    /// Text without a drawable emoji, or an emoji the font does not have.
    NotEmoji,
    /// An emoji over [`EMOJI_MAX_BYTES`] (a family, a subdivision flag): it cannot be sent.
    TooLong,
}

/// Read the field as the browser's `pickEmoji` does.
pub fn read_field(text: &str) -> FieldInput {
    match text.graphemes(true).rev().find(|g| is_drawable(g)) {
        Some(g) if g.len() > EMOJI_MAX_BYTES => FieldInput::TooLong,
        Some(g) => FieldInput::Emoji(g.to_string()),
        None if text.trim().is_empty() => FieldInput::Empty,
        None => FieldInput::NotEmoji,
    }
}

/// An emoji ready to draw at one size: its baked texture and the avatar gradient its colour gives.
/// Cheap to clone (the texture is a handle).
#[derive(Clone)]
pub struct Glyph {
    pub tex: TextureHandle,
    /// The texture's size in points: one texel is one device pixel.
    pub size: Vec2,
    /// The balance point inside the texture, in points from its top left.
    pub anchor: Vec2,
    pub colors: [Color32; 2],
}

impl Glyph {
    /// Where the texture goes so that its balance point lands on `center`, on whole device pixels
    /// so that texels stay pixels.
    pub fn rect(&self, center: Pos2, ppp: f32) -> Rect {
        let min = center - self.anchor;
        Rect::from_min_size(pos2((min.x * ppp).round() / ppp, (min.y * ppp).round() / ppp), self.size)
    }

    /// Paint it with its balance point on `center`.
    pub fn paint(&self, ui: &Ui, center: Pos2) {
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));
        ui.painter().image(self.tex.id(), self.rect(center, ui.ctx().pixels_per_point()), uv, Color32::WHITE);
    }
}

/// The decoded form of one bitmap, before it is a texture: what the tests look at.
pub struct Prepared {
    /// The visible pixels' bounding box in the source bitmap: x, y, width, height.
    pub crop: [u32; 4],
    /// The crop at full size, premultiplied.
    pub pixels: image::RgbaImage,
    /// Where the emoji balances, in crop pixels: see [`balance_point`].
    pub balance: (f32, f32),
    /// The strongest hue in degrees (a 10-degree bin's middle), `None` when the emoji has almost
    /// no colour.
    pub hue: Option<u32>,
}

/// Crop, measure and colour one bitmap. Pure, so it is tested without a GPU.
pub fn prepare(src: &image::RgbaImage) -> Option<Prepared> {
    let (w, h) = src.dimensions();
    let visible = |x: u32, y: u32| src.get_pixel(x, y)[3] >= VISIBLE_ALPHA;
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for y in 0..h {
        for x in 0..w {
            if visible(x, y) {
                (x0, y0, x1, y1) = (x0.min(x), y0.min(y), x1.max(x), y1.max(y));
            }
        }
    }
    if x1 < x0 {
        return None;
    }
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    let mut pixels = image::RgbaImage::new(cw, ch);
    for (x, y, px) in pixels.enumerate_pixels_mut() {
        let [r, g, b, a] = src.get_pixel(x0 + x, y0 + y).0;
        let pm = |c: u8| (u16::from(c) * u16::from(a) / 255) as u8;
        *px = image::Rgba([pm(r), pm(g), pm(b), a]);
    }
    let balance = balance_point(&pixels);
    Some(Prepared { crop: [x0, y0, cw, ch], pixels, balance, hue: strongest_hue(src) })
}

/// The point of `crop` that goes on the disc's centre: the midpoint of the solid pixels' mass centre
/// and the centre of their smallest enclosing circle. The first is where the eye feels the weight,
/// the second gives equal margins inside a round frame; see the module's `Why` for each one alone.
fn balance_point(crop: &image::RgbaImage) -> (f32, f32) {
    let (mut sum, mut count) = ((0.0_f64, 0.0_f64), 0_u32);
    // Each row's outermost solid pixels: all the enclosing circle can touch.
    let mut edge = Vec::new();
    for y in 0..crop.height() {
        let solid = |x: &u32| crop.get_pixel(*x, y)[3] >= SOLID_ALPHA;
        let cy = y as f32 + 0.5;
        if let (Some(l), Some(r)) = ((0..crop.width()).find(solid), (0..crop.width()).rfind(solid)) {
            edge.extend([(l as f32, cy), (r as f32 + 1.0, cy)]);
        }
        for x in (0..crop.width()).filter(solid) {
            sum = (sum.0 + f64::from(x) + 0.5, sum.1 + f64::from(cy));
            count += 1;
        }
    }
    if count == 0 {
        return (crop.width() as f32 / 2.0, crop.height() as f32 / 2.0);
    }
    let mass = ((sum.0 / f64::from(count)) as f32, (sum.1 / f64::from(count)) as f32);
    let circle = enclosing_centre(&edge, mass);
    ((mass.0 + circle.0) / 2.0, (mass.1 + circle.1) / 2.0)
}

/// The centre of the smallest circle around `points` (Badoiu-Clarkson: step towards the farthest
/// point by a shrinking fraction); after 400 steps it is well under a bitmap pixel off.
fn enclosing_centre(points: &[(f32, f32)], start: (f32, f32)) -> (f32, f32) {
    let mut c = start;
    for i in 1..=400 {
        let dist = |p: &&(f32, f32)| (p.0 - c.0).powi(2) + (p.1 - c.1).powi(2);
        let Some(far) = points.iter().max_by(|a, b| dist(a).total_cmp(&dist(b))) else { break };
        let step = 1.0 / (i as f32 + 1.0);
        c = (c.0 + (far.0 - c.0) * step, c.1 + (far.1 - c.1) * step);
    }
    c
}

/// The texture of one drawn size.
pub struct Baked {
    /// Premultiplied, one texel per device pixel.
    pub image: image::RgbaImage,
    /// The balance point in texels.
    pub anchor: (f32, f32),
}

/// Scale the crop to `font_px` device pixels and, with `outline` > 0, put a black line of that many
/// device pixels around its shape. The line is a dilation of the scaled alpha over a round kernel
/// (a radius of 1 is the 3 x 3 square of the replica's four chained `drop-shadow`s), composited
/// behind the emoji in the same texture. Resizing in premultiplied space keeps the edges free of a
/// dark halo.
pub fn bake(p: &Prepared, font_px: f32, outline: u32) -> Baked {
    let scale = font_px / EM_PX;
    let (cw, ch) = p.pixels.dimensions();
    let (w, h) = (((cw as f32 * scale).round() as u32).max(1), ((ch as f32 * scale).round() as u32).max(1));
    let small = image::imageops::resize(&p.pixels, w, h, image::imageops::FilterType::Triangle);
    let r = outline as i32;
    let reach = (r as f32 + 0.5).powi(2);
    let kernel: Vec<(i32, i32)> = (-r..=r).flat_map(|y| (-r..=r).map(move |x| (x, y))).filter(|(x, y)| (x * x + y * y) as f32 <= reach).collect();
    let alpha = |x: i32, y: i32| if x >= 0 && y >= 0 && x < w as i32 && y < h as i32 { small.get_pixel(x as u32, y as u32)[3] } else { 0 };
    let mut image = image::RgbaImage::new(w + 2 * outline, h + 2 * outline);
    for (x, y, px) in image.enumerate_pixels_mut() {
        let (sx, sy) = (x as i32 - r, y as i32 - r);
        let line = u16::from(kernel.iter().map(|(dx, dy)| alpha(sx + dx, sy + dy)).max().unwrap_or(0));
        let [cr, cg, cb, ca] = if alpha(sx, sy) > 0 { small.get_pixel(sx as u32, sy as u32).0 } else { [0; 4] };
        // the emoji over a black line: premultiplied colours stay, only the alpha adds up
        let a = u16::from(ca) + (255 - u16::from(ca)) * line / 255;
        *px = image::Rgba([cr, cg, cb, a as u8]);
    }
    Baked { image, anchor: (p.balance.0 * w as f32 / cw as f32 + r as f32, p.balance.1 * h as f32 / ch as f32 + r as f32) }
}

/// The replica's `emojiHue()` on the full bitmap: the strongest hue in 10-degree bins (each pixel
/// votes with its saturation, so a mix of colours cannot average to mud), `None` when almost
/// colourless. The replica rasterises the emoji at 26 px (about 1000 visible pixels) and needs 30
/// coloured ones; the bitmap has about 17.6 times the pixels, so the same share is 527.
fn strongest_hue(src: &image::RgbaImage) -> Option<u32> {
    const MIN_COLOURED: u32 = 527;
    let mut bins = [0.0_f32; 36];
    let mut coloured = 0;
    for px in src.pixels() {
        let [r, g, b, a] = px.0;
        if a < 128 {
            continue;
        }
        let (hi, lo) = (r.max(g).max(b), r.min(g).min(b));
        let sat = if hi == 0 { 0.0 } else { f32::from(hi - lo) / f32::from(hi) };
        if sat < 0.25 || hi < 40 {
            continue;
        }
        let (rf, gf, bf, dl) = (f32::from(r), f32::from(g), f32::from(b), f32::from(hi - lo));
        let h = 60.0 * if hi == r { (gf - bf) / dl + 6.0 } else if hi == g { (bf - rf) / dl + 2.0 } else { (rf - gf) / dl + 4.0 };
        bins[((h % 360.0) / 10.0) as usize] += sat;
        coloured += 1;
    }
    if coloured < MIN_COLOURED {
        return None;
    }
    // first of the largest bins, like `bins.indexOf(Math.max(...bins))`
    let best = bins.iter().enumerate().fold((0, -1.0), |best, (i, v)| if *v > best.1 { (i, *v) } else { best }).0;
    Some(best as u32 * 10 + 5)
}

/// The avatar gradient (140 degrees, lighter stop first) of an emoji with this strongest hue:
/// the name gradient's saturation and lightness (60 %, 36 % to 24 %, the second stop 10 degrees
/// back) on the emoji's hue, or the neutral slate `hsl(222, 14 %, 36 %)` to `hsl(222, 14 %, 24 %)`
/// for a colourless one like the skull.
pub fn emoji_colors(hue: Option<u32>) -> [Color32; 2] {
    match hue {
        Some(h) => [theme::hsl_rgb(h, 0.6, 0.36), theme::hsl_rgb((h + 350) % 360, 0.6, 0.24)],
        None => [theme::hsl_rgb(222, 0.14, 0.36), theme::hsl_rgb(222, 0.14, 0.24)],
    }
}

/// One cached emoji; `prepared` is `None` for a string with no picture, remembered like any other.
struct Entry {
    prepared: Option<Prepared>,
    /// The baked sizes, keyed by [`size_key`], oldest first.
    baked: Vec<(u32, u32, Glyph)>,
    used: u64,
}

#[derive(Default)]
struct Glyphs {
    entries: HashMap<String, Entry>,
    tick: u64,
}

type Cache = Arc<Mutex<Glyphs>>;

/// `font_px` (device pixels) to an eighth of a pixel: what tells two baked sizes apart.
fn size_key(font_px: f32) -> u32 {
    (font_px * 8.0).round() as u32
}

/// The decoded form of `emoji`, `None` when it has no picture.
fn load(emoji: &str) -> Option<Prepared> {
    if !is_drawable(emoji) {
        return None;
    }
    prepare(&image::load_from_memory_with_format(bitmap(emoji)?, image::ImageFormat::Png).ok()?.to_rgba8())
}

/// The drawable form of `emoji` at a font size of `font_px` points, with the outline when `outlined`;
/// `None` for anything without a picture (see [`is_drawable`], or over [`EMOJI_MAX_BYTES`]).
/// Prepared and baked on first use and cached in `ctx`.
pub fn emoji_glyph(ctx: &Context, emoji: &str, font_px: f32, outlined: bool) -> Option<Glyph> {
    if emoji.is_empty() || emoji.len() > EMOJI_MAX_BYTES {
        return None;
    }
    let ppp = ctx.pixels_per_point();
    let cache: Cache = ctx.data_mut(|d| d.get_temp_mut_or_insert_with(Id::new("sss-emoji-glyphs"), Cache::default).clone());
    let mut cache = cache.lock().unwrap_or_else(|e| e.into_inner());
    cache.tick += 1;
    let tick = cache.tick;
    if !cache.entries.contains_key(emoji) {
        if cache.entries.len() >= MAX_CACHED
            && let Some(old) = cache.entries.iter().min_by_key(|(_, e)| e.used).map(|(k, _)| k.clone())
        {
            cache.entries.remove(&old);
        }
        cache.entries.insert(emoji.to_string(), Entry { prepared: load(emoji), baked: Vec::new(), used: tick });
    }
    let entry = cache.entries.get_mut(emoji)?;
    entry.used = tick;
    let p = entry.prepared.as_ref()?;
    // the replica's outline is one CSS pixel, so it scales with the display, and always shows
    let outline = if outlined { ppp.round().max(1.0) as u32 } else { 0 };
    let key = size_key(font_px * ppp);
    if let Some((.., g)) = entry.baked.iter().find(|(k, o, _)| *k == key && *o == outline) {
        return Some(g.clone());
    }
    let baked = bake(p, font_px * ppp, outline);
    let (tw, th) = baked.image.dimensions();
    let pixels = baked.image.pixels().map(|px| Color32::from_rgba_premultiplied(px[0], px[1], px[2], px[3])).collect();
    let image = egui::ColorImage { size: [tw as usize, th as usize], source_size: vec2(tw as f32, th as f32), pixels };
    let glyph = Glyph {
        tex: ctx.load_texture(format!("emoji-{emoji}-{key}-{outline}"), image, TextureOptions::LINEAR),
        size: vec2(tw as f32, th as f32) / ppp,
        anchor: vec2(baked.anchor.0, baked.anchor.1) / ppp,
        colors: emoji_colors(p.hue),
    };
    if entry.baked.len() >= MAX_SIZES {
        entry.baked.remove(0);
    }
    entry.baked.push((key, outline, glyph.clone()));
    Some(glyph)
}

/// What the picker returns when the person did something.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EmojiChoice {
    /// This emoji from the grid: apply it and close.
    Pick(String),
    /// This emoji from the field: apply it and stay open, so a second paste can follow.
    Typed(String),
    /// Enter in the field: close.
    Done,
    /// "Use my initial": no emoji.
    Clear,
}

/// The picker's strings, translated by the caller (`widgets/` has no locale).
pub struct PickerText<'a> {
    pub clear: &'a str,
    pub paste: &'a str,
    pub not_emoji: &'a str,
    pub too_long: &'a str,
}

/// What the picker did this frame.
pub struct PickerResult {
    pub choice: Option<EmojiChoice>,
    /// Where the "Paste any emoji" field is.
    pub field: Rect,
}

fn field_id() -> Id {
    Id::new("sss-emoji-field")
}

/// Forget what the field holds; the caller does this while the popover is closed, so a refused
/// paste and its warning are not waiting there next time.
pub fn emoji_picker_closed(ctx: &Context) {
    ctx.data_mut(|d| d.remove_temp::<String>(field_id()));
}

/// The emoji picker of Settings > Profile (replica `emojiMenu`, minus its header): the 56 emoji in an
/// 8-column grid (groups set apart by a small gap) with 2 px gaps filling the available width, `current` highlighted, the "Paste any
/// emoji" field with its warning line, then one item that returns [`EmojiChoice::Clear`]. Pure view:
/// the caller stores the choice.
///
/// `Why` the field empties once it took an emoji, where the replica keeps it selected: the field is
/// egui text, which has no colour emoji and shows a flag or a ZWJ sequence as loose symbols. The
/// picture in the popover's header is the feedback.
pub fn emoji_picker(ui: &mut Ui, current: &str, text: &PickerText) -> PickerResult {
    ui.vertical(|ui| picker_rows(ui, current, text)).inner
}

fn picker_rows(ui: &mut Ui, current: &str, text: &PickerText) -> PickerResult {
    const COLS: usize = 8;
    const GAP: f32 = 2.0;
    let mut choice = None;
    ui.spacing_mut().item_spacing.y = 0.0;
    let cell = ((ui.available_width() - GAP * (COLS - 1) as f32) / COLS as f32).floor();
    for (row, chunk) in EMOJI.chunks(COLS).enumerate() {
        let top = ui.cursor().top();
        let left = ui.cursor().left();
        let row_rect = Rect::from_min_size(pos2(left, top), vec2(ui.available_width(), cell));
        for (col, emoji) in chunk.iter().enumerate() {
            let rect = Rect::from_min_size(pos2(left + col as f32 * (cell + GAP), top), vec2(cell, cell));
            let resp = ui.interact(rect, ui.id().with(("emoji-cell", row, col)), Sense::click());
            let r7 = CornerRadius::same(7);
            if *emoji == current {
                ui.painter().rect(rect, r7, theme::ACCENT_CHIP_BG, Stroke::new(1.0_f32, theme::ACCENT_CHIP_BORDER), StrokeKind::Inside);
            } else if resp.hovered() {
                ui.painter().rect_filled(rect, r7, theme::WASH_HOVER_1);
            }
            if let Some(g) = emoji_glyph(ui.ctx(), emoji, PICKER_EMOJI_PX, false) {
                g.paint(ui, rect.center());
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                choice = Some(EmojiChoice::Pick((*emoji).to_string()));
            }
        }
        ui.allocate_rect(row_rect, Sense::hover());
        if row + 1 < EMOJI.len().div_ceil(COLS) {
            ui.add_space(if GROUP_ROWS.contains(&(row + 1)) { GAP + 6.0 } else { GAP });
        }
    }
    ui.add_space(theme::S2);
    let mut typed: String = ui.data_mut(|d| d.get_temp(field_id())).unwrap_or_default();
    let field = text_field(ui, &mut typed, text.paste);
    let hint = match read_field(&typed) {
        FieldInput::Emoji(e) => {
            typed.clear();
            choice = Some(EmojiChoice::Typed(e));
            None
        }
        FieldInput::Empty => None,
        FieldInput::NotEmoji => Some(text.not_emoji),
        FieldInput::TooLong => Some(text.too_long),
    };
    if field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && choice.is_none() {
        choice = Some(EmojiChoice::Done);
    }
    ui.data_mut(|d| d.insert_temp(field_id(), typed));
    if let Some(hint) = hint {
        // replica `.emoji-hint`: 10 px in, the field's own text inset; tiny, in the warning colour
        let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 20.0), Sense::hover());
        paint_text(ui, rect.with_min_x(rect.left() + 10.0), hint, font(Weight::Regular, theme::FS_TINY), theme::WARNING, Align::Min);
    }
    ui.add_space(theme::S1);
    let clear = PopItem { icon: "text_fields", label: text.clear, sub: None, danger: false, selected: current.is_empty(), enabled: true, meter: None };
    if pop_item(ui, &clear).clicked() {
        choice = Some(EmojiChoice::Clear);
    }
    PickerResult { choice, field: field.rect }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(emoji: &str) -> image::RgbaImage {
        image::load_from_memory_with_format(bitmap(emoji).unwrap_or_else(|| panic!("{emoji} has no bitmap")), image::ImageFormat::Png).unwrap().to_rgba8()
    }

    #[test]
    fn every_picker_emoji_has_a_picture() {
        for emoji in EMOJI {
            assert!(is_drawable(emoji), "{emoji}");
            let p = prepare(&decode(emoji)).unwrap_or_else(|| panic!("{emoji} has no visible pixels"));
            assert!(p.crop[2] > 40 && p.crop[3] > 40, "{emoji} crop {:?}", p.crop);
        }
        let unique: std::collections::HashSet<_> = EMOJI.iter().collect();
        assert_eq!(unique.len(), EMOJI.len());
    }

    #[test]
    fn sequences_shape_to_one_picture_and_everything_else_to_none() {
        // a flag, a skin tone, a ZWJ sequence, a keycap, a variation selector, a pride flag
        for e in ["🇩🇪", "👍🏽", "🧑‍💻", "1️⃣", "#️⃣", "☕\u{fe0f}", "🏳️‍🌈", "👩🏽‍💻"] {
            assert!(is_drawable(e), "{e}");
            assert!(e.len() <= EMOJI_MAX_BYTES, "{e} fits the wire");
        }
        // the sequences are not their first code point's picture
        assert_ne!(bitmap("👍🏽"), bitmap("👍"));
        assert_ne!(bitmap("1️⃣"), bitmap("1"));
        // text; two graphemes; the font's plain digit; an unassigned code point; no such ZWJ sequence; a lone joiner
        for e in ["", "a", "ab", "🦊🍕", "🦊a", "a🦊", " 🦊", "1", "#", "\u{1fade}", "🦊\u{200d}🍕", "\u{200d}", "\u{fe0f}", "ä", "日"] {
            assert!(!is_drawable(e), "{e:?}");
        }
    }

    #[test]
    fn the_field_takes_the_last_emoji_and_refuses_the_rest() {
        let emoji = |s: &str| FieldInput::Emoji(s.to_string());
        assert_eq!(read_field(""), FieldInput::Empty);
        assert_eq!(read_field("   "), FieldInput::Empty);
        assert_eq!(read_field("🦊"), emoji("🦊"));
        assert_eq!(read_field("🦊🍕"), emoji("🍕"), "pasted after the old one");
        assert_eq!(read_field("hi 🇩🇪 there"), emoji("🇩🇪"));
        assert_eq!(read_field("🧑‍💻"), emoji("🧑‍💻"), "one grapheme, not its parts");
        assert_eq!(read_field("hello"), FieldInput::NotEmoji);
        assert_eq!(read_field("42"), FieldInput::NotEmoji);
        assert_eq!(read_field("\u{1fade}"), FieldInput::NotEmoji, "an emoji newer than the font");
        // 25 bytes, drawable, but the wire takes 16 (the browser's `avatar_too_long`)
        assert!("👨‍👩‍👧‍👦".len() > EMOJI_MAX_BYTES);
        assert_eq!(read_field("👨‍👩‍👧‍👦"), FieldInput::TooLong);
        assert_eq!(read_field("🏴\u{e0067}\u{e0062}\u{e0065}\u{e006e}\u{e0067}\u{e007f}"), FieldInput::TooLong);
    }

    #[test]
    fn whatever_a_peer_sends_never_panics() {
        // a deterministic stream of awkward strings: joiners, selectors, tags, combining marks, emoji, surrogate neighbours
        let pool = ['\u{200d}', '\u{fe0f}', '\u{fe0e}', '\u{20e3}', '\u{e0067}', '\u{e007f}', '\u{1f3fd}', '\u{1f1e9}', '\u{1f1ea}', '🦊', '👨', '1', '#', 'a', '\u{301}', '\u{d7ff}', '\u{e000}', '\u{10ffff}', '\u{0}', '\u{ad}'];
        let ctx = Context::default();
        let mut seed = 0x2545_f491_u32;
        for _ in 0..3000 {
            let mut s = String::new();
            loop {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let c = pool[(seed >> 16) as usize % pool.len()];
                if s.len() + c.len_utf8() > EMOJI_MAX_BYTES || seed >> 28 == 0 {
                    break;
                }
                s.push(c);
            }
            let _ = emoji_glyph(&ctx, &s, 20.0, true);
            let _ = read_field(&s);
        }
    }

    #[test]
    fn the_fox_is_orange_and_the_skull_is_slate() {
        let fox = prepare(&decode("🦊")).unwrap().hue.expect("a fox has a colour");
        assert!((10..=40).contains(&fox), "fox hue {fox}");
        assert_eq!(prepare(&decode("💀")).unwrap().hue, None, "a skull has almost no colour");
        // the gradient stops: the name gradient's saturation and lightness on the emoji's hue
        let [a, b] = emoji_colors(Some(fox));
        assert!(a.r() > a.b() && b.r() > b.b(), "{a:?} {b:?}");
        assert!(a.r() > b.r(), "the first stop is the lighter one");
        assert_eq!(emoji_colors(None), [Color32::from_rgb(0x4f, 0x57, 0x69), Color32::from_rgb(0x35, 0x3a, 0x46)]);
        // the colour logic over every emoji: the hues sit in bins, colourless ones are few
        let colourless = EMOJI.iter().filter(|e| prepare(&decode(e)).unwrap().hue.is_none()).count();
        assert!((1..=12).contains(&colourless), "{colourless} colourless emoji");
    }

    fn block(w: u32, h: u32, x: std::ops::Range<u32>, y: std::ops::Range<u32>) -> image::RgbaImage {
        let mut src = image::RgbaImage::new(w, h);
        for py in y {
            for px in x.clone() {
                src.put_pixel(px, py, image::Rgba([200, 30, 30, 255]));
            }
        }
        src
    }

    #[test]
    fn a_shifted_glyph_is_cropped_to_its_visible_pixels_and_balances_in_its_middle() {
        // a 40x30 bitmap whose only visible pixels are a 6x4 block pushed into the top-left corner
        let p = prepare(&block(40, 30, 5..11, 3..7)).unwrap();
        assert_eq!(p.crop, [5, 3, 6, 4]);
        assert_eq!(p.hue, None, "far too few coloured pixels to count");
        assert!((p.balance.0 - 3.0).abs() < 0.1 && (p.balance.1 - 2.0).abs() < 0.1, "{:?}", p.balance);
        // a fully transparent bitmap has nothing to draw
        assert!(prepare(&image::RgbaImage::new(8, 8)).is_none());
    }

    #[test]
    fn near_transparent_pixels_move_neither_the_crop_nor_the_balance() {
        let mut src = block(40, 40, 10..30, 10..30);
        // a faint glow far right: below the crop's threshold, and a soft one above it but below "solid"
        src.put_pixel(39, 20, image::Rgba([255, 255, 255, 8]));
        src.put_pixel(35, 20, image::Rgba([255, 255, 255, 60]));
        let p = prepare(&src).unwrap();
        assert_eq!(p.crop, [10, 10, 26, 20], "the soft pixel is kept, the near-transparent one is not");
        assert!((p.balance.0 - 10.0).abs() < 0.1 && (p.balance.1 - 10.0).abs() < 0.1, "{:?}", p.balance);
    }

    #[test]
    fn the_enclosing_circle_of_a_right_triangle_is_its_hypotenuse_midpoint() {
        let c = enclosing_centre(&[(0.0, 0.0), (100.0, 0.0), (0.0, 80.0)], (33.0, 26.0));
        assert!((c.0 - 50.0).abs() < 0.5 && (c.1 - 40.0).abs() < 0.5, "{c:?}");
    }

    /// The balance point relative to the crop box's centre, at a 32 px avatar's font size.
    fn shift_at_32(emoji: &str) -> (f32, f32) {
        let p = prepare(&decode(emoji)).unwrap();
        let k = EMOJI_AVATAR_FRACTION * 32.0 / EM_PX;
        ((p.balance.0 - p.crop[2] as f32 / 2.0) * k, (p.balance.1 - p.crop[3] as f32 / 2.0) * k)
    }

    #[test]
    fn asymmetric_emoji_balance_off_their_box_and_symmetric_ones_on_it() {
        // the pizza's weight is on the right: the point moves right, so the picture moves left
        let (x, _) = shift_at_32("🍕");
        assert!(x > 1.0, "pizza {x}");
        for e in ["😀", "⚽", "🍩"] {
            let (x, y) = shift_at_32(e);
            assert!(x.abs() < 0.3 && y.abs() < 0.3, "{e} {x} {y}");
        }
        // nothing is pushed out of the disc: the shape stays inside its 16 px radius with the outline
        let k = EMOJI_AVATAR_FRACTION * 32.0 / EM_PX;
        for e in EMOJI {
            let p = prepare(&decode(e)).unwrap();
            let far = p.pixels.enumerate_pixels().filter(|(.., px)| px[3] >= SOLID_ALPHA).map(|(x, y, _)| ((x as f32 + 0.5 - p.balance.0).powi(2) + (y as f32 + 0.5 - p.balance.1).powi(2)).sqrt()).fold(0.0_f32, f32::max);
            assert!(far * k + 1.0 <= 17.0, "{e} reaches {}", far * k);
        }
    }

    #[test]
    fn the_outline_is_an_even_black_line_around_the_shape() {
        // a solid 20 x 20 block drawn 1:1: 109 px font = 1 bitmap pixel per device pixel
        let p = prepare(&block(20, 20, 0..20, 0..20)).unwrap();
        let b = bake(&p, EM_PX, 1);
        assert_eq!(b.image.dimensions(), (22, 22));
        assert!((b.anchor.0 - 11.0).abs() < 0.1 && (b.anchor.1 - 11.0).abs() < 0.1, "{:?}", b.anchor);
        let black = |x, y| b.image.get_pixel(x, y).0 == [0, 0, 0, 255];
        for i in 0..22 {
            assert!(black(i, 0) && black(i, 21) && black(0, i) && black(21, i), "ring at {i}, corners too");
        }
        assert_eq!(b.image.get_pixel(11, 11).0, [200, 30, 30, 255], "the emoji is untouched");
        // hi-DPI: two device pixels, no longer square at the very corner
        let b2 = bake(&p, EM_PX, 2);
        assert_eq!(b2.image.dimensions(), (24, 24));
        assert_eq!(b2.image.get_pixel(1, 12).0, [0, 0, 0, 255]);
        assert_eq!(b2.image.get_pixel(0, 0)[3], 0);
        // without an outline nothing is added
        let plain = bake(&p, EM_PX, 0);
        assert_eq!(plain.image.dimensions(), (20, 20));
        assert!(plain.image.pixels().all(|px| px.0 == [200, 30, 30, 255]));
    }

    #[test]
    fn a_baked_texture_is_valid_premultiplied_at_every_size_the_app_draws() {
        for e in ["🦊", "🍕", "💀", "🇩🇪"] {
            let p = prepare(&decode(e)).unwrap();
            for font_px in [13.6, 19.8, 27.3, 39.7, 59.5, 119.0] {
                let b = bake(&p, font_px, 1);
                assert!(b.image.pixels().all(|px| px[0] <= px[3] && px[1] <= px[3] && px[2] <= px[3]), "{e} at {font_px}");
                let (w, h) = b.image.dimensions();
                assert!(b.anchor.0 > 0.0 && b.anchor.0 < w as f32 && b.anchor.1 > 0.0 && b.anchor.1 < h as f32);
            }
        }
    }

    #[test]
    fn an_undrawable_emoji_has_no_picture_and_a_known_one_is_cached_per_size() {
        let ctx = Context::default();
        assert!(emoji_glyph(&ctx, "\u{1fade}", 20.0, true).is_none());
        assert!(emoji_glyph(&ctx, "", 20.0, true).is_none());
        assert!(emoji_glyph(&ctx, "👨‍👩‍👧‍👦", 20.0, true).is_none(), "over the wire's cap");
        let a = emoji_glyph(&ctx, "🦊", 20.0, true).expect("the fox");
        let b = emoji_glyph(&ctx, "🦊", 20.0, true).expect("the fox again");
        assert_eq!(a.tex.id(), b.tex.id(), "baked and uploaded once");
        assert_ne!(a.tex.id(), emoji_glyph(&ctx, "🦊", 30.0, true).unwrap().tex.id(), "another size");
        assert_ne!(a.tex.id(), emoji_glyph(&ctx, "🦊", 20.0, false).unwrap().tex.id(), "without the outline");
        assert!(emoji_glyph(&ctx, "🦖", 20.0, true).is_some(), "any emoji, not only the 40");
    }

    #[test]
    fn a_glyph_lands_on_whole_pixels_with_its_balance_point_on_the_centre() {
        let ctx = Context::default();
        let g = emoji_glyph(&ctx, "🍕", 20.0, true).unwrap();
        let r = g.rect(pos2(100.0, 50.0), 1.0);
        assert_eq!((r.min.x.fract(), r.min.y.fract()), (0.0, 0.0));
        assert!((r.min.x + g.anchor.x - 100.0).abs() <= 0.5 && (r.min.y + g.anchor.y - 50.0).abs() <= 0.5);
    }

    #[test]
    fn the_cache_is_bounded_however_many_strings_arrive() {
        let ctx = Context::default();
        // 400 different strings with no picture, and more real emoji than the cache holds
        for i in 0..400_u32 {
            let _ = emoji_glyph(&ctx, &format!("x{i}"), 20.0, true);
        }
        let real: Vec<String> = (0x1f600..0x1f650).chain(0x1f400..0x1f480).filter_map(char::from_u32).map(String::from).collect();
        assert!(real.len() > MAX_CACHED);
        for e in &real {
            assert!(emoji_glyph(&ctx, e, 20.0, true).is_some(), "{e}");
        }
        let cache: Cache = ctx.data_mut(|d| d.get_temp_mut_or_insert_with(Id::new("sss-emoji-glyphs"), Cache::default).clone());
        assert_eq!(cache.lock().unwrap().entries.len(), MAX_CACHED);
        // a size sweep cannot grow one entry either
        for i in 0..100 {
            let _ = emoji_glyph(&ctx, "🦊", 10.0 + i as f32, true);
        }
        assert!(cache.lock().unwrap().entries["🦊"].baked.len() <= MAX_SIZES);
    }
}
