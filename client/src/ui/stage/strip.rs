//! The people strip under the stage's streams, and the hover card on a person.
//!
//! One 64 px cell per person: ring, avatar, LIVE badge and name all hang off **one centre line** (the snapped
//! avatar rect's), so they line up for a person with and without audio. The green ring (a 2 px circle stroke
//! 3 px outside the avatar, even all round) means the person's shared audio is audible to you: the app has no
//! voice chat. Several streams of one person are one avatar with a count; a popped-out stream shows only as a
//! badge on its owner's avatar (dashed outline, a pop badge) and a click brings it back from there.
//!
//! Hovering a person opens the same card for everyone: a header and, per stream, a row saying Audio or No
//! audio, its numbers and its state (on the stage, popped out with a Bring back button, not watched). The
//! card stays open while the pointer is on the avatar, the gap straight above it or the card, so its button
//! can be reached ([`card_keeps`]).

use egui::emath::GuiRounding;
use egui::{Align, Color32, CornerRadius, Id, Order, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};
use simple_screensharing_client::session::RoomHandle;

use super::{Item, Kind, StageView};
use crate::theme::{self, Weight, font};
use crate::ui::{Cx, UiAction, drive_dump, tr, tr_with};
use crate::widgets::{self, FILL, HBox, fixed, paint_galley, paint_icon, paint_text, text_galley, text_width};

/// A person in the strip: their name and the stage items that are theirs (indices into the stage's items).
pub(super) struct Person {
    pub name: String,
    pub you: bool,
    pub emoji: String,
    pub items: Vec<usize>,
}

/// Who the strip shows: a room's members (you first), or the owners of the watched streams ("Watched").
pub(super) fn people(cx: &Cx, view: &StageView, items: &[Item]) -> Vec<Person> {
    let mine = |name: &str| -> Vec<usize> { items.iter().enumerate().filter(|(_, i)| i.id.who == name).map(|(k, _)| k).collect() };
    match view {
        StageView::Room(h) => {
            let Some(s) = cx.session(*h) else { return Vec::new() };
            let me = s.client.conn_id();
            let you_name = cx.state.display_name.clone();
            let own: Vec<usize> = items.iter().enumerate().filter(|(_, i)| i.mine).map(|(k, _)| k).collect();
            let others = s.members.iter().filter(|m| m.conn_id != me).map(|m| Person { name: m.display_name.clone(), you: false, emoji: s.emoji_of(m.conn_id), items: mine(&m.display_name) });
            std::iter::once(Person { name: you_name, you: true, emoji: cx.state.emoji.clone(), items: own }).chain(others).collect()
        }
        StageView::Watched => {
            let mut out: Vec<Person> = Vec::new();
            for (k, it) in items.iter().enumerate() {
                match out.iter_mut().find(|p| p.name == it.id.who) {
                    Some(p) => p.items.push(k),
                    None => out.push(Person { name: it.id.who.clone(), you: false, emoji: cx.emoji_of_name(it.id.room, &it.id.who), items: vec![k] }),
                }
            }
            out
        }
    }
}

/// Cell width, gap and side padding of the strip.
const CELL_W: f32 = 64.0;
const CELL_GAP: f32 = 14.0;
const SIDE_PAD: f32 = theme::S4;
/// Avatar top inside the strip (room for the ring and the badges above it).
const TOP_PAD: f32 = 22.0;

/// Whether `it` carries audio you can hear now (the ring).
fn audible(cx: &Cx, it: &Item) -> bool {
    match &it.kind {
        Kind::Video(key) => cx.stream_has_audio(*key) && cx.session(key.0).is_some_and(|s| !s.volume(key.1, key.2).muted),
        Kind::Audio(key, _) => cx.is_watching(*key) && cx.session(key.0).is_some_and(|s| !s.volume(key.1, key.2).muted),
        Kind::Mine(capture, _) => cx.paired_audio(*capture).is_some_and(|(id, _)| !cx.state.is_audio_share_muted(id)),
        Kind::Idle(..) => false,
    }
}

