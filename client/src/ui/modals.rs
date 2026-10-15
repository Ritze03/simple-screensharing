//! Every modal except stream settings (owner: I16): create/join, settings,
//! leave confirm, forget confirm, rename. Each follows `widgets::modal_shell`
//! (head band / scrolling body of cards with ctrl rows / footer
//! `[fill][buttons]`, mockup `modalShell`). Called every frame while
//! `cx.nav.modal` is one of these; close with [`UiAction::CloseModal`] (or the
//! action that finishes the job — `Forget`, `LeaveConfirmed`, `Rename` close
//! their modal). Escape, × and a backdrop click close every one of them.
//!
//! Form buffers live in [`ModalsState`] and are re-seeded from the app state
//! each time a modal opens (see [`ModalsState::sync`]).
//!
//! - **Create / join** (`Modal::CreateJoin`, 520): segmented `Join with a link
//!   | Create a new room`. Join pushes [`UiAction::JoinRoom`] (3- or 4-field
//!   link; failure lands in `cx.shell.join_error`, success closes the modal,
//!   `cx.shell.awaiting_join` shows a progress banner meanwhile). Create pushes
//!   [`UiAction::CreateRoom`] (minted link in `cx.shell.create_result`), then
//!   shows the link — masked, because it carries the room key and this is a
//!   screen-sharing app — with "Join now" (`JoinRoom`; the modal stays open).
//!   Copy is enabled only once that room is connected and copies the live
//!   connection's invite link ([`UiAction::CopyInviteLink`]). `Why:` the
//!   minted link has no relay cert hash, so browsers can't use it; the live
//!   link carries it. The footer then turns into "Done".
//!   The server field is pre-filled with the server of the most
//!   recently saved room. `Why:` the lib keeps no "last server" setting, and
//!   the newest saved room is the last server the user created or joined on.
//! - **Settings** (`Modal::Settings`, 640): cards Profile / Language / Display /
//!   Playback. Text and sliders are buffered and pushed when the edit settles
//!   (focus lost, pointer released), not every frame of a drag, since each
//!   setter writes `settings.toml`.
//! - **Leave / forget confirm** (520): explanation + hatched warnings when we
//!   are live in the room or watching streams there.
//! - **Rename** (520): one Name row, Enter or Save pushes [`UiAction::Rename`].


use egui::{Align, Ui};
use simple_screensharing_client::net::{ServerAddrError, parse_join_string, parse_server_addr};
use simple_screensharing_client::rooms::{AppSettings, PresentationMode};
use simple_screensharing_client::session::RoomHandle;

use super::{Cx, Modal, ShareState, UiAction, tr, tr_with};
use crate::theme::{self, Weight, font};
use crate::widgets::{self, BannerKind, Btn, FooterClick, ModalSpec};


/// Which half of the create/join modal is showing.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AddTab {
    #[default]
    Join,
    Create,
}

/// Modals UI state: form buffers, re-seeded whenever a modal (re)opens.
#[derive(Default)]
pub struct ModalsState {
    /// The modal drawn last pass, and that pass's number (a gap = it was
    /// closed in between, so the buffers are re-seeded).
    open: Option<Modal>,
    seen_pass: u64,
    // create / join
    tab: AddTab,
    join_link: String,
    join_name: String,
    /// The link text `prefill_name` last looked at (it only acts on a change).
    seen_link: String,
    /// The name `prefill_name` put into `join_name` from a pasted link; the
    /// field still holding exactly this means the user did not touch it.
    prefilled_name: Option<String>,
    server: String,
    create_name: String,
    /// The shown `create_result` was dismissed by switching tabs (the shell
    /// keeps it until the modal reopens).
    result_dismissed: bool,
    // settings
    display_name: String,
    jitter_s: f32,
    catch_up: u32,
    // rename
    rename: String,
    // choose name
    choose_name: String,
    /// Focus + select-all the name field on its first frame.
    choose_name_focus: bool,
}

