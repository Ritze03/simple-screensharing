//! Emoji avatars (task #106, decision D6/D1): the 40 picker emoji as bundled pictures, the colour
//! logic that takes an avatar's background from its emoji, the crop that centres it, and the picker.
//!
//! egui's text path cannot draw colour emoji and the AppImage / Flatpak / Windows builds cannot rely
//! on a system emoji font, so the 40 emoji of the picker are embedded as Noto Color Emoji bitmaps
//! (`client/assets/emoji/`, OFL 1.1, see its README). An emoji outside the set (a peer sent it, or an
//! old settings file holds it) has no picture and shows as the initial. Each picture is decoded once
//! per egui context, cropped to its visible pixels, its strongest hue taken, and uploaded as a
//! texture that stays cached; nothing of this runs per frame.
//!
//! `Why` the crop is what gets centred: the font's own box is not centred for every emoji (the fox's
//! ears and chin sit off-centre in it), so centring the box puts those emoji visibly off the disc's
//! middle. The visible pixels' bounding box is what the eye centres.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use egui::{Color32, Context, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind, TextureHandle, TextureOptions, Ui, Vec2, pos2, vec2};

use super::{PopItem, pop_item};
use crate::theme;

/// The picker's emoji, in picker order: the replica's `const EMOJI`, the contract with the browser.
/// Each has its picture in `client/assets/emoji/<hex codepoint>.png` (see [`PICTURES`]).
pub const EMOJI: [&str; 40] = [
    "😀", "😎", "🤓", "🥳", "🤖", "👻", "👽", "🎃", "💀", "🤡", "🐶", "🐱", "🦊", "🐻", "🐼", "🐸", "🐙", "🦄", "🐧", "🦉", "🍕",
    "🍔", "🍩", "🍣", "🍉", "☕", "🌮", "🍺", "🎮", "🎧", "🎬", "🎸", "⚽", "🚀", "🌈", "⭐", "🔥", "⚡", "🌙", "🌵",
];

macro_rules! pictures {
    ($($hex:literal),* $(,)?) => { [$(include_bytes!(concat!("../../assets/emoji/", $hex, ".png")) as &[u8]),*] };
}

/// The bitmaps, in [`EMOJI`] order (a test checks the file names against the code points).
static PICTURES: [&[u8]; 40] = pictures![
    "1f600", "1f60e", "1f913", "1f973", "1f916", "1f47b", "1f47d", "1f383", "1f480", "1f921", "1f436", "1f431", "1f98a", "1f43b",
    "1f43c", "1f438", "1f419", "1f984", "1f427", "1f989", "1f355", "1f354", "1f369", "1f363", "1f349", "2615", "1f32e", "1f37a",
    "1f3ae", "1f3a7", "1f3ac", "1f3b8", "26bd", "1f680", "1f308", "2b50", "1f525", "26a1", "1f319", "1f335",
];

/// Whether `emoji` is one this client can draw (one of [`EMOJI`]).
pub fn is_picker_emoji(emoji: &str) -> bool {
    EMOJI.contains(&emoji)
}

/// Noto's bitmaps are the 109 ppem strike: a font size of `s` px draws a bitmap pixel `s / 109` px
/// wide. This is what makes `font-size: 0.62 x avatar` of the replica and our picture agree.
const EM_PX: f32 = 109.0;
/// The emoji's font size as a fraction of the avatar's size (replica `.avatar .emo`).
pub const EMOJI_AVATAR_FRACTION: f32 = 0.62;
/// The emoji's font size in the picker's cells (replica `.emoji-grid button`).
const PICKER_EMOJI_PX: f32 = 20.0;
/// A pixel counts as visible from this alpha on.
const VISIBLE_ALPHA: u8 = 16;

/// An emoji ready to draw: its cropped, half-size texture, the crop's size in source pixels and
/// the avatar gradient its colour gives. Cheap to clone (the texture is a handle).
#[derive(Clone)]
pub struct Glyph {
    pub tex: TextureHandle,
    /// Size of the visible pixels' bounding box in bitmap pixels (the texture is half of this).
    pub crop: Vec2,
    pub colors: [Color32; 2],
}

/// The decoded form of one bitmap, before it is a texture: what the tests look at.
pub struct Prepared {
    /// The visible pixels' bounding box in the source bitmap: x, y, width, height.
    pub crop: [u32; 4],
    /// The crop, premultiplied and at half size (what becomes the texture).
    pub texture: image::RgbaImage,
    /// The strongest hue in degrees (a 10-degree bin's middle), `None` when the emoji has almost
    /// no colour.
    pub hue: Option<u32>,
}