/// Whether the stream the item shows has audio at all (the hover card's "Audio" / "No audio").
fn has_audio(cx: &Cx, it: &Item) -> bool {
    match &it.kind {
        Kind::Video(key) | Kind::Idle(key, _) => cx.stream_has_audio(*key),
        Kind::Audio(..) => true,
        Kind::Mine(capture, _) => cx.paired_audio(*capture).is_some(),
    }
}

/// Draw the strip into `rect` (`STRIP_H` tall). Pushes `BringBack` for a click on a person with popped streams.
pub(super) fn show(ui: &mut Ui, cx: &Cx, rect: Rect, view: &StageView, people: &[Person], items: &[Item], acts: &mut Vec<UiAction>) {
    let n = people.len();
    let total = n as f32 * CELL_W + n.saturating_sub(1) as f32 * CELL_GAP;
    let mut child = ui.new_child(UiBuilder::new().max_rect(rect));
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    egui::ScrollArea::horizontal().id_salt("people_strip").scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden).show(&mut child, |ui| {
        let content_w = (total + 2.0 * SIDE_PAD).max(rect.width());
        let (area, _) = ui.allocate_exact_size(vec2(content_w, rect.height()), Sense::hover());
        let mut x = area.left() + (content_w - total) / 2.0;
        for p in people {
            let cell = Rect::from_min_size(pos2(x, area.top()), vec2(CELL_W, rect.height()));
            x += CELL_W + CELL_GAP;
            person(ui, cx, cell, view, p, items, acts);
        }
    });
}

fn person(ui: &mut Ui, cx: &Cx, cell: Rect, view: &StageView, p: &Person, items: &[Item], acts: &mut Vec<UiAction>) {
    let its: Vec<&Item> = p.items.iter().map(|k| &items[*k]).collect();
    let popped: Vec<&Item> = its.iter().copied().filter(|i| i.popped).collect();
    let ring = its.iter().any(|i| audible(cx, i));
    let a = theme::STRIP_AVATAR;
    // One centre line for everything: the avatar rect snapped to whole pixels.
    let av = Rect::from_min_size(pos2(cell.center().x - a / 2.0, cell.top() + TOP_PAD), vec2(a, a)).round_to_pixels(ui.pixels_per_point());
    let hit = Rect::from_min_max(pos2(cell.left(), av.top() - 6.0), pos2(cell.right(), av.bottom() + 7.0 + 14.0));
    let tag = if p.you { "you" } else { &p.name };
    drive_dump::rect(|| format!("strip.{tag}"), av);
    drive_dump::line(|| format!("person {tag} streams {} popped {} ring {}", its.len(), popped.len(), u8::from(ring)));
    let resp = ui.interact(hit, Id::new(("strip_person", &p.name)), if popped.is_empty() { Sense::hover() } else { Sense::click() });
    crate::ui::paint_person(ui, av, &p.name, &p.emoji, false);
    let c = av.center();
    let painter = ui.painter();
    if resp.hovered() {
        painter.circle_filled(c, a / 2.0, Color32::from_white_alpha(14));
    }
    if ring {
        painter.circle_stroke(c, a / 2.0 + 3.0, Stroke::new(2.0_f32, theme::SUCCESS));
    }
    if !popped.is_empty() {
        let r = av.expand(4.0);
        widgets::paint_dashed_rect(painter, r, CornerRadius::same((r.height() / 2.0).ceil() as u8), Stroke::new(2.0_f32, theme::ACCENT_GHOST_BORDER), 4.0, 3.0);
    }
    let border = Stroke::new(2.0_f32, theme::STAGE_BG);
    if !its.is_empty() {
        let live = Rect::from_center_size(pos2(c.x, av.bottom() + 6.0 - 9.0), vec2(34.0, 18.0));
        drive_dump::rect(|| format!("strip.{tag}.live"), live);
        painter.rect(live, CornerRadius::same(4), theme::SUCCESS, border, egui::StrokeKind::Inside);
        let g = text_galley(ui, &tr("ui.strip.live", "LIVE"), font(Weight::Bold, 8.5), theme::PRIMARY_TEXT, 30.0, 0.5);
        paint_galley(ui, live, g, Align::Center);
    }
    let painter = ui.painter();
    if its.len() > 1 {
        let txt = its.len().to_string();
        let w = (text_width(ui, &txt, font(Weight::Mono, 10.5), 0.0) + 12.0).max(20.0);
        let r = Rect::from_min_size(pos2(av.left() - 6.0, av.top() - 5.0), vec2(w, 20.0));
        painter.rect(r, CornerRadius::same(10), theme::PANEL_RAISED, border, egui::StrokeKind::Inside);
        paint_text(ui, r, &txt, font(Weight::Mono, 10.5), theme::TEXT, Align::Center);
    }
    if !popped.is_empty() {
        let r = Rect::from_min_size(pos2(av.right() + 6.0 - 20.0, av.top() - 5.0), vec2(20.0, 20.0));
        painter.circle(r.center(), 10.0, theme::ACCENT, border);
        paint_icon(ui, r, "open_in_new", 12.0, theme::PRIMARY_TEXT);
    }
    let name = if p.you { tr("ui.room.you", "You") } else { p.name.clone() };
    let nr = Rect::from_center_size(pos2(c.x, av.bottom() + 7.0 + 7.0), vec2(CELL_W, 14.0));
    let col = if its.is_empty() { theme::TEXT_DIM } else { theme::TEXT };
    paint_text(ui, nr, &name, font(Weight::Regular, theme::FS_TINY), col, Align::Center);

    if !popped.is_empty() {
        let tip = tr_with("ui.strip.bring_all_tip", "Bring %{name}'s streams back to the stage", &[("name", &name)]);
        if resp.clone().on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip).clicked() {
            for it in &popped {
                if let Kind::Video(key) = &it.kind {
                    acts.push(UiAction::BringBack(*key));
                }
            }
        }
    }
    hover_card(ui, cx, &resp, av, view, p, &its, acts);
}