impl ModalsState {
    /// Re-seed the buffers when `modal` was not drawn on the previous pass.
    fn sync(&mut self, ctx: &egui::Context, cx: &Cx, modal: &Modal) {
        let pass = ctx.cumulative_pass_nr();
        let fresh = self.open.as_ref() != Some(modal) || pass != self.seen_pass + 1;
        self.seen_pass = pass;
        if !fresh {
            return;
        }
        self.open = Some(modal.clone());
        match modal {
            Modal::CreateJoin => {
                self.tab = AddTab::Join;
                self.join_link.clear();
                self.join_name.clear();
                self.seen_link.clear();
                self.prefilled_name = None;
                self.create_name.clear();
                self.result_dismissed = false;
                self.server = last_server(cx).unwrap_or_default();
            }
            Modal::Settings => {
                self.display_name = cx.state.display_name.clone();
                self.jitter_s = cx.state.max_jitter_buffer_ms as f32 / 1000.0;
                self.catch_up = cx.state.catch_up_speed_percent;
            }
            Modal::ChooseName => {
                self.choose_name = cx.state.display_name.clone();
                self.choose_name_focus = true;
            }
            Modal::Rename { room } => {
                self.rename = cx.room(room).map(|r| r.name).unwrap_or_default();
            }
            _ => {}
        }
    }
}

impl ModalsState {
    /// When the join-link field changed to a link carrying a room name (5th
    /// field), fill the name field with it, unless the user typed a name of
    /// their own: the field is empty or still holds the previous prefill.
    /// `Why:` user request 2026-10-15, a pasted link pre-fills the name but it
    /// stays editable and what is in the field on "Join" is what is saved.
    fn prefill_name(&mut self) {
        if self.join_link == self.seen_link {
            return;
        }
        self.seen_link.clone_from(&self.join_link);
        let Some(name) = parse_join_string(&self.join_link).ok().and_then(|p| p.name) else { return };
        let untouched = self.join_name.trim().is_empty() || self.prefilled_name.as_deref() == Some(self.join_name.as_str());
        if untouched {
            self.join_name.clone_from(&name);
            self.prefilled_name = Some(name);
        }
    }
}

/// The server of the most recently saved room (`RoomStore` appends new names).
fn last_server(cx: &Cx) -> Option<String> {
    cx.state
        .room_store
        .rooms
        .iter()
        .rev()
        .find_map(|r| parse_join_string(&r.join_string).ok().map(|p| p.server))
}

/// Draw `modal`.
pub fn show(ctx: &egui::Context, cx: &mut Cx, st: &mut ModalsState, modal: &Modal) {
    st.sync(ctx, cx, modal);
    match modal {
        Modal::CreateJoin => create_join(ctx, cx, st),
        Modal::Settings => settings(ctx, cx, st),
        Modal::LeaveConfirm { room } => {
            let name = cx.room_name(*room);
            let title = tr_with("ui.modal.leave_title", "Leave %{room}?", &[("room", &name)]);
            let body = tr(
                "ui.modal.leave_body",
                "You'll disconnect. The room stays in your list so you can reconnect later.",
            );
            let ok = tr("ui.modal.leave_room", "Leave room");
            confirm(
                ctx,
                cx,
                "leave_confirm",
                &title,
                &body,
                Some(*room),
                &ok,
                UiAction::LeaveConfirmed(*room),
            );
        }
        Modal::ForgetConfirm { room } => {
            let entry = cx.room(room);
            let name = entry.as_ref().map(|r| r.name.clone()).unwrap_or_default();
            let title = tr_with("ui.modal.forget_title", "Forget %{room}?", &[("room", &name)]);
            let body = tr(
                "ui.modal.forget_body",
                "You'll leave the room and it disappears from your list. You need the invite link to come back.",
            );
            let ok = tr("ui.room.forget", "Forget room");
            let handle = entry.and_then(|r| r.handle);
            confirm(
                ctx,
                cx,
                "forget_confirm",
                &title,
                &body,
                handle,
                &ok,
                UiAction::Forget(room.clone()),
            );
        }
        Modal::Rename { room } => rename(ctx, cx, st, room),
        Modal::ChooseName => choose_name(ctx, cx, st),
        Modal::StreamSettings { .. } => {}
    }
}

// ---------------------------------------------------------------------------
// Create / join
// ---------------------------------------------------------------------------