/// Crop, colour and downscale one bitmap. Pure, so it is tested without a GPU.
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
    let mut crop = image::RgbaImage::new(cw, ch);
    for (x, y, px) in crop.enumerate_pixels_mut() {
        let [r, g, b, a] = src.get_pixel(x0 + x, y0 + y).0;
        let pm = |c: u8| (u16::from(c) * u16::from(a) / 255) as u8;
        *px = image::Rgba([pm(r), pm(g), pm(b), a]);
    }
    // Resizing in premultiplied space keeps the edges free of a dark halo.
    let texture = image::imageops::resize(&crop, cw.div_ceil(2), ch.div_ceil(2), image::imageops::FilterType::Triangle);
    Some(Prepared { crop: [x0, y0, cw, ch], texture, hue: strongest_hue(src) })
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

/// CSS `hsl()` to sRGB, each channel `round(255 * v)`; the same formula as `theme::avatar_colors`
/// (that one is private to the theme, and this takes a fractional saturation).
fn hsl(h: u32, s: f32, l: f32) -> Color32 {
    let a = s * l.min(1.0 - l);
    let f = |n: f32| {
        let k = (n + h as f32 / 30.0) % 12.0;
        (255.0 * (l - a * (-1.0_f32).max((k - 3.0).min(9.0 - k).min(1.0)))).round() as u8
    };
    Color32::from_rgb(f(0.0), f(8.0), f(4.0))
}

/// The avatar gradient (140 degrees, lighter stop first) of an emoji with this strongest hue:
/// the name gradient's saturation and lightness (60 %, 36 % to 24 %, the second stop 10 degrees
/// back) on the emoji's hue, or the neutral slate `hsl(222, 14 %, 36 %)` to `hsl(222, 14 %, 24 %)`
/// for a colourless one like the skull.
pub fn emoji_colors(hue: Option<u32>) -> [Color32; 2] {
    match hue {
        Some(h) => [hsl(h, 0.6, 0.36), hsl((h + 350) % 360, 0.6, 0.24)],
        None => [hsl(222, 0.14, 0.36), hsl(222, 0.14, 0.24)],
    }
}

/// Where the cropped picture goes: centred on `center`, scaled so that a bitmap pixel is
/// `font_px / 109` px wide (the picture's own box, not the crop, is what `font_px` means).
pub fn emoji_rect(center: egui::Pos2, font_px: f32, crop: Vec2) -> Rect {
    Rect::from_center_size(center, crop * (font_px / EM_PX))
}

type Cache = Arc<Mutex<HashMap<String, Option<Glyph>>>>;

/// The drawable form of `emoji`, decoded and uploaded on first use and cached in `ctx`; `None` for
/// anything that is not one of [`EMOJI`].
pub fn emoji_glyph(ctx: &Context, emoji: &str) -> Option<Glyph> {
    let cache: Cache = ctx.data_mut(|d| d.get_temp_mut_or_insert_with(Id::new("sss-emoji-glyphs"), Cache::default).clone());
    let mut cache = cache.lock().unwrap();
    if let Some(hit) = cache.get(emoji) {
        return hit.clone();
    }
    let glyph = EMOJI.iter().position(|e| *e == emoji).and_then(|i| {
        let src = image::load_from_memory_with_format(PICTURES[i], image::ImageFormat::Png).ok()?.to_rgba8();
        let p = prepare(&src)?;
        let (tw, th) = p.texture.dimensions();
        let pixels = p.texture.pixels().map(|px| Color32::from_rgba_premultiplied(px[0], px[1], px[2], px[3])).collect();
        let image = egui::ColorImage { size: [tw as usize, th as usize], source_size: vec2(tw as f32, th as f32), pixels };
        Some(Glyph {
            tex: ctx.load_texture(format!("emoji-{emoji}"), image, TextureOptions::LINEAR),
            crop: vec2(p.crop[2] as f32, p.crop[3] as f32),
            colors: emoji_colors(p.hue),
        })
    });
    cache.insert(emoji.to_string(), glyph.clone());
    glyph
}

/// What the picker returns when the person chose something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmojiChoice {
    /// This emoji, one of [`EMOJI`].
    Pick(&'static str),
    /// "Use my initial": no emoji.
    Clear,
}

/// The emoji picker of Settings > Profile (replica `emojiMenu`, minus its header): the 40 emoji in an
/// 8-column grid with 2 px gaps filling the available width, `current` highlighted, then one item
/// labelled `clear_label` that returns [`EmojiChoice::Clear`]. Pure view: the caller stores the choice.
pub fn emoji_picker(ui: &mut Ui, current: &str, clear_label: &str) -> Option<EmojiChoice> {
    ui.vertical(|ui| picker_rows(ui, current, clear_label)).inner
}

