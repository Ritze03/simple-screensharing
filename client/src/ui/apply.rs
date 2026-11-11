//! The one place [`UiAction`]s change state. Every handler is a thin call
//! into the lib (`session.rs`), plus the navigation / shell bookkeeping
//! the lib doesn't know about (which modal is open, staged captures, toasts).

use std::sync::atomic::Ordering;

use simple_screensharing_client::net::{format_join_string_with_hash, parse_join_string};
use simple_screensharing_client::session::{self, PendingRole};

use super::{App, FullscreenStream, Modal, StagedShare, UiAction, View, room_entries, same_room, tr, tr_with, view_is_room};
use simple_screensharing_client::audio::{AudioChoice, name_is_excluded};

impl App {
    /// Apply one action. `ctx` is only used for the clipboard.
    /// Stage a capture for `room` (opens the picker). `auto`: go live by itself once the picker resolved
    /// (`poll_staged`) instead of opening the settings modal.
    ///
    /// On Linux a gamescope look-up comes first (row 142, `poll_staged` finishes it): no node, today's flow; a node,
    /// the "Screen or gamescope" question. Why a look-up per click and no cached list: a cached "no gamescope" would
    /// hide a game started a minute ago, and `pw-dump` takes ~30 ms off the UI thread.
    fn stage_share(&mut self, room: simple_screensharing_client::session::RoomHandle, auto: bool) {
        if self.shell.staged.contains_key(&room) || !self.state.rooms.contains_key(&room) || self.shell.gamescope_probe.is_some() {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            let (tx, rx) = tokio::sync::oneshot::channel();
            tokio::spawn(async move {
                let _ = tx.send(simple_screensharing_client::capture::gamescope::find_all().await);
            });
            self.shell.gamescope_probe = Some((room, auto, rx));
            self.nav.popover = None;
        }
        #[cfg(not(target_os = "linux"))]
        self.stage_share_now(room, auto, None);
    }

    /// Stage the capture: the portal's picker, or the gamescope node `node` (no picker).
    pub(super) fn stage_share_now(&mut self, room: simple_screensharing_client::session::RoomHandle, auto: bool, node: Option<simple_screensharing_client::capture::gamescope::Node>) {
        if self.shell.staged.contains_key(&room) || !self.state.rooms.contains_key(&room) {
            return;
        }
        let capture = match node {
            Some(node) => self.state.stage_gamescope_share_with_saved_defaults(node),
            None => self.state.stage_video_share_with_saved_defaults(),
        };
        self.shell.staged.insert(room, StagedShare { capture, modal_opened: false, auto });
        self.nav.popover = None;
    }