fn create_join(ctx: &egui::Context, cx: &mut Cx, st: &mut ModalsState) {
    let title = tr("ui.sidebar.create_or_join", "Create or join a room");
    let spec = ModalSpec {
        id: "create_join",
        title: &title,
        sub: None,
        width: theme::MODAL_W,
        head_pill: None,
    };
    let tab_join = tr("ui.modal.join_tab", "Join with a link");
    let tab_create = tr("ui.modal.create_tab", "Create a new room");
    let cancel = tr("ui.modal.cancel", "Cancel");
    let join = tr("ui.modal.join", "Join");
    let create = tr("ui.modal.create", "Create");
    let join_now = tr("ui.modal.join_now", "Join now");
    let copy = tr("ui.modal.copy", "Copy");

    let awaiting = cx.shell.awaiting_join.is_some();
    let join_error = cx.shell.join_error.clone();
    let created = match &cx.shell.create_result {
        Some(Ok(link)) if !st.result_dismissed => Some(Ok(link.clone())),
        Some(Err(e)) if !st.result_dismissed => Some(Err(e.clone())),
        _ => None,
    };
    let minted = matches!(created, Some(Ok(_))) && st.tab == AddTab::Create;
    // The minted link has no relay cert hash (browsers can't use it), so Copy
    // waits for the live connection and copies its invite link instead
    // (`UiAction::CopyInviteLink`, cert hash included). `App::on_joined` keeps
    // this modal open when the join came from "Join now".
    let joined = match &created {
        Some(Ok(link)) => cx.state.rooms.iter().find(|(_, r)| super::same_room(&r.join_string, &link.join_string)).map(|(h, _)| *h),
        _ => None,
    };
    let done = tr("ui.stream.done", "Done");
    let copy_tip = tr("ui.room.copy_invite_offline_tip", "Connect first: the link includes the relay's certificate");
    // The footer closure can't borrow `st` next to the body's `&mut`: it uses
    // this pass's start values (a tab switch requests a repaint below).
    let tab = st.tab;
    let join_hint = join_link_hint(&st.join_link);
    let create_hint = server_hint(&st.server);
    let can_join = !awaiting && !st.join_link.trim().is_empty() && join_hint.is_none();
    let can_create =
        !st.server.trim().is_empty() && create_hint.is_none() && !st.create_name.trim().is_empty();

    let mut copy_clicked = false;
    let mut submit = false;
    let mut click = FooterClick::None;
    let a = widgets::modal_shell(
        ctx,
        &spec,
        |ui: &mut Ui| {
            let before = st.tab;
            widgets::segmented(
                ui,
                &mut st.tab,
                &[(AddTab::Join, &tab_join), (AddTab::Create, &tab_create)],
            );
            if st.tab != before {
                st.result_dismissed = true;
                ui.ctx().request_repaint();
            }
            match st.tab {
                AddTab::Join => {
                    plain_card(ui, |ui| {
                        let link = widgets::ctrl_row(
                            ui,
                            &tr("ui.modal.join_link", "Join link"),
                            None,
                            |ui| widgets::text_field(ui, &mut st.join_link, "server|room-id|key"),
                        );
                        if let Some(h) = &join_hint {
                            field_hint(ui, h);
                        }
                        st.prefill_name();
                        let name = widgets::ctrl_row(
                            ui,
                            &tr("ui.modal.room_name", "Room name"),
                            Some(&tr("ui.modal.room_name_tip", "Only shown in your own list")),
                            |ui| widgets::text_field(ui, &mut st.join_name, "Friday Films"),
                        );
                        submit |= entered(ui, &link) || entered(ui, &name);
                    });
                }
                AddTab::Create => match &created {
                    Some(Ok(link)) => {
                        paragraph(
                            ui,
                            &tr(
                                "ui.modal.created_body",
                                "Room created. Send this link to your friends; anyone with it can join.",
                            ),
                        );
                        plain_card(ui, |ui| {
                            widgets::ctrl_row(ui, &tr("ui.modal.server", "Server"), None, |ui| {
                                let server = parse_join_string(&link.join_string)
                                    .map(|p| p.server)
                                    .unwrap_or_default();
                                widgets::readout(ui, &server, Align::Min)
                            });
                            widgets::ctrl_row_value(
                                ui,
                                &tr("ui.modal.join_link", "Join link"),
                                Some(&tr(
                                    "ui.modal.link_hidden_tip",
                                    "Hidden because it contains the room key. Copy puts the full link on the clipboard.",
                                )),
                                |ui| {
                                    widgets::readout(
                                        ui,
                                        &masked_link(&link.join_string),
                                        Align::Min,
                                    )
                                },
                                |ui| {
                                    let b = Btn::secondary(&copy).enabled(joined.is_some());
                                    let b = if joined.is_none() { b.tip(&copy_tip) } else { b };
                                    copy_clicked = b.show_sized(ui, theme::VALUE_W).clicked()
                                },
                            );
                        });
                    }
                    _ => {
                        plain_card(ui, |ui| {
                            let server = widgets::ctrl_row(
                                ui,
                                &tr("ui.modal.server", "Server"),
                                Some(&tr(
                                    "ui.modal.server_tip",
                                    "The relay that hosts the room, host:port",
                                )),
                                |ui| {
                                    widgets::text_field(
                                        ui,
                                        &mut st.server,
                                        "relay.example.org:4433",
                                    )
                                },
                            );
                            if let Some(h) = &create_hint {
                                field_hint(ui, h);
                            }
                            let name = widgets::ctrl_row(
                                ui,
                                &tr("ui.modal.room_name", "Room name"),
                                Some(&tr("ui.modal.room_name_tip", "Only shown in your own list")),
                                |ui| widgets::text_field(ui, &mut st.create_name, "Friday Films"),
                            );
                            submit |= entered(ui, &server) || entered(ui, &name);
                        });
                        if let Some(Err(e)) = &created {
                            widgets::banner(ui, BannerKind::Warn, "error", e, None);
                        }
                    }
                },
            }
            // Join progress / failure (Join tab, or "Join now" after Create).
            if st.tab == AddTab::Join || minted {
                if awaiting {
                    widgets::banner(
                        ui,
                        BannerKind::Info,
                        "sync",
                        &tr("ui.modal.joining", "Connecting…"),
                        None,
                    );
                } else if let Some(e) = &join_error {
                    widgets::banner(ui, BannerKind::Warn, "error", e, None);
                }
            }
        },
        |ui| {
            click = if minted && joined.is_some() {
                widgets::modal_footer(ui, &[], None, &[Btn::primary(&done)])
            } else if minted {
                widgets::modal_footer(
                    ui,
                    &[],
                    None,
                    &[Btn::primary(&join_now).icon("login").enabled(!awaiting)],
                )
            } else if tab == AddTab::Join {
                widgets::modal_footer(
                    ui,
                    &[],
                    None,
                    &[
                        Btn::secondary(&cancel),
                        Btn::primary(&join).icon("login").enabled(can_join),
                    ],
                )
            } else {
                widgets::modal_footer(
                    ui,
                    &[],
                    None,
                    &[
                        Btn::secondary(&cancel),
                        Btn::primary(&create).icon("add").enabled(can_create),
                    ],
                )
            };
        },
    );

    if let (true, Some(h)) = (copy_clicked, joined) {
        cx.act(UiAction::CopyInviteLink(h));
    }
    // Footer: `[Join now]` once a room was minted (mockup), else `[Cancel][primary]`.
    let primary_i = if minted { 0 } else { 1 };
    let primary = click == FooterClick::Trailing(primary_i) || submit;
    let cancel_clicked = !minted && click == FooterClick::Trailing(0);
    let done_clicked = primary && minted && joined.is_some();
    if cancel_clicked || done_clicked || (a.close && click == FooterClick::None && !submit) {
        cx.act(UiAction::CloseModal);
    } else if primary && !awaiting {
        if minted {
            if let Some(Ok(link)) = &created {
                cx.act(UiAction::JoinRoom {
                    join_string: link.join_string.clone(),
                    room_name: String::new(),
                });
            }
        } else if st.tab == AddTab::Join {
            if can_join {
                cx.act(UiAction::JoinRoom {
                    join_string: st.join_link.trim().to_string(),
                    room_name: st.join_name.trim().to_string(),
                });
            }
        } else if can_create {
            st.result_dismissed = false;
            cx.act(UiAction::CreateRoom {
                server: st.server.trim().to_string(),
                room_name: st.create_name.trim().to_string(),
            });
        }
    }
}

