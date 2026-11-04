//! The audio settings menu (task I7, replica `audioModal`): the big menu the bar's audio dropdown opens with
//! "All audio settings…" ([`Modal::AudioSettings`]), like the stream settings are the screen share's. It holds what
//! used to be in the general Settings plus the new exclusion list:
//!
//! - **Source** card: the Audio field (No audio, Desktop audio, then every app under its group's heading, the
//!   app's own menu style, [`share::SourceOptions`]). Picking acts at once, like the bar's list.
//! - **Volume** card: Boost, 50-200 % (#114, #119; moved here from Settings > Shared audio). The slider applies to
//!   the running share on every changed frame of a drag and saves on release (#135).
//! - **Desktop audio** card: **Exclude**, a list of texts. A source whose name contains one (ignoring case) stays out
//!   of desktop audio and off every source list. "Add" opens [`PopoverKind::AudioExclude`]: the sources detected right
//!   now (one click adds one) or any text typed in. Each entry has a remove button. Discord and TeamSpeak are on the
//!   list by default (`AppSettings::audio_exclusions`).
//! - Footer: `[Stop sharing audio | Share desktop audio][fill][Done]`.
//!
//! `Why a menu of its own and not rows in Settings:` the user's call (D1): the audio button is the one place audio
//! is chosen, so its settings sit behind it, the way the screen button's stream settings do.
//!
//! The rig's `SSS_UI_DUMP` gets `audio.source`, `audio.boost`, `audio.excl_add`, `audio.excl.<i>`,
//! `audio.excl.<i>.remove`, `audio.excl_text` and `audio.excl_add_text` rects and `boost` / `excl` lines.

use egui::{Align, CornerRadius, Rect, Stroke, StrokeKind, Ui};
use simple_screensharing_client::audio::AudioChoice;
use simple_screensharing_client::audio_gain;
use simple_screensharing_client::session::RoomHandle;

use super::share::{POPOVER_W, SourceOptions, pop_heading, pop_note, pop_sep};
use super::{Cx, Popover, PopoverKind, UiAction, drive_dump, menu, popover, tr};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, Btn, FILL, FooterClick, HBox, IconBtnKind, ModalSpec, PopItem, fixed};

/// Audio settings menu state.
#[derive(Default)]
pub struct AudioSettingsState {
    /// The room the menu was last drawn for: a new one re-seeds the buffers and re-lists the sources.
    opened_for: Option<RoomHandle>,
    /// Boost slider, in dB (`audio_gain::gain_to_db` of the setting).
    gain_db: f32,
    /// The setting `gain_db` was last in step with: when the state moves away from it by something other than
    /// our own push, the slider follows.
    gain_seen: f64,
    /// The slider was edited and not saved yet (saved once the pointer is released).
    gain_dirty: bool,
    /// The text field of the Add menu.
    text: String,
    /// Focus the text field on its first frame.
    focus_text: bool,
}

/// What the Boost slider pushes this frame (#135): while the pointer is down a live, unsaved
/// [`UiAction::PreviewAudioGain`] whenever the value moved; once settled one [`UiAction::SetAudioGain`]
/// that saves it (always, because the previews already made `applied` equal to `gain`).
pub(super) fn boost_action(gain: f64, applied: f64, settled: bool) -> Option<UiAction> {
    if settled {
        Some(UiAction::SetAudioGain(gain))
    } else if (gain - applied).abs() > 1e-9 {
        Some(UiAction::PreviewAudioGain(gain))
    } else {
        None
    }
}

/// `list` with `name` appended unless it is there already (ignoring case) or blank.
fn with_entry(list: &[String], name: &str) -> Option<Vec<String>> {
    let name = name.trim();
    if name.is_empty() || list.iter().any(|e| e.to_lowercase() == name.to_lowercase()) {
        return None;
    }
    let mut out = list.to_vec();
    out.push(name.to_string());
    Some(out)
}