    pub(super) fn apply(&mut self, ctx: &egui::Context, action: UiAction) {
        match action {
            UiAction::LivePreviewSeen(id) => {
                self.shell.live_previews.entry(id).or_default().seen = ctx.input(|i| i.time);
                ctx.request_repaint_after(std::time::Duration::from_millis(33));
            }
            // --- navigation ---------------------------------------------
            UiAction::SelectRoom(key) => {
                self.state.active_room = self.state.rooms.iter().find(|(_, r)| same_room(&r.join_string, &key)).map(|(h, _)| *h);
                self.nav.view = View::Room(key);
                self.nav.popover = None;
            }
            UiAction::ShowWatching => {
                self.nav.view = View::Watching;
                self.nav.popover = None;
            }
            UiAction::OpenModal(m) => {
                if m == Modal::CreateJoin {
                    self.shell.join_error = None;
                    self.shell.create_result = None;
                }
                self.nav.modal = Some(m);
                self.nav.popover = None;
            }
            UiAction::CloseModal => self.nav.modal = None,
            UiAction::OpenPopover(p) => self.nav.popover = Some(p),
            UiAction::ClosePopover => self.nav.popover = None,

            // --- rooms ---------------------------------------------------
            UiAction::Connect(key) => {
                self.shell.clear_connect_error(&key);
                if self.state.pending_connect.is_some() {
                    // One connect at a time (lib invariant): queue it.
                    if !self.state.auto_connect_queue.iter().any(|k| same_room(k, &key)) {
                        self.state.auto_connect_queue.push(key);
                    }
                } else if let Err(e) = self.state.start_connect(key, false) {
                    self.toast(tr_with("ui.toast.connect_failed", "Couldn't connect: %{error}", &[("error", &e.to_string())]), "error");
                }
            }
            UiAction::RetryNow(key) => self.state.reconnect_now(&key),
            UiAction::CancelReconnect(key) => {
                self.state.cancel_reconnect(&key);
                // Still "reconnecting" = a first-time connect to this room is
                // in flight (no reconnect entry): abort that.
                if self.state.is_reconnecting(&key) {
                    self.state.cancel_connect();
                }
            }
            UiAction::JoinRoom { join_string, room_name } => {
                self.shell.join_error = None;
                let parsed = match parse_join_string(&join_string) {
                    Ok(p) => p,
                    Err(e) => {
                        self.shell.join_error = Some(format!("{e:#}"));
                        return;
                    }
                };
                // Drop the link's 5th field (the suggested room name): the
                // name field of the dialog already decided what is saved, and
                // the stored join string stays `server|id|key[|certhash]`.
                let join_string =
                    format_join_string_with_hash(&parsed.server, &parsed.room_id, &parsed.room_key, parsed.cert_hash.as_ref());
                if let Some(h) = self.state.rooms.iter().find(|(_, r)| same_room(&r.join_string, &join_string)).map(|(h, _)| *h) {
                    // Already joined: just show it.
                    self.state.active_room = Some(h);
                    self.nav.view = View::Room(join_string);
                    self.nav.modal = None;
                    return;
                }
                if self.state.pending_connect.is_some() {
                    self.state.cancel_connect();
                }
                match self.state.join_room(join_string.clone(), String::new(), room_name) {
                    Ok(()) => self.shell.awaiting_join = Some(join_string),
                    Err(e) => self.shell.join_error = Some(e),
                }
            }
            UiAction::CreateRoom { server, room_name } => {
                self.shell.create_result = Some(self.state.create_room(&server, &room_name));
            }
            UiAction::Leave(handle) => {
                if !self.state.request_leave_room(handle) {
                    self.nav.modal = Some(Modal::LeaveConfirm { room: handle });
                    self.nav.popover = None;
                }
            }
            UiAction::LeaveConfirmed(handle) => {
                self.state.disconnect_room(handle);
                if self.nav.modal == Some(Modal::LeaveConfirm { room: handle }) {
                    self.nav.modal = None;
                }
            }
            UiAction::Forget(key) => {
                if let Some(h) = self.state.rooms.iter().find(|(_, r)| same_room(&r.join_string, &key)).map(|(h, _)| *h) {
                    self.state.disconnect_room(h);
                }
                if self.state.is_reconnecting(&key) {
                    self.state.cancel_connect();
                }
                self.state.auto_connect_queue.retain(|k| !same_room(k, &key));
                self.shell.clear_connect_error(&key);
                if let Err(e) = self.state.forget_room(&key) {
                    self.toast(tr_with("ui.toast.save_failed", "Couldn't save: %{error}", &[("error", &e.to_string())]), "error");
                }
                if view_is_room(&self.nav.view, &key) {
                    self.nav.view = room_entries(&self.state).first().map(|r| View::Room(r.key.clone())).unwrap_or_default();
                }
                if matches!(&self.nav.modal, Some(Modal::ForgetConfirm { room }) if same_room(room, &key)) {
                    self.nav.modal = None;
                }
            }
            UiAction::Rename { room, name } => {
                let name = name.trim().to_string();
                if !name.is_empty() {
                    self.state.rename_saved_room(&room, name);
                }
                if matches!(&self.nav.modal, Some(Modal::Rename { room: r }) if *r == room) {
                    self.nav.modal = None;
                }
            }
            UiAction::SetAutoConnect { room, on } => {
                // Saving the flag needs a saved entry; an unsaved live room
                // gets saved under its current label first.
                if self.state.room_store.name_for(&room).is_none() {
                    let name = session::room_label(&self.state.room_store, &room);
                    self.state.rename_saved_room(&room, name);
                }
                self.state.set_room_auto_connect(&room, on);
            }
            UiAction::CopyInviteLink(handle) | UiAction::CopyDirectLink(handle) => {
                let direct = matches!(action, UiAction::CopyDirectLink(_));
                let Some(room) = self.state.rooms.get(&handle) else { return };
                match parse_join_string(&room.join_string) {
                    Ok(parsed) => {
                        let name = self.state.room_store.name_for(&room.join_string);
                        let link = room.client.invite_link(&parsed.server, name);
                        if direct {
                            ctx.copy_text(super::direct_link::direct_link(&link));
                            self.toast(tr("ui.toast.direct_link_copied", "Browser direct link copied").to_string(), "content_copy");
                        } else {
                            ctx.copy_text(link);
                            self.toast(tr("ui.toast.invite_copied", "Invite link copied").to_string(), "content_copy");
                        }
                    }
                    Err(e) => self.toast(tr_with("ui.toast.copy_failed", "Couldn't copy the invite link: %{error}", &[("error", &format!("{e:#}"))]), "error"),
                }
            }

            // --- watching ------------------------------------------------
            UiAction::Watch((room, owner, stream)) => {
                if let Some(r) = self.state.rooms.get_mut(&room)
                    && !r.watches.lock().unwrap().contains_key(&(owner, stream))
                {
                    r.toggle_watch(owner, stream);
                }
                self.videos.sync(&self.state);
            }
            UiAction::WatchPoppedOut(key) => {
                self.apply(ctx, UiAction::Watch(key));
                self.apply(ctx, UiAction::PopOut(key));
            }
            UiAction::Unwatch((room, owner, stream)) => {
                // The user ended it: not a watch to carry the pop-out over (`VideoViews::sync`).
                if let Some(e) = self.videos.get_mut((room, owner, stream)) {
                    e.popped = false;
                }
                if let Some(r) = self.state.rooms.get_mut(&room)
                    && r.watches.lock().unwrap().contains_key(&(owner, stream))
                {
                    r.toggle_watch(owner, stream);
                }
                self.videos.sync(&self.state);
            }
            UiAction::PopOut(key) => {
                if let Some(e) = self.videos.get_mut(key) {
                    e.close_requested.store(false, Ordering::Relaxed);
                    e.popped = true;
                }
            }
            UiAction::EnterStreamFullscreen(key) => {
                // Only from the normal layout, and not for a popped-out stream
                // (its tile is a placeholder; the pop-out has its own F11).
                if self.nav.fullscreen.is_none() && self.videos.get(key).is_some_and(|e| !e.popped) {
                    let was_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                    self.nav.fullscreen = Some(FullscreenStream { key, was_fullscreen });
                    self.nav.popover = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                }
            }
            UiAction::ExitStreamFullscreen => {
                if let Some(fs) = self.nav.fullscreen.take() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(fs.was_fullscreen));
                }
            }
            UiAction::BringBack(key) => {
                if let Some(e) = self.videos.get_mut(key) {
                    e.popped = false;
                    e.close_requested.store(false, Ordering::Relaxed);
                }
            }
            UiAction::ToggleListen((room, owner, stream)) => {
                if let Some(r) = self.state.rooms.get_mut(&room) {
                    r.toggle_listen(owner, stream);
                }
            }
            UiAction::SetVolume { stream: (room, owner, stream), volume_percent, muted } => {
                self.state.set_volume(room, owner, stream, volume_percent, muted);
            }