/// Enter pressed in a single-line field (it loses focus on Enter).
fn entered(ui: &Ui, r: &egui::Response) -> bool {
    r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))
}

/// A join link with the key (and room id tail) hidden: `server|1a2b3c…|••••••`.
fn masked_link(join_string: &str) -> String {
    match parse_join_string(join_string) {
        Ok(p) => {
            let id: String = p
                .room_id
                .iter()
                .take(3)
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("{}|{id}…|••••••••", p.server)
        }
        Err(_) => "••••••••".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

const PRESENTATION_MODES: [PresentationMode; 3] = [
    PresentationMode::Mailbox,
    PresentationMode::AutoVsync,
    PresentationMode::Immediate,
];

fn settings(ctx: &egui::Context, cx: &mut Cx, st: &mut ModalsState) {
    let title = tr("ui.sidebar.settings", "Settings");
    let spec = ModalSpec {
        id: "settings",
        title: &title,
        sub: None,
        width: theme::MODAL_W_WIDE,
        head_pill: None,
    };
    let done = tr("ui.stream.done", "Done");
    let lang_now = if cx.state.language.starts_with("de") {
        "de"
    } else {
        "en"
    };
    let mut lang = lang_now;
    let mut mode_i = PRESENTATION_MODES
        .iter()
        .position(|m| *m == cx.state.presentation_mode)
        .unwrap_or(0);
    let restart = cx.state.presentation_mode != cx.state.launch_presentation_mode;
    let mode_labels = [
        tr("ui.settings.mode_mailbox", "Mailbox (default)"),
        tr("ui.settings.mode_vsync", "Vsync"),
        tr("ui.settings.mode_immediate", "Immediate"),
    ];
    let mode_refs: Vec<&str> = mode_labels.iter().map(|s| s.as_ref()).collect();

    let mut name_done = false;
    let mut click = FooterClick::None;
    let a = widgets::modal_shell(
        ctx,
        &spec,
        |ui: &mut Ui| {
            widgets::card(ui, &tr("ui.settings.profile", "Profile"), None, |ui| {
                let r = widgets::ctrl_row(
                    ui,
                    &tr("ui.settings.display_name", "Display name"),
                    Some(&tr(
                        "ui.settings.display_name_tip",
                        "One name for every room. Rooms you're already in keep the old name until you reconnect.",
                    )),
                    |ui| widgets::text_field(ui, &mut st.display_name, "e.g. mo"),
                );
                name_done = r.lost_focus();
            });
            widgets::card(
                ui,
                &tr("ui.settings.language_card", "Language"),
                None,
                |ui| {
                    widgets::ctrl_row(ui, &tr("ui.settings.language", "Language"), None, |ui| {
                        widgets::segmented(ui, &mut lang, &[("en", "English"), ("de", "Deutsch")])
                    });
                },
            );
            widgets::card(ui, &tr("ui.settings.display", "Display"), None, |ui| {
                widgets::ctrl_row(
                    ui,
                    &tr("ui.settings.presentation", "Presentation"),
                    Some(&tr(
                        "ui.settings.presentation_tip",
                        "How frames reach the monitor.\n\
                         Mailbox (default): no frame-rate cap and no tearing; best for high-refresh viewing and pop-out windows.\n\
                         Vsync: locked to the monitor's refresh rate, no tearing; a pop-out window can slow the main window down.\n\
                         Immediate: no cap, lowest latency, may tear.\n\
                         Takes effect after a restart.",
                    )),
                    |ui| widgets::select(ui, "presentation_mode", &mut mode_i, &mode_refs),
                );
                if restart {
                    widgets::banner(
                        ui,
                        BannerKind::Hatched,
                        "restart_alt",
                        &tr(
                            "ui.settings.restart_banner",
                            "Restart the app to apply the new presentation mode.",
                        ),
                        None,
                    );
                }
            });
            widgets::card(ui, &tr("ui.settings.playback", "Playback"), None, |ui| {
                let max_ms = AppSettings::clamp_max_jitter_buffer_ms(u32::MAX) as f32 / 1000.0;
                widgets::slider_row(
                    ui,
                    &tr("ui.settings.jitter", "Jitter buffer"),
                    Some(&tr(
                        "ui.settings.jitter_tip",
                        "Max jitter buffer. Smooths network hiccups; grows only when needed, up to this ceiling.",
                    )),
                    &mut st.jitter_s,
                    0.0..=max_ms,
                    0.1,
                    1,
                    " s",
                );
                let lo = AppSettings::clamp_catch_up_speed_percent(0);
                let hi = AppSettings::clamp_catch_up_speed_percent(u32::MAX);
                widgets::slider_row(
                    ui,
                    &tr("ui.settings.catch_up", "Catch-up speed"),
                    Some(&tr(
                        "ui.settings.catch_up_tip",
                        "How much faster than live playback runs while shrinking the buffer after a spike.",
                    )),
                    &mut st.catch_up,
                    lo..=hi,
                    1.0,
                    0,
                    "%",
                );
            });
        },
        |ui| click = widgets::modal_footer(ui, &[], None, &[Btn::primary(&done)]),
    );
    let closing = a.close || click != FooterClick::None;

    if lang != lang_now {
        cx.act(UiAction::SetLanguage(lang.to_string()));
    }
    if PRESENTATION_MODES[mode_i] != cx.state.presentation_mode {
        cx.act(UiAction::SetPresentationMode(PRESENTATION_MODES[mode_i]));
    }
    if (name_done || closing)
        && st.display_name.trim() != cx.state.display_name
        && !st.display_name.trim().is_empty()
    {
        cx.act(UiAction::SetDisplayName(st.display_name.trim().to_string()));
    }
    // Sliders: push once the pointer is released (each setter saves to disk).
    let settled = closing || !ctx.input(|i| i.pointer.any_down());
    let jitter_ms = (st.jitter_s * 1000.0).round() as u32;
    if settled && jitter_ms != cx.state.max_jitter_buffer_ms {
        cx.act(UiAction::SetJitterBuffer(jitter_ms));
    }
    if settled && st.catch_up != cx.state.catch_up_speed_percent {
        cx.act(UiAction::SetCatchUpSpeed(st.catch_up));
    }
    if closing {
        cx.act(UiAction::CloseModal);
    }
}

// ---------------------------------------------------------------------------
// Leave / forget confirm, rename
// ---------------------------------------------------------------------------

/// The hatched warnings for leaving `room`: our stream there stops (it keeps
/// going elsewhere if it fans out), and how many watched streams close.
fn leave_warnings(cx: &Cx, room: RoomHandle) -> Vec<String> {
    let mut out = Vec::new();
    let name = cx.room_name(room);
    let entry = cx.rooms().into_iter().find(|r| r.handle == Some(room));
    if let Some(entry) = entry
        && let ShareState::Live { capture, .. } = cx.share_state(&entry)
    {
        let others = cx
            .capture(capture)
            .is_some_and(|c| c.destinations.values().any(|d| d.room != room));
        let mut s = tr(
            "ui.modal.warn_stream_stops",
            "Your stream to %{room} stops.",
        )
        .replace("%{room}", &name);
        if others {
            s.push(' ');
            s.push_str(&tr(
                "ui.modal.warn_stream_elsewhere",
                "It keeps going to your other rooms.",
            ));
        }
        out.push(s);
    }
    let watching = cx
        .shares_in(room)
        .iter()
        .filter(|i| cx.is_watching((room, i.owner, i.stream_id)))
        .count();
    if watching == 1 {
        out.push(
            tr(
                "ui.modal.warn_watching_one",
                "You stop watching 1 stream from this room.",
            ),
        );
    } else if watching > 1 {
        out.push(
            tr(
                "ui.modal.warn_watching_many",
                "You stop watching %{count} streams from this room.",
            )
            .replace("%{count}", &watching.to_string()),
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn confirm(
    ctx: &egui::Context,
    cx: &mut Cx,
    id: &str,
    title: &str,
    body: &str,
    room: Option<RoomHandle>,
    ok: &str,
    action: UiAction,
) {
    let spec = ModalSpec {
        id,
        title,
        sub: None,
        width: theme::MODAL_W,
        head_pill: None,
    };
    let cancel = tr("ui.modal.cancel", "Cancel");
    let warnings = room.map(|h| leave_warnings(cx, h)).unwrap_or_default();
    let mut click = FooterClick::None;
    let a = widgets::modal_shell(
        ctx,
        &spec,
        |ui| {
            paragraph(ui, body);
            for w in &warnings {
                widgets::banner(ui, BannerKind::Hatched, "warning", w, None);
            }
        },
        |ui| {
            click = widgets::modal_footer(
                ui,
                &[],
                None,
                &[Btn::secondary(&cancel), Btn::danger_solid(ok)],
            )
        },
    );
    match click {
        FooterClick::Trailing(1) => cx.act(action),
        FooterClick::Trailing(0) => cx.act(UiAction::CloseModal),
        _ if a.close => cx.act(UiAction::CloseModal),
        _ => {}
    }
}

fn rename(ctx: &egui::Context, cx: &mut Cx, st: &mut ModalsState, room: &str) {
    let title = tr("ui.room.rename_title", "Rename room");
    let spec = ModalSpec {
        id: "rename",
        title: &title,
        sub: None,
        width: theme::MODAL_W,
        head_pill: None,
    };
    let cancel = tr("ui.modal.cancel", "Cancel");
    let save = tr("ui.modal.save", "Save");
    let can_save = !st.rename.trim().is_empty();
    let mut submit = false;
    let mut click = FooterClick::None;
    let a = widgets::modal_shell(
        ctx,
        &spec,
        |ui| {
            plain_card(ui, |ui| {
                let r = widgets::ctrl_row(ui, &tr("ui.modal.name", "Name"), None, |ui| {
                    widgets::text_field(ui, &mut st.rename, "")
                });
                submit = entered(ui, &r);
            });
        },
        |ui| {
            click = widgets::modal_footer(
                ui,
                &[],
                None,
                &[
                    Btn::secondary(&cancel),
                    Btn::primary(&save).enabled(can_save),
                ],
            )
        },
    );
    if (click == FooterClick::Trailing(1) || submit) && !st.rename.trim().is_empty() {
        cx.act(UiAction::Rename {
            room: room.to_string(),
            name: st.rename.trim().to_string(),
        });
    } else if click == FooterClick::Trailing(0) || a.close {
        cx.act(UiAction::CloseModal);
    }
}

/// "Choose your name" (`MODAL_W`): one line of text and a prefilled, selected
/// name field. Enter or Save sets the name, Later / Escape / × closes.
fn choose_name(ctx: &egui::Context, cx: &mut Cx, st: &mut ModalsState) {
    let title = tr("ui.modal.choose_name_title", "Choose your name");
    let spec = ModalSpec {
        id: "choose_name",
        title: &title,
        sub: None,
        width: theme::MODAL_W,
        head_pill: None,
    };
    let later = tr("ui.stream.later", "Later");
    let save = tr("ui.modal.save", "Save");
    let can_save = !st.choose_name.trim().is_empty();
    let mut submit = false;
    let mut click = FooterClick::None;
    let a = widgets::modal_shell(
        ctx,
        &spec,
        |ui| {
            paragraph(ui, &tr("ui.modal.choose_name_body", "Friends see this name in rooms."));
            plain_card(ui, |ui| {
                let r = widgets::ctrl_row(ui, &tr("ui.modal.name", "Name"), None, |ui| {
                    widgets::text_field(ui, &mut st.choose_name, "")
                });
                if std::mem::take(&mut st.choose_name_focus) {
                    let id = r.id;
                    r.request_focus();
                    let all = egui::text::CCursorRange::two(egui::text::CCursor::new(0), egui::text::CCursor::new(st.choose_name.chars().count()));
                    let mut state = egui::TextEdit::load_state(ui.ctx(), id).unwrap_or_default();
                    state.cursor.set_char_range(Some(all));
                    state.store(ui.ctx(), id);
                }
                submit = entered(ui, &r);
            });
        },
        |ui| click = widgets::modal_footer(ui, &[], None, &[Btn::secondary(&later), Btn::primary(&save).enabled(can_save)]),
    );
    if (click == FooterClick::Trailing(1) || submit) && can_save {
        cx.act(UiAction::SetDisplayName(st.choose_name.trim().to_string()));
        cx.act(UiAction::CloseModal);
    } else if click == FooterClick::Trailing(0) || a.close {
        cx.act(UiAction::CloseModal);
    }
}

/// An untitled settings card (mockup `V("Frame · vbox (card)")` without a
/// title row, as in the create/join and rename modals): the same frame as
/// `widgets::card`, rows 8 px apart, full width.
fn plain_card<R>(ui: &mut Ui, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(theme::PANEL_RAISED)
        .stroke(egui::Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(egui::CornerRadius::same(theme::ROUNDING_WELL))
        .inner_margin(egui::Margin::same(theme::S3 as i8))
        .show(ui, |ui| {
            let w = ui.available_width();
            ui.set_width(w);
            ui.spacing_mut().item_spacing.y = theme::S2;
            body(ui)
        })
        .inner
}

/// Dim explanatory text (`.modal p`): 12.5 px, line height 1.55, wraps.
fn paragraph(ui: &mut Ui, text: &str) {
    colored_paragraph(ui, text, theme::TEXT_DIM);
}

/// Inline validation message under a field row: small, `DANGER`, wraps.
/// Why: the Create/Join button is disabled while one shows, so the reason has
/// to be on screen (task #84, a relay address without a port).
fn field_hint(ui: &mut Ui, text: &str) {
    ui.add_space(2.0);
    colored_paragraph(ui, text, theme::DANGER);
}

/// Hint for the server field of the create dialog; `None` when it is empty
/// (nothing to say yet, the button is disabled anyway) or valid.
fn server_hint(server: &str) -> Option<String> {
    match parse_server_addr(server) {
        Ok(_) | Err(ServerAddrError::Empty) => None,
        Err(ServerAddrError::NoPort) => Some(tr(
            "ui.modal.server_no_port",
            "Add the relay's port, e.g. example.org:4433",
        )),
        Err(ServerAddrError::BadPort) => Some(tr(
            "ui.modal.server_bad_port",
            "The port must be a number from 1 to 65535, e.g. example.org:4433",
        )),
    }
}

/// Same check for the server part (before the first `|`) of a pasted join
/// link. Silent until the first `|` is there: before that the user is still
/// typing or pasting the address.
fn join_link_hint(link: &str) -> Option<String> {
    let link = link.trim();
    link.split_once('|').and_then(|(server, _)| server_hint(server))
}

fn colored_paragraph(ui: &mut Ui, text: &str, color: egui::Color32) {
    let w = ui.available_width();
    let mut job = egui::text::LayoutJob::single_section(
        text.to_owned(),
        egui::TextFormat {
            font_id: font(Weight::Regular, theme::FS_LABEL),
            color,
            line_height: Some((theme::FS_LABEL * 1.55).round()),
            ..Default::default()
        },
    );
    job.wrap.max_width = w;
    let g = ui.painter().layout_job(job);
    let (rect, _) = ui.allocate_exact_size(egui::vec2(w, g.size().y), egui::Sense::hover());
    ui.painter().galley(rect.min, g, color);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_field_hint_and_join_link_hint() {
        assert!(server_hint("").is_none(), "empty: no hint, the button is just disabled");
        assert!(server_hint("example.org:4433").is_none());
        assert!(server_hint("[::1]:4433").is_none());
        for bad in ["example.org", "::1", "example.org:0", "example.org:70000", "example.org:"] {
            assert!(server_hint(bad).is_some(), "{bad}");
        }
        // Join link: silent until the first `|`, then the server part counts.
        assert!(join_link_hint("example.org").is_none());
        assert!(join_link_hint("example.org|").is_some());
        assert!(join_link_hint("  example.org|00|key").is_some());
        assert!(join_link_hint("example.org:4433|00|key").is_none());
    }

    #[test]
    fn masked_link_never_contains_the_key() {
        let js = simple_screensharing_client::net::format_join_string(
            "relay.example:4433",
            &[0xab; 16],
            &[7u8; 32],
        );
        let parsed = parse_join_string(&js).unwrap();
        let masked = masked_link(&js);
        assert!(
            masked.starts_with("relay.example:4433|ababab…|"),
            "{masked}"
        );
        let key_field = js.split('|').nth(2).unwrap();
        assert!(!masked.contains(key_field), "the key must not be shown");
        assert_eq!(parsed.room_key, [7u8; 32]);
        assert_eq!(masked_link("garbage"), "••••••••");
    }

    #[test]
    fn pasted_link_prefills_the_name_unless_the_user_typed_one() {
        use simple_screensharing_client::net::format_join_string_full;
        let link = |n: &str| format_join_string_full("r:1", &[1; 16], &[2; 32], None, Some(n));
        let mut st = ModalsState::default();

        // Paste: empty name field gets the link's name.
        st.join_link = link("Friday Films");
        st.prefill_name();
        assert_eq!(st.join_name, "Friday Films");

        // User edits the prefill; pasting another link must not overwrite it.
        st.join_name = "My name".into();
        st.join_link = link("Other");
        st.prefill_name();
        assert_eq!(st.join_name, "My name");

        // Cleared by the user: a new paste may prefill again.
        st.join_name.clear();
        st.join_link = link("Third");
        st.prefill_name();
        assert_eq!(st.join_name, "Third");

        // An untouched prefill is replaced by the next paste's name.
        st.join_link = link("Fourth");
        st.prefill_name();
        assert_eq!(st.join_name, "Fourth");

        // A link without a name, or an unparsable one, leaves the field alone;
        // so does an unchanged link text (no re-prefill every frame).
        st.join_link = simple_screensharing_client::net::format_join_string("r:1", &[1; 16], &[2; 32]);
        st.prefill_name();
        st.join_link = "garbage".into();
        st.prefill_name();
        assert_eq!(st.join_name, "Fourth");
        st.join_name = "typed".into();
        st.prefill_name();
        assert_eq!(st.join_name, "typed");
    }

    #[test]
    fn tr_falls_back_to_english_for_missing_keys() {
        assert_eq!(
            tr("ui.modal.definitely_missing_key", "Fallback"),
            "Fallback"
        );
    }
}
