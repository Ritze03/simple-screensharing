//! Bottom action bar (owner: I15), room-scoped, one layout per
//! [`ShareState`] of the selected room (`cx.share_state(&room)`):
//! - Idle: `[Share screen ▾][Share audio][fill status text]` (▾ +
//!   [`PopoverKind::ShareMenu`] when another capture is live elsewhere)
//! - Picking / Staged: `[84 pill][fill status][audio][Stream settings][sep][Discard][Go live]`
//! - Live: `[84 pill][fill status][audio control][Stream settings][N rooms ▾][sep][Stop sharing]`
//!   (`N rooms ▾` = [`PopoverKind::Rooms`]; audio = [`PopoverKind::AudioSource`])
//! - Paused (room reconnecting): `[84 pill][fill status]`
//!
//! Region: the padded 56 px band (pad 16), left-to-right, `PANEL_SUNKEN`
//! with the `DIVIDER` top line (same y as the user band's). Only called
//! while the selected room is joined or reconnecting. Buttons are 36 px
//! (`Btn::large`).
//!
//! **STUB**: plain buttons per state so sharing can be driven.

use egui::{Align, Ui};

use super::{Cx, Modal, ShareState, UiAction};
use crate::theme::{self, Weight, font};
use crate::widgets::Btn;

/// Action-bar UI state (I15: add fields freely).
#[derive(Default)]
pub struct ActionBarState {}

/// The bar for the selected room.
pub fn show(ui: &mut Ui, cx: &mut Cx, _st: &mut ActionBarState) {
    let Some(room) = cx.selected_room() else { return };
    let state = cx.share_state(&room);
    let (label, btns): (String, Vec<(String, &'static str, UiAction)>) = match (state, room.handle) {
        (ShareState::Idle, Some(h)) => (
            "idle".into(),
            vec![(t!("ui.bar.share_screen").to_string(), "primary", UiAction::StageShare(h))],
        ),
        (ShareState::Picking { capture }, _) => (
            t!("ui.bar.picking").to_string(),
            vec![(t!("ui.bar.discard").to_string(), "secondary", UiAction::Discard(capture))],
        ),
        (ShareState::Staged { capture }, Some(h)) => (
            "staged".into(),
            vec![
                (t!("ui.bar.stream_settings").to_string(), "secondary", UiAction::OpenModal(Modal::StreamSettings { capture, room: h })),
                (t!("ui.bar.discard").to_string(), "secondary", UiAction::Discard(capture)),
                (t!("ui.bar.go_live").to_string(), "primary", UiAction::GoLive { capture, room: h }),
            ],
        ),
        (ShareState::Live { capture, destination }, Some(h)) => (
            "live".into(),
            vec![
                (t!("ui.bar.stream_settings").to_string(), "secondary", UiAction::OpenModal(Modal::StreamSettings { capture, room: h })),
                (t!("ui.bar.stop_sharing").to_string(), "danger", UiAction::StopDestination(destination)),
            ],
        ),
        _ => (t!("ui.bar.paused").to_string(), vec![]),
    };
    let b: Vec<Btn> = btns
        .iter()
        .map(|(l, kind, _)| match *kind {
            "primary" => Btn::primary(l).large(),
            "danger" => Btn::danger_solid(l).large(),
            _ => Btn::secondary(l).large(),
        })
        .collect();
    let (row, clicked) = super::fill_then_buttons(ui, "action_band", &b);
    row.text(ui, 0, &format!("[action bar stub: {label}]"), font(Weight::Mono, theme::FS_TINY), theme::TEXT_FAINT, Align::Min);
    if let Some(i) = clicked {
        cx.act(btns[i].2.clone());
    }
}