/// Card width: one row's worth of numbers needs 320, a person without streams only a name.
const CARD_W: f32 = 320.0;
const CARD_W_PLAIN: f32 = 260.0;
const ROW_H: f32 = 8.0 + 22.0 + 3.0 + 14.0 + 3.0 + 14.0;

#[allow(clippy::too_many_arguments)]
fn hover_card(ui: &Ui, cx: &Cx, resp: &egui::Response, av: Rect, view: &StageView, p: &Person, its: &[&Item], acts: &mut Vec<UiAction>) {
    let ctx = ui.ctx();
    let id = Id::new(("strip_card", &p.name));
    let shown: Option<Rect> = ctx.data(|d| d.get_temp(id));
    let keep = shown.is_some_and(|r| ctx.pointer_hover_pos().is_some_and(|q| card_keeps(r, av, q)));
    if !(resp.hovered() || keep) {
        ctx.data_mut(|d| d.remove::<Rect>(id));
        return;
    }
    let width = if its.is_empty() { CARD_W_PLAIN } else { CARD_W };
    let screen = ctx.content_rect();
    let x = (av.center().x - width / 2.0).clamp(screen.left() + 8.0, (screen.right() - width - 8.0).max(screen.left() + 8.0));
    let area = egui::Area::new(id)
        .order(Order::Foreground)
        .pivot(egui::Align2::LEFT_BOTTOM)
        .fixed_pos(pos2(x, av.top() - 12.0))
        .show(ctx, |ui| {
            crate::ui::menu::frame().corner_radius(CornerRadius::same(12)).fill(theme::PANEL_BASE).inner_margin(egui::Margin::same(12)).show(ui, |ui| {
                ui.set_width(width - 24.0);
                ui.spacing_mut().item_spacing.y = 10.0;
                card_body(ui, cx, view, p, its, acts);
            });
        });
    drive_dump::rect(|| "card".into(), area.response.rect);
    ctx.data_mut(|d| d.insert_temp(id, area.response.rect));
}