            // --- sharing -------------------------------------------------
            UiAction::StageShare(room) => self.stage_share(room, false),
            UiAction::ShareNow(room) => self.stage_share(room, true),
            UiAction::SharePicked { room, auto, node } => {
                if matches!(self.nav.modal, Some(Modal::SharePick { .. })) {
                    self.nav.modal = None;
                }
                self.stage_share_now(room, auto, node);
            }
            UiAction::GoLive { capture, room } => {
                self.state.go_live_and_save_defaults(capture, room);
                self.shell.staged.shift_remove(&room);
                if matches!(self.nav.modal, Some(Modal::StreamSettings { capture: c, .. }) if c == capture) {
                    self.nav.modal = None;
                }
                // No "Live in" toast here: it fires from `poll` once a
                // destination is really on air.
            }
            UiAction::Discard(capture) => {
                self.state.stop_share(capture);
                self.shell.staged.retain(|_, s| s.capture != capture);
                if matches!(self.nav.modal, Some(Modal::StreamSettings { capture: c, .. }) if c == capture) {
                    self.nav.modal = None;
                }
            }
            UiAction::StopDestination(destination) => {
                let room = session::find_destination(&self.state.captures, destination).map(|(_, _, d)| d.room);
                self.state.stop_destination(destination);
                if let Some(room) = room {
                    let name = self.room_name(room);
                    self.toast(tr_with("ui.toast.stopped_in", "Stopped sharing in %{room}", &[("room", &name.to_string())]), "stop_circle");
                    // Keep an open settings modal pointing at a room the
                    // capture still feeds.
                    if let Some(Modal::StreamSettings { capture, room: mroom }) = self.nav.modal.clone()
                        && mroom == room
                    {
                        match self.state.captures.get(&capture).and_then(|c| c.destinations.values().next()) {
                            Some(d) => self.nav.modal = Some(Modal::StreamSettings { capture, room: d.room }),
                            None => self.nav.modal = None,
                        }
                    }
                }
            }
            UiAction::StopAllSharing => {
                // The webcam too, and a webcam start that was waiting for the old one to stop.
                self.state.stop_webcam();
                let ids: Vec<_> = self.state.captures.keys().copied().collect();
                for id in ids {
                    self.state.stop_share(id);
                }
                self.nav.popover = None;
                self.toast(tr("ui.toast.stopped_all", "Stopped all sharing").to_string(), "stop_circle");
            }
            UiAction::AddDestination { capture, room } => {
                self.state.add_destination(capture, room, PendingRole::FanOut);
                self.nav.popover = None;
            }
            UiAction::MoveDestination { destination, room } => {
                let from = session::find_destination(&self.state.captures, destination).map(|(c, _, d)| (c, d.room));
                self.state.transfer_destination(destination, room);
                if let Some((capture, from_room)) = from
                    && self.nav.modal == Some(Modal::StreamSettings { capture, room: from_room })
                {
                    self.nav.modal = Some(Modal::StreamSettings { capture, room });
                }
                self.nav.popover = None;
            }
            UiAction::MoveCaptureHere { capture, room } => {
                let dests: Vec<_> = self.state.captures.get(&capture).map(|c| c.destinations.keys().copied().collect()).unwrap_or_default();
                if let Some((first, rest)) = dests.split_first() {
                    // Make-before-break for the first; the others just stop
                    // (the transfer keeps the capture alive).
                    self.state.transfer_destination(*first, room);
                    for d in rest {
                        self.state.stop_destination(*d);
                    }
                }
                self.nav.popover = None;
            }
            UiAction::EditDestination { destination, edit } => self.state.edit_destination(destination, edit),
            UiAction::ApplyPresetToDestination { destination, preset } => self.state.apply_preset_to_destination(destination, preset),
            UiAction::SetNextStream(n) => {
                self.state.framerate = n.framerate;
                self.state.bitrate_kbps = n.bitrate_kbps;
                self.state.tune = n.tune;
                self.state.quality_level = n.quality_level;
                self.state.gop_millis = n.gop_millis;
                self.state.codec = n.codec;
            }
            UiAction::ApplyPresetToNextStream(p) => self.state.apply_preset_to_start_stream(p),
            UiAction::SetBandwidthMode(m) => self.state.set_bandwidth_mode(m),
            UiAction::SetBandwidthTotal(v) => self.state.set_bandwidth_total(v),
            UiAction::SetBandwidthFloor(v) => self.state.set_bandwidth_floor(v),