fn picker_rows(ui: &mut Ui, current: &str, clear_label: &str) -> Option<EmojiChoice> {
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
            if let Some(g) = emoji_glyph(ui.ctx(), emoji) {
                let img = egui::emath::GuiRounding::round_to_pixels(emoji_rect(rect.center(), PICKER_EMOJI_PX, g.crop), ui.ctx().pixels_per_point());
                ui.painter().image(g.tex.id(), img, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                choice = Some(EmojiChoice::Pick(emoji));
            }
        }
        ui.allocate_rect(row_rect, Sense::hover());
        if row + 1 < EMOJI.len().div_ceil(COLS) {
            ui.add_space(GAP);
        }
    }
    ui.add_space(theme::S1);
    let clear = PopItem { icon: "close", label: clear_label, sub: None, danger: false, selected: current.is_empty(), enabled: true, meter: None };
    if pop_item(ui, &clear).clicked() {
        choice = Some(EmojiChoice::Clear);
    }
    choice
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(emoji: &str) -> image::RgbaImage {
        let i = EMOJI.iter().position(|e| *e == emoji).unwrap();
        image::load_from_memory_with_format(PICTURES[i], image::ImageFormat::Png).unwrap().to_rgba8()
    }

    #[test]
    fn every_picture_matches_its_emoji_and_decodes() {
        for (emoji, png) in EMOJI.iter().zip(PICTURES) {
            let hex = emoji.chars().map(|c| format!("{:x}", c as u32)).collect::<Vec<_>>().join("-");
            // the file name is the code point: a mismatch would show the wrong picture
            let name = format!("{hex}.png");
            let on_disk = std::fs::read(format!("{}/assets/emoji/{name}", env!("CARGO_MANIFEST_DIR"))).expect(&name);
            assert_eq!(&on_disk[..], png, "{emoji} is not {name}");
            let p = prepare(&decode(emoji)).unwrap_or_else(|| panic!("{emoji} has no visible pixels"));
            assert!(p.crop[2] > 40 && p.crop[3] > 40, "{emoji} crop {:?}", p.crop);
        }
        let unique: std::collections::HashSet<_> = EMOJI.iter().collect();
        assert_eq!(unique.len(), 40);
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

    #[test]
    fn a_shifted_glyph_is_cropped_and_centred_on_its_visible_pixels() {
        // a 40x30 bitmap whose only visible pixels are a 6x4 block pushed into the top-left corner
        let mut src = image::RgbaImage::new(40, 30);
        for y in 3..7 {
            for x in 5..11 {
                src.put_pixel(x, y, image::Rgba([200, 30, 30, 255]));
            }
        }
        let p = prepare(&src).unwrap();
        assert_eq!(p.crop, [5, 3, 6, 4]);
        assert_eq!(p.texture.dimensions(), (3, 2), "half size");
        assert_eq!(p.hue, None, "far too few coloured pixels to count");
        // wherever the crop sat in the bitmap, its rectangle is centred on the avatar's centre
        let c = pos2(100.0, 50.0);
        let r = emoji_rect(c, 0.62 * 32.0, vec2(6.0, 4.0));
        assert_eq!(r.center(), c);
        assert!((r.width() - 6.0 * 0.62 * 32.0 / 109.0).abs() < 1e-4);
        // a fully transparent bitmap has nothing to draw
        assert!(prepare(&image::RgbaImage::new(8, 8)).is_none());
    }

    #[test]
    fn the_real_fox_crop_drops_the_empty_edges_of_its_box() {
        let fox = decode("🦊");
        let p = prepare(&fox).unwrap();
        let (w, h) = fox.dimensions();
        assert!(p.crop[2] <= w && p.crop[3] <= h);
        assert!(p.crop[0] > 0 || p.crop[1] > 0 || p.crop[2] < w || p.crop[3] < h, "something was cropped: {:?}", p.crop);
    }

    #[test]
    fn an_unknown_emoji_has_no_picture_and_a_known_one_is_cached() {
        let ctx = Context::default();
        assert!(emoji_glyph(&ctx, "🦖").is_none());
        assert!(emoji_glyph(&ctx, "").is_none());
        let a = emoji_glyph(&ctx, "🦊").expect("the fox");
        let b = emoji_glyph(&ctx, "🦊").expect("the fox again");
        assert_eq!(a.tex.id(), b.tex.id(), "decoded and uploaded once");
        assert!(is_picker_emoji("🦊") && !is_picker_emoji("🦖") && !is_picker_emoji(""));
    }
}