/// Whether the pointer at `q` keeps the card `card` of the avatar `av` open: on the card, on the avatar or in the
/// gap straight between them. `Why` not the whole gap: a card is wider than a person's cell, so the union of card and
/// avatar covers the neighbours' avatars and every card you pass stayed open under the next one (found in the rig).
fn card_keeps(card: Rect, av: Rect, q: egui::Pos2) -> bool {
    let gap = Rect::from_x_y_ranges(av.x_range(), card.bottom()..=av.top());
    [card, av, gap].iter().any(|r| r.expand(3.0).contains(q))
}

fn card_body(ui: &mut Ui, cx: &Cx, view: &StageView, p: &Person, its: &[&Item], acts: &mut Vec<UiAction>) {
    let name = if p.you { tr("ui.room.you", "You") } else { p.name.clone() };
    let sub = match its.len() {
        0 => tr("ui.strip.not_sharing", "Not sharing anything"),
        1 => tr("ui.strip.sharing_one", "Sharing 1 stream"),
        n => tr_with("ui.strip.sharing_n", "Sharing %{count} streams", &[("count", &n.to_string())]),
    };
    let live_l = tr("ui.pill.live", "Live");
    let pill = widgets::Pill::live(&live_l).lead(widgets::PillLead::Dot);
    let pw = pill.width(ui);
    let slots = if its.is_empty() { vec![fixed(32.0), FILL] } else { vec![fixed(32.0), FILL, fixed(pw)] };
    let head = HBox::new("card_head", 32.0).gap(10.0).slots(&slots).show(ui);
    crate::ui::paint_person(ui, Rect::from_center_size(head.cell(0).center(), vec2(32.0, 32.0)), &p.name, &p.emoji, false);
    let c = head.cell(1);
    paint_text(ui, Rect::from_x_y_ranges(c.x_range(), c.center().y - 16.0..=c.center().y), &name, font(Weight::Bold, 13.5), theme::TEXT, Align::Min);
    paint_text(ui, Rect::from_x_y_ranges(c.x_range(), c.center().y..=c.center().y + 14.0), &sub, font(Weight::Regular, theme::FS_TINY), theme::TEXT_DIM, Align::Min);
    if !its.is_empty() {
        pill.paint(ui, Rect::from_center_size(head.cell(2).center(), vec2(pw, theme::PILL_H)));
    }
    ui.spacing_mut().item_spacing.y = 0.0;
    for (i, it) in its.iter().enumerate() {
        if i > 0 {
            ui.add_space(10.0); // the replica's rows stand apart: the gap sits above the next row's line
        }
        drive_dump::rect(|| format!("card.row.{i}"), Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), ROW_H)));
        stream_row(ui, cx, view, i, it, acts);
    }
}