            // --- webcam ----------------------------------------------------
            UiAction::StartWebcam(room) => {
                if let Err(e) = self.state.start_webcam(room) {
                    self.toast(tr_with("ui.toast.webcam_failed", "Couldn't share the webcam: %{error}", &[("error", &e)]), "error");
                }
                self.nav.popover = None;
            }
            UiAction::StopWebcam => {
                self.state.stop_webcam();
                self.nav.popover = None;
            }
            UiAction::SetWebcam(w) => self.state.set_webcam_settings(w),
            UiAction::RefreshWebcams => self.state.refresh_webcams(),

            // --- shared audio --------------------------------------------
            UiAction::ShareAudio { room, choice } => {
                // Switching replaces the running share; keep it muted if it was (#118).
                let old = self.state.audio_share_in(room);
                let muted = old.is_some_and(|o| self.state.is_audio_share_muted(o));
                if let Some(old) = old {
                    self.state.stop_share(old);
                }
                if let Err(e) = self.state.start_audio_share(room, None, choice, muted) {
                    self.toast(tr_with("ui.toast.audio_failed", "Couldn't share audio: %{error}", &[("error", &e.to_string())]), "error");
                }
                self.nav.popover = None;
            }
            UiAction::StopSharingAudio { room } => {
                if let Some(audio) = self.state.audio_share_in(room) {
                    self.state.stop_share(audio);
                }
                self.nav.popover = None;
            }
            UiAction::SetSharedAudioMuted { room, muted } => {
                if let Some(audio) = self.state.audio_share_in(room) {
                    self.state.set_audio_share_muted(audio, muted);
                }
            }
            UiAction::SetAudioExclusions(list) => {
                let stored = self.state.set_audio_exclusions(list);
                // The app you were sharing just went on the list (replica): the share turns into desktop
                // audio, which leaves it out, instead of silently going quiet.
                let rooms: Vec<_> = self.state.rooms.keys().copied().collect();
                for room in rooms {
                    if let Some(AudioChoice::Named(n)) = self.state.audio_choice_in(room)
                        && name_is_excluded(&n, &stored)
                    {
                        self.apply(ctx, UiAction::ShareAudio { room, choice: AudioChoice::Desktop });
                    }
                }
            }
            UiAction::RefreshAudioSources => self.state.refresh_audio_sources(),
            UiAction::StartLevelTaps(sources) => {
                self.stop_level_taps();
                let generation = self.shell.tap_generation;
                for source in sources {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    tokio::spawn(async move {
                        let _ = tx.send(session::build_audio_level_tap(source).await);
                    });
                    self.shell.pending_taps.push((generation, source, rx));
                }
            }
            UiAction::StopLevelTaps => self.stop_level_taps(),