/// Draw the menu for the audio share in `room`.
pub fn show(ctx: &egui::Context, cx: &mut Cx, st: &mut AudioSettingsState, room: RoomHandle) {
    if st.opened_for != Some(room) {
        *st = AudioSettingsState {
            opened_for: Some(room),
            gain_db: audio_gain::gain_to_db(cx.state.audio_gain) as f32,
            gain_seen: cx.state.audio_gain,
            ..Default::default()
        };
        cx.act(UiAction::RefreshAudioSources);
    }
    // Follow the state when something else changed the boost.
    if !st.gain_dirty && (cx.state.audio_gain - st.gain_seen).abs() > 1e-9 {
        st.gain_db = audio_gain::gain_to_db(cx.state.audio_gain) as f32;
        st.gain_seen = cx.state.audio_gain;
    }
    let current = cx.audio_choice(room);
    let title = tr("ui.audio_settings.title", "Audio settings");
    let sub = match &current {
        Some(c) => tr("ui.audio_settings.sub_on", "Sharing %{source}").replace("%{source}", &super::share_label(c.label())),
        None => tr("ui.audio_settings.sub_off", "Not sharing audio"),
    };
    let spec = ModalSpec { id: "audio_settings", title: &title, sub: Some(&sub), width: theme::MODAL_W_WIDE, head_pill: None };
    let stop = tr("ui.audio.stop", "Stop sharing audio");
    let share = tr("ui.audio_settings.share_desktop", "Share desktop audio");
    let done = tr("ui.stream.done", "Done");

    let mut out: Vec<UiAction> = Vec::new();
    let mut add_anchor: Option<Rect> = None;
    let mut click = FooterClick::None;
    let cxr: &Cx = cx;
    let a = widgets::modal_shell(
        ctx,
        &spec,
        |ui| body(ui, cxr, st, room, &mut out, &mut add_anchor),
        |ui| {
            let lead = if current.is_some() { Btn::danger(&stop).icon("music_off") } else { Btn::secondary(&share).icon("music_note") };
            click = widgets::modal_footer(ui, &[lead], None, &[Btn::primary(&done)]);
        },
    );
    // The boost slider (#135): applied to the running shares on every changed frame of the drag (the user wants
    // to hear the level while choosing it), saved once the pointer is released.
    let closing = a.close || click != FooterClick::None;
    let settled = closing || !ctx.input(|i| i.pointer.any_down());
    if st.gain_dirty {
        let gain = audio_gain::clamp_gain(audio_gain::db_to_gain(st.gain_db as f64));
        if let Some(act) = boost_action(gain, cx.state.audio_gain, settled) {
            out.push(act);
        }
        if settled {
            st.gain_dirty = false;
            st.gain_seen = gain;
        }
    }
    for act in out {
        cx.act(act);
    }
    match click {
        FooterClick::Leading(0) => cx.act(if current.is_some() {
            UiAction::StopSharingAudio { room }
        } else {
            UiAction::ShareAudio { room, choice: AudioChoice::Desktop }
        }),
        FooterClick::Trailing(_) => cx.act(UiAction::CloseModal),
        // Escape with the Add menu open only closes that menu (the shell consumes it first).
        _ if a.close && cx.nav.popover.is_none() => cx.act(UiAction::CloseModal),
        _ => {}
    }

    // The open Add menu first, then its button: a click outside the menu that lands on the button closes the menu
    // before the button's OpenPopover is queued (actions apply in order).
    exclude_popover(ctx, cx, st);
    if let Some(r) = add_anchor {
        if cx.popover_of(&PopoverKind::AudioExclude).is_none() {
            st.text.clear();
            st.focus_text = true;
        }
        cx.toggle_popover(PopoverKind::AudioExclude, r);
    }
}

fn body(ui: &mut Ui, cx: &Cx, st: &mut AudioSettingsState, room: RoomHandle, out: &mut Vec<UiAction>, add_anchor: &mut Option<Rect>) {
    // Source.
    let options = SourceOptions::new(cx, room);
    let mut idx = options.selected;
    let before = idx;
    widgets::card(ui, &tr("ui.audio_settings.source_card", "Source"), None, |ui| {
        let opts = options.opts();
        let r = widgets::ctrl_row(ui, &tr("ui.stream.audio", "Audio"), None, |ui| menu::select(ui, "audio_source", &mut idx, &opts));
        drive_dump::rect(|| "audio.source".into(), r.rect);
    });
    if idx != before {
        out.push(options.action(room, idx));
    }

    // Volume: Boost. `Why the slider is in dB:` each doubling is the same distance and 100 % sits in the middle of
    // the track (-6 dB .. +6 dB). The value shown is still the percent, what the viewers' sliders use.
    widgets::card(ui, &tr("ui.audio_settings.volume_card", "Volume"), None, |ui| {
        let percent = format!("{}%", (audio_gain::db_to_gain(st.gain_db as f64) * 100.0).round());
        let (r, _) = widgets::ctrl_row_value(
            ui,
            &tr("ui.settings.audio_boost", "Boost"),
            Some(&tr(
                "ui.settings.audio_boost_tip",
                "Makes the audio you share louder (or quieter) before it is sent, up to 200%. A limiter keeps the loud peaks from distorting. 100% leaves it as it is. Applies to a share that is already running. Viewers level the loudness themselves, so you rarely need this.",
            )),
            |ui| widgets::slider(ui, &mut st.gain_db, audio_gain::gain_db_range(), 0.5),
            |ui| widgets::value_box(ui, &percent),
        );
        drive_dump::rect(|| "audio.boost".into(), r.rect);
        drive_dump::line(|| format!("boost {percent}"));
        st.gain_dirty |= r.changed();
    });

    // Desktop audio: the exclusion list.
    let list = cx.state.audio_exclusions.get();
    widgets::card(ui, &tr("ui.audio_settings.desktop_card", "Desktop audio"), None, |ui| {
        let add = tr("ui.audio_settings.add", "Add");
        widgets::ctrl_row(
            ui,
            &tr("ui.audio_settings.exclude", "Exclude"),
            Some(&tr("ui.audio_settings.exclude_tip", "Any source whose name contains this text, ignoring case")),
            |ui| {
                let btn = Btn::secondary(&add).icon("add");
                let w = btn.width(ui);
                let row = HBox::new("excl_head", theme::CTL_H).slots(&[FILL, fixed(w)]).show(ui);
                row.text(ui, 0, &tr("ui.audio_settings.exclude_caption", "Left out of desktop audio and hidden from the source list"), font(Weight::Regular, theme::FS_SMALL), theme::TEXT_DIM, Align::Min);
                let r = row.show(ui, 1, Align::Max, |ui| btn.show_sized(ui, w));
                drive_dump::rect(|| "audio.excl_add".into(), r.rect);
                if r.clicked() {
                    *add_anchor = Some(r.rect);
                }
            },
        );
        for (i, entry) in list.iter().enumerate() {
            let row = HBox::new("excl_row", theme::CTL_H).pad(theme::S2, theme::S1).gap(theme::S2).slots(&[fixed(18.0), FILL, fixed(26.0)]).show(ui);
            ui.painter().rect(row.rect, CornerRadius::same(theme::ROUNDING_SM), theme::FIELD, Stroke::new(1.0_f32, theme::BORDER), StrokeKind::Inside);
            drive_dump::rect(|| format!("audio.excl.{i}"), row.rect);
            row.icon(ui, 0, "volume_off", 16.0, theme::TEXT_DIM);
            row.text(ui, 1, entry, font(Weight::Regular, theme::FS_ROW), theme::TEXT, Align::Min);
            let remove = tr("ui.audio_settings.remove", "Remove");
            let r = row.show(ui, 2, Align::Center, |ui| widgets::icon_btn(ui, "close", IconBtnKind::Small, &remove));
            drive_dump::rect(|| format!("audio.excl.{i}.remove"), r.rect);
            if r.clicked() {
                let mut rest = list.clone();
                rest.remove(i);
                out.push(UiAction::SetAudioExclusions(rest));
            }
        }
        drive_dump::line(|| format!("excl {}", list.join("|")));
    });
}