/// One stream of the card: `[label][audio][Bring back]`, the numbers, the state.
fn stream_row(ui: &mut Ui, cx: &Cx, view: &StageView, n: usize, it: &Item, acts: &mut Vec<UiAction>) {
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), ROW_H), Sense::hover());
    ui.painter().hline(rect.x_range(), rect.top() + 0.5, Stroke::new(1.0_f32, theme::DIVIDER));
    let l1 = Rect::from_min_size(pos2(rect.left(), rect.top() + 8.0), vec2(rect.width(), 22.0));
    let label = match &it.kind {
        Kind::Mine(..) => mine_label(cx, it),
        Kind::Video(key) | Kind::Idle(key, _) | Kind::Audio(key, _) => stream_label(cx, *key),
    };
    let audio = has_audio(cx, it);
    drive_dump::line(|| format!("cardrow {n} audio {} label={label}", u8::from(audio)));
    let (aicon, atext, acol) = if audio {
        ("volume_up", tr("ui.strip.audio", "Audio"), theme::SUCCESS)
    } else {
        ("volume_off", tr("ui.strip.no_audio", "No audio"), theme::TEXT_FAINT)
    };
    let af = font(Weight::Regular, theme::FS_TINY);
    let aw = 14.0 + 4.0 + text_width(ui, &atext, af.clone(), 0.0).ceil();
    let back = it.popped && matches!(it.kind, Kind::Video(_));
    let back_l = tr("ui.strip.bring_back", "Bring back");
    let bf = font(Weight::Bold, theme::FS_TINY);
    let bw = 6.0 + 13.0 + 4.0 + text_width(ui, &back_l, bf.clone(), 0.0).ceil() + 8.0 + 2.0;
    let slots = if back { vec![FILL, fixed(aw), fixed(bw)] } else { vec![FILL, fixed(aw)] };
    let row = HBox::new("card_row", 22.0).gap(8.0).width(l1.width()).slots(&slots).show(&mut ui.new_child(UiBuilder::new().max_rect(l1)));
    row.text(ui, 0, &label, font(Weight::Bold, theme::FS_SMALL), theme::TEXT, Align::Min);
    let ac = row.cell(1);
    paint_icon(ui, Rect::from_min_size(ac.min, vec2(14.0, ac.height())), aicon, 14.0, acol);
    paint_text(ui, Rect::from_x_y_ranges(ac.left() + 18.0..=ac.right(), ac.y_range()), &atext, af, acol, Align::Min);
    if back && let Kind::Video(key) = &it.kind {
        let br = Rect::from_center_size(row.cell(2).center(), vec2(bw, 22.0));
        drive_dump::rect(|| format!("card.back.{n}"), br);
        let r = ui.interact(br, Id::new(("card_back", key)), Sense::click());
        let fill = if r.hovered() { theme::ACCENT_GHOST_BG_HOVER } else { theme::ACCENT_CHIP_BG };
        ui.painter().rect(br, CornerRadius::same(11), fill, Stroke::new(1.0_f32, theme::ACCENT_CHIP_BORDER), egui::StrokeKind::Inside);
        paint_icon(ui, Rect::from_min_size(pos2(br.left() + 6.0, br.top()), vec2(13.0, 22.0)), "open_in_new", 13.0, theme::ACCENT_LIGHTER);
        paint_text(ui, Rect::from_x_y_ranges(br.left() + 23.0..=br.right() - 8.0, br.y_range()), &back_l, bf, theme::ACCENT_LIGHTER, Align::Min);
        if r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tr("ui.strip.bring_back_tip", "Bring this stream back to the stage")).clicked() {
            acts.push(UiAction::BringBack(*key));
        }
    }
    let mono = font(Weight::Mono, theme::FS_TINY);
    let l2 = Rect::from_min_size(pos2(rect.left(), l1.bottom() + 3.0), vec2(rect.width(), 14.0));
    paint_text(ui, l2, &numbers(cx, it), mono, theme::TEXT_DIM, Align::Min);
    let l3 = Rect::from_min_size(pos2(rect.left(), l2.bottom() + 3.0), vec2(rect.width(), 14.0));
    let (sicon, stext) = state_line(cx, view, it);
    paint_icon(ui, Rect::from_min_size(l3.min, vec2(14.0, 14.0)), sicon, 14.0, theme::ACCENT_LIGHTER);
    paint_text(ui, Rect::from_x_y_ranges(l3.left() + 19.0..=l3.right(), l3.y_range()), &stext, font(Weight::Regular, theme::FS_TINY), theme::TEXT_FAINT, Align::Min);
}

fn mine_label(cx: &Cx, it: &Item) -> String {
    match &it.kind {
        Kind::Mine(capture, _) => cx.capture(*capture).map(|c| crate::ui::share_label(&c.label)).unwrap_or_default(),
        _ => String::new(),
    }
}

fn stream_label(cx: &Cx, key: crate::ui::StreamKey) -> String {
    cx.session(key.0).and_then(|s| s.streams.iter().find(|i| i.owner == key.1 && i.stream_id == key.2)).map(|i| crate::ui::share_label(&i.label)).unwrap_or_default()
}