            // --- settings ------------------------------------------------
            UiAction::SetDisplayName(n) => self.state.set_display_name(&n),
            UiAction::SetEmoji(e) => self.state.set_emoji(&e),
            UiAction::SetLanguage(l) => self.state.set_language(&l),
            UiAction::SetPresentationMode(m) => self.state.set_presentation_mode(m),
            UiAction::SetJitterBuffer(ms) => {
                self.state.set_max_jitter_buffer_ms(ms);
            }
            UiAction::SetCatchUpSpeed(p) => {
                self.state.set_catch_up_speed_percent(p);
            }
            UiAction::SetAutoWatch(on) => {
                self.shell.auto_watch = on;
                let mut settings = simple_screensharing_client::rooms::AppSettings::load();
                settings.auto_watch = on;
                if let Err(e) = settings.save() {
                    tracing::warn!("failed to save the auto-watch preference: {e:#}");
                }
            }
            UiAction::SetSendDiagnostics(on) => self.state.set_send_diagnostics(on),
            UiAction::SetAudioGain(g) => {
                self.state.set_audio_gain(g);
            }
            UiAction::PreviewAudioGain(g) => {
                self.state.apply_audio_gain(g);
            }
            UiAction::SetLevelLoudness(on) => self.state.set_level_loudness(on),
            UiAction::SetSoundVolume(v) => self.state.set_sound_volume(v),
            UiAction::PreviewSoundVolume(v) => {
                self.state.sound_volume = v.min(simple_screensharing_client::sounds::MAX_VOLUME);
                simple_screensharing_client::sounds::set_volume(v);
            }
            UiAction::TestSound(sound) => simple_screensharing_client::sounds::play(sound),
            UiAction::CopyLogs => {
                ctx.copy_text(simple_screensharing_client::logbuf::dump());
                self.toast(tr("ui.toast.logs_copied", "Logs copied"), "content_copy");
            }
            UiAction::SaveLogs => match simple_screensharing_client::logbuf::save_to_downloads() {
                Ok(path) => {
                    self.shell.saved_logs = Some(path.display().to_string());
                    self.toast(tr("ui.toast.logs_saved", "Logs saved"), "check_circle");
                }
                Err(e) => self.toast(tr_with("ui.toast.logs_save_failed", "Couldn't save the logs: %{error}", &[("error", &format!("{e:#}"))]), "error"),
            },