/// The Add menu under the exclusion list when it is open: the sources detected right now (they are already
/// without the excluded ones), then a text field for any other text. Adding closes it.
fn exclude_popover(ctx: &egui::Context, cx: &mut Cx, st: &mut AudioSettingsState) {
    let Some(pop): Option<Popover> = cx.popover_of(&PopoverKind::AudioExclude).cloned() else { return };
    let list = cx.state.audio_exclusions.get();
    let names: Vec<String> = cx.state.audio_sources.iter().map(|s| s.name.clone()).collect();
    let mut picked: Option<Vec<String>> = None;
    let open = popover(ctx, &pop, POPOVER_W, |ui| {
        pop_heading(ui, &tr("ui.audio_settings.detected", "Detected audio sources"));
        if names.is_empty() {
            pop_note(ui, &tr("ui.audio_settings.all_excluded", "Every detected source is excluded"));
        }
        egui::ScrollArea::vertical().max_height(240.0).auto_shrink([true, true]).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            for n in &names {
                if widgets::pop_item(ui, &PopItem::new("volume_up", n)).clicked() {
                    picked = with_entry(&list, n);
                }
            }
        });
        pop_sep(ui);
        pop_heading(ui, &tr("ui.audio_settings.enter", "Or enter a name"));
        let add = tr("ui.audio_settings.add", "Add");
        let btn = Btn::secondary(&add);
        let bw = btn.width(ui);
        let row = HBox::new("excl_enter", theme::CTL_H + 8.0).pad(theme::S2, 4.0).gap(theme::S2).slots(&[FILL, fixed(bw)]).show(ui);
        let field = row.show(ui, 0, Align::Center, |ui| widgets::text_field(ui, &mut st.text, &tr("ui.audio_settings.hint", "Part of a source name")));
        drive_dump::rect(|| "audio.excl_text".into(), field.rect);
        if std::mem::take(&mut st.focus_text) {
            field.request_focus();
        }
        let r = row.show(ui, 1, Align::Center, |ui| btn.show_sized(ui, bw));
        drive_dump::rect(|| "audio.excl_add_text".into(), r.rect);
        let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if r.clicked() || enter {
            picked = with_entry(&list, &st.text);
            if picked.is_none() {
                st.text.clear();
            }
        }
    });
    if let Some(next) = picked {
        cx.act(UiAction::SetAudioExclusions(next));
        st.text.clear();
        cx.act(UiAction::ClosePopover);
    } else if !open {
        cx.act(UiAction::ClosePopover);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_entry_is_added_once_ignoring_case_and_blanks_never() {
        let list = vec!["Discord".to_string()];
        assert_eq!(with_entry(&list, "  Zoom "), Some(vec!["Discord".to_string(), "Zoom".to_string()]));
        assert_eq!(with_entry(&list, "discord"), None, "already there, ignoring case");
        assert_eq!(with_entry(&list, "   "), None);
        assert_eq!(with_entry(&[], "x"), Some(vec!["x".to_string()]));
    }
}
