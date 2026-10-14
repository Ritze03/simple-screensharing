//! Every modal except stream settings (owner: I16): create/join, settings,
//! leave confirm, forget confirm, rename. Each follows `widgets::modal_shell`
//! (head band / scrolling body of cards with ctrl rows / footer
//! `[fill][buttons]`). Called every frame while `cx.nav.modal` is one of
//! these; close with [`UiAction::CloseModal`] (or the action that finishes
//! the job — `Forget`, `LeaveConfirmed`, `Rename` close their modal).
//!
//! Create/join: Join pushes [`UiAction::JoinRoom`] (failure lands in
//! `cx.shell.join_error`, success closes the modal); Create pushes
//! [`UiAction::CreateRoom`] (minted link in `cx.shell.create_result`), then
//! "Open room" joins it with `JoinRoom`.
//!
//! **STUB**: minimal forms so a room can be joined.

use egui::Ui;

use super::{Cx, Modal, UiAction};
use crate::theme;
use crate::widgets::{self, Btn, FooterClick, ModalSpec};

/// Modals UI state (I16: add fields freely).
#[derive(Default)]
pub struct ModalsState {
    join_link: String,
    room_name: String,
}

/// Draw `modal`.
pub fn show(ctx: &egui::Context, cx: &mut Cx, st: &mut ModalsState, modal: &Modal) {
    let cancel = t!("ui.modal.cancel");
    match modal {
        Modal::CreateJoin => {
            let title = t!("ui.sidebar.create_or_join");
            let spec = ModalSpec { id: "create_join", title: &title, sub: None, width: theme::MODAL_W, head_pill: None };
            let join = t!("ui.modal.join");
            let err = cx.shell.join_error.clone();
            let connecting = cx.shell.awaiting_join.is_some();
            let mut click = FooterClick::None;
            let a = widgets::modal_shell(
                ctx,
                &spec,
                |ui: &mut Ui| {
                    widgets::card(ui, &t!("ui.modal.join_tab"), None, |ui| {
                        widgets::ctrl_row(ui, &t!("ui.modal.join_link"), None, |ui| widgets::text_field(ui, &mut st.join_link, "server|room-id|key"));
                        widgets::ctrl_row(ui, &t!("ui.modal.room_name"), None, |ui| widgets::text_field(ui, &mut st.room_name, ""));
                    });
                    if let Some(e) = &err {
                        widgets::banner(ui, widgets::BannerKind::Warn, "error", e, None);
                    }
                },
                |ui| click = widgets::modal_footer(ui, &[], None, &[Btn::secondary(&cancel), Btn::primary(&join).icon("login").enabled(!connecting)]),
            );
            match click {
                FooterClick::Trailing(1) => cx.act(UiAction::JoinRoom { join_string: st.join_link.clone(), room_name: st.room_name.clone() }),
                FooterClick::Trailing(0) => cx.act(UiAction::CloseModal),
                _ if a.close => cx.act(UiAction::CloseModal),
                _ => {}
            }
        }
        Modal::LeaveConfirm { room } => {
            let title = t!("ui.modal.leave_title", room = cx.room_name(*room));
            let ok = t!("ui.modal.leave_room");
            confirm(ctx, cx, "leave_confirm", &title, &ok, UiAction::LeaveConfirmed(*room));
        }
        Modal::ForgetConfirm { room } => {
            let name = cx.room(room).map(|r| r.name).unwrap_or_default();
            let title = t!("ui.modal.forget_title", room = name);
            let ok = t!("ui.room.forget");
            confirm(ctx, cx, "forget_confirm", &title, &ok, UiAction::Forget(room.clone()));
        }
        Modal::Settings | Modal::Rename { .. } => {
            let title = if *modal == Modal::Settings { t!("ui.sidebar.settings") } else { t!("ui.room.rename") };
            let spec = ModalSpec { id: "stub_modal", title: &title, sub: None, width: theme::MODAL_W_WIDE, head_pill: None };
            let done = t!("ui.stream.done");
            let mut click = FooterClick::None;
            let a = widgets::modal_shell(
                ctx,
                &spec,
                |ui| {
                    super::placeholder(ui, "modal body (I16)", 160.0);
                },
                |ui| click = widgets::modal_footer(ui, &[], None, &[Btn::primary(&done)]),
            );
            if a.close || click != FooterClick::None {
                cx.act(UiAction::CloseModal);
            }
        }
        Modal::StreamSettings { .. } => {}
    }
}

fn confirm(ctx: &egui::Context, cx: &mut Cx, id: &str, title: &str, ok: &str, action: UiAction) {
    let spec = ModalSpec { id, title, sub: None, width: theme::MODAL_W, head_pill: None };
    let cancel = t!("ui.modal.cancel");
    let mut click = FooterClick::None;
    let a = widgets::modal_shell(
        ctx,
        &spec,
        |ui| {
            super::placeholder(ui, "confirm body (I16)", 48.0);
        },
        |ui| click = widgets::modal_footer(ui, &[], None, &[Btn::secondary(&cancel), Btn::danger_solid(ok)]),
    );
    match click {
        FooterClick::Trailing(1) => cx.act(action),
        FooterClick::Trailing(0) => cx.act(UiAction::CloseModal),
        _ if a.close => cx.act(UiAction::CloseModal),
        _ => {}
    }
}