            // --- stage ----------------------------------------------------
            UiAction::SetStageLayout { view, layout } => {
                self.sync_stage_keys();
                self.shell.stage.set_layout(view, layout);
                self.shell.stage.save();
            }
            UiAction::SetHideOwnStreams(on) => {
                self.shell.stage.hide_own = on;
                self.shell.stage.save();
            }
            UiAction::SetAlwaysShowNames(on) => {
                self.shell.stage.always_names = on;
                self.shell.stage.save();
            }
            UiAction::SetShowStats(on) => {
                self.shell.stage.show_stats = on;
                self.shell.stage.save();
            }
            UiAction::SetHideWhenUnfocused(on) => {
                self.shell.stage.preview_unfocused = !on;
                self.shell.stage.save();
            }
            UiAction::SetStagePip { view, on } => {
                self.sync_stage_keys();
                self.shell.stage.set_pip(view, on);
                self.shell.stage.save();
            }
            UiAction::ToggleSidebar => self.nav.sidebar_hidden = !self.nav.sidebar_hidden,
            UiAction::EnterStageFullscreen => {
                if self.nav.stage_fullscreen.is_none() {
                    let was_fullscreen = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                    self.nav.stage_fullscreen = Some(was_fullscreen);
                    self.nav.popover = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                }
            }
            UiAction::ExitStageFullscreen => {
                if let Some(was_fullscreen) = self.nav.stage_fullscreen.take() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(was_fullscreen));
                }
            }
        }
    }

    fn stop_level_taps(&mut self) {
        self.shell.tap_generation = self.shell.tap_generation.wrapping_add(1);
        for (_, tap) in self.shell.level_taps.drain() {
            self.state.stop_audio_level_tap(tap);
        }
    }

    pub(super) fn room_name(&self, room: session::RoomHandle) -> String {
        match self.state.rooms.get(&room) {
            Some(r) => session::room_label(&self.state.room_store, &r.join_string),
            None => tr("ui.room.unknown", "Unknown room").to_string(),
        }
    }
}
