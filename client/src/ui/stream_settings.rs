//! The stream-settings modal (owner: I15), for [`Modal::StreamSettings`]:
//! staged ("Set up your stream", footer `[Discard][fill][Later][Go live in X]`)
//! when the capture has no destination yet, live ("Stream settings",
//! `[Stop sharing in X][fill][✓ Saved][Done]`) otherwise. Body: two columns
//! (Source / Sending to / Upload bandwidth | Video / Encoder), see the plan's
//! `ui-tree.md`. Draw it with `widgets::modal_shell` at `theme::MODAL_W_XWIDE`.
//! Owns [`super::PopoverKind::AlsoSendTo`] / [`super::PopoverKind::MoveTo`].
//!
//! Staged edits go through [`UiAction::SetNextStream`] /
//! [`UiAction::ApplyPresetToNextStream`]; live edits through
//! [`UiAction::EditDestination`] / [`UiAction::ApplyPresetToDestination`]
//! (keep the `DestEdit` being dragged in [`StreamSettingsState`], push on
//! release). Called every frame while the modal is open; the shell closes it
//! if the capture disappears.
//!
//! **STUB**: chrome + footer actions only.

use simple_screensharing_client::session::{CaptureSourceId, RoomHandle};

use super::{Cx, UiAction};
use crate::theme;
use crate::widgets::{self, Btn, FooterClick, ModalSpec};

/// Stream-settings modal state (I15: add fields freely).
#[derive(Default)]
pub struct StreamSettingsState {}

/// Draw the modal for `capture` as seen from `room`.
pub fn show(ctx: &egui::Context, cx: &mut Cx, _st: &mut StreamSettingsState, capture: CaptureSourceId, room: RoomHandle) {
    let Some(c) = cx.capture(capture) else { return };
    let staged = c.destinations.is_empty();
    let room_name = cx.room_name(room);
    let title = if staged { t!("ui.stream.title_staged") } else { t!("ui.stream.title_live") };
    let sub = format!("{} · {room_name}", c.label);
    let spec = ModalSpec { id: "stream_settings", title: &title, sub: Some(&sub), width: theme::MODAL_W_XWIDE, head_pill: None };
    let mut click = FooterClick::None;
    let discard = t!("ui.bar.discard");
    let later = t!("ui.stream.later");
    let go_live = t!("ui.bar.go_live");
    let stop = t!("ui.bar.stop_sharing");
    let done = t!("ui.stream.done");
    let action = widgets::modal_shell(
        ctx,
        &spec,
        |ui| {
            super::placeholder(ui, "stream settings body (I15)", 240.0);
        },
        |ui| {
            click = if staged {
                widgets::modal_footer(ui, &[Btn::secondary(&discard)], None, &[Btn::secondary(&later), Btn::primary(&go_live).icon("cast")])
            } else {
                widgets::modal_footer(ui, &[Btn::danger(&stop).icon("stop_screen_share")], None, &[Btn::primary(&done)])
            };
        },
    );
    let dest = c.destinations.iter().find(|(_, d)| d.room == room).map(|(id, _)| *id);
    match (staged, click) {
        (true, FooterClick::Leading(0)) => cx.act(UiAction::Discard(capture)),
        (true, FooterClick::Trailing(1)) => cx.act(UiAction::GoLive { capture, room }),
        (false, FooterClick::Leading(0)) => {
            if let Some(d) = dest {
                cx.act(UiAction::StopDestination(d));
            }
        }
        (_, FooterClick::Trailing(_)) => cx.act(UiAction::CloseModal),
        _ if action.close => cx.act(UiAction::CloseModal),
        _ => {}
    }
}