/// `1920×1080 · 60 fps · H.265 · 6.0 Mbps` for what is known: everything for a watched stream or your own,
/// the announced size for the rest.
fn numbers(cx: &Cx, it: &Item) -> String {
    match &it.kind {
        Kind::Mine(capture, dest) => {
            let Some((c, d)) = cx.capture(*capture).and_then(|c| c.destinations.get(dest).map(|d| (c, d))) else { return String::new() };
            let _ = c;
            let p = d.live_params();
            format!("{}×{} · {} fps · {} · {}", p.width, p.height, p.framerate, simple_screensharing_client::presets::codec_label(p.codec), crate::ui::share::fmt_rate(crate::ui::share::dest_kbps(cx, *dest, d)))
        }
        Kind::Video(key) => {
            let size = cx.videos.frame_size(*key).map(|(w, h)| format!("{w}×{h}")).unwrap_or_default();
            let Some(entry) = cx.videos.get(*key) else { return size };
            let stats = entry.view.lock().unwrap().source().stats();
            let kbps = cx.session(key.0).and_then(|s| s.watches.lock().unwrap().get(&(key.1, key.2)).map(|w| w.decoder.lock().unwrap().received_bitrate_kbps())).unwrap_or(0.0);
            let codec = stats.info.codec.map(|c| c.name()).unwrap_or("—");
            format!("{size} · {:.0} fps · {codec} · {}", stats.decoded_fps, super::tile::fmt_kbps(kbps))
        }
        Kind::Idle(_, info) => aspect_label(info.width, info.height),
        Kind::Audio(..) => tr("ui.room.audio_only", "Audio only"),
    }
}

/// "1920×1080", or nothing when the size is not announced.
fn aspect_label(w: u16, h: u16) -> String {
    if w > 0 && h > 0 { format!("{w}×{h}") } else { String::new() }
}

/// The state line: icon and text (`On the stage · room`; the room only in "Watched"), your own count of viewers.
fn state_line(cx: &Cx, view: &StageView, it: &Item) -> (&'static str, String) {
    let (icon, mut text) = match &it.kind {
        Kind::Mine(..) => ("cast", tr("ui.strip.your_stream", "Your stream")),
        _ if it.popped => ("open_in_new", tr("ui.strip.popped", "Popped out")),
        Kind::Video(_) => ("visibility", tr("ui.strip.on_stage", "On the stage")),
        Kind::Audio(key, _) if cx.is_watching(*key) => ("volume_up", tr("ui.strip.listening", "Listening")),
        _ => ("visibility_off", tr("ui.strip.not_watching", "Not watching")),
    };
    if matches!(view, StageView::Watched) {
        text = format!("{text} · {}", cx.room_name(it.id.room));
    }
    if let Kind::Mine(_, dest) = &it.kind
        && let Some(n) = viewers(cx, it.id.room, *dest)
    {
        text = format!("{text} · {}", tr_with("ui.strip.watching_n", "%{count} watching", &[("count", &n.to_string())]));
    }
    (icon, text)
}

/// How many people watch our share in `room`, when the relay reports it.
fn viewers(cx: &Cx, room: RoomHandle, dest: simple_screensharing_client::session::DestinationId) -> Option<usize> {
    let stream_id = cx.state.captures.values().find_map(|c| c.destinations.get(&dest).map(|d| d.stream_id))?;
    super::tile::viewer_names(cx, room, stream_id).map(|v| v.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_stays_open_over_itself_its_avatar_and_the_gap_but_not_over_the_next_avatar() {
        let av = Rect::from_min_size(pos2(1113.0, 941.0), vec2(44.0, 44.0));
        let card = Rect::from_min_max(pos2(936.0, 797.0), pos2(1258.0, 929.0));
        for q in [pos2(1000.0, 850.0), av.center(), pos2(av.center().x, 935.0)] {
            assert!(card_keeps(card, av, q), "{q:?}");
        }
        assert!(!card_keeps(card, av, pos2(1174.0, 963.0)), "the next person's avatar");
        assert!(!card_keeps(card, av, pos2(1018.0, 963.0)), "the previous person's avatar");
    }
}
