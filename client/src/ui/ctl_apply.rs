//! The app's half of `simple-screensharing ctl`: apply what the control socket queued (`ctl::take_pending`) and
//! answer. The answer is the per-instance line the CLI prints; a toast ending in "(CTL)" shows what acted.
//!
//! Every command acts at once (no dialog) and respects connected vs in the call (`AppState::in_call`): start,
//! toggle and hangup act only on rooms in the call; the stops act wherever a share runs. See
//! `superdoc/features/ctl.md`.

use simple_screensharing_client::ctl::{Command, WebcamPlan};
use simple_screensharing_client::protocol::StreamKind;
use simple_screensharing_client::session::{CaptureSourceId, RoomHandle, ShareStatus};

use super::{App, Modal, tr, tr_with};

/// "1 room" / "3 rooms".
fn rooms(n: usize) -> String {
    if n == 1 { "1 room".into() } else { format!("{n} rooms") }
}

impl App {
    /// Apply every command the socket queued since the last frame and send each its answer.
    pub(super) fn drain_ctl(&mut self) {
        for request in simple_screensharing_client::ctl::take_pending() {
            if !request.claim() {
                continue;
            }
            let answer = self.apply_ctl(request.command);
            let _ = request.reply.send(answer);
        }
    }

    /// Rooms we are in the call in, in the app's room order.
    fn rooms_in_call(&self) -> Vec<RoomHandle> {
        self.state.rooms.iter().filter(|(_, r)| r.in_call).map(|(h, _)| *h).collect()
    }

    /// Start the webcam in `targets` (skipping rooms it is already on in); the answer line. An error (no
    /// camera, no plugin) is a toast and the answer, like the room button's.
    fn ctl_start_webcam(&mut self, targets: &[RoomHandle]) -> String {
        let already = self.state.webcam_rooms_or_pending();
        let mut started = 0;
        for room in targets.iter().filter(|r| !already.contains(r)) {
            if let Err(e) = self.state.start_webcam(*room) {
                self.toast(tr_with("ui.toast.webcam_failed", "Couldn't share the webcam: %{error}", &[("error", &e)]), "error");
                return format!("webcam not started: {e}");
            }
            started += 1;
        }
        if started == 0 {
            return format!("webcam already on in {}", rooms(targets.len()));
        }
        self.toast(tr("ui.toast.ctl_webcam_started", "Webcam started (CTL)").to_string(), "videocam");
        format!("webcam started in {}", rooms(started))
    }

    fn apply_ctl(&mut self, command: Command) -> String {
        match command {
            Command::WebcamStart => {
                self.shell.webcam_memory.forget();
                let in_call = self.rooms_in_call();
                if in_call.is_empty() {
                    return "no room is in the call, webcam not started".into();
                }
                self.ctl_start_webcam(&in_call)
            }
            Command::WebcamStop => {
                self.shell.webcam_memory.forget();
                let on = self.state.webcam_rooms_or_pending();
                if on.is_empty() {
                    return "webcam is off, nothing to stop".into();
                }
                self.state.stop_webcam();
                self.toast(tr("ui.toast.ctl_webcam_stopped", "Webcam stopped (CTL)").to_string(), "videocam_off");
                format!("webcam stopped in {}", rooms(on.len()))
            }
            Command::WebcamToggle => {
                let on = self.state.webcam_rooms_or_pending();
                let in_call = self.rooms_in_call();
                match self.shell.webcam_memory.toggle(&on, &in_call) {
                    WebcamPlan::Stop(rs) => {
                        self.state.stop_webcam();
                        self.toast(tr("ui.toast.ctl_webcam_stopped", "Webcam stopped (CTL)").to_string(), "videocam_off");
                        format!("webcam stopped in {}", rooms(rs.len()))
                    }
                    WebcamPlan::Start(rs) => self.ctl_start_webcam(&rs),
                    WebcamPlan::NoRoomInCall => "webcam is off and no room is in the call, nothing started".into(),
                }
            }
            Command::ScreenshareStop => self.ctl_stop_screens(),
            Command::AudioStop => {
                let audio: Vec<CaptureSourceId> = self
                    .state
                    .captures
                    .iter()
                    .filter(|(_, c)| c.kind == StreamKind::Audio && !matches!(c.status, ShareStatus::Stopping(_)))
                    .map(|(id, _)| *id)
                    .collect();
                if audio.is_empty() {
                    return "no audio is shared, nothing to stop".into();
                }
                for id in &audio {
                    self.state.stop_share(*id);
                }
                self.toast(tr("ui.toast.ctl_audio_stopped", "Audio stopped (CTL)").to_string(), "volume_off");
                format!("audio stopped ({} share{})", audio.len(), if audio.len() == 1 { "" } else { "s" })
            }
            Command::Hangup => {
                let in_call = self.rooms_in_call();
                if in_call.is_empty() {
                    return "no room is in the call, nothing to hang up".into();
                }
                for room in &in_call {
                    self.state.hang_up(*room);
                }
                self.nav.popover = None;
                let text = if in_call.len() == 1 {
                    tr("ui.toast.ctl_hung_up", "Left the call (CTL)").to_string()
                } else {
                    tr_with("ui.toast.ctl_hung_up_n", "Left the call in %{count} rooms (CTL)", &[("count", &in_call.len().to_string())])
                };
                self.toast(text, "call_end");
                format!("left the call in {}", rooms(in_call.len()))
            }
            // `kill` and `help` are the CLI's own; they never reach the app.
            Command::Kill | Command::Help => "unsupported".into(),
        }
    }

    /// `screenshare stop`: every screen or window share (live, or still staged / in its picker or set-up dialog),
    /// with the audio started together with it (`audio_for_capture`, which `stop_share` cascades to). The
    /// webcam and audio started on its own keep going. Unlike `UiAction::StopAllSharing` this leaves them alone
    /// and drops the staged UI record and its open dialog too.
    fn ctl_stop_screens(&mut self) -> String {
        let screens: Vec<CaptureSourceId> = self
            .state
            .captures
            .iter()
            .filter(|(_, c)| c.kind == StreamKind::Video && !c.is_webcam() && !matches!(c.status, ShareStatus::Stopping(_)))
            .map(|(id, _)| *id)
            .collect();
        // A Share click whose gamescope look-up or choice dialog is still open has no capture yet.
        let pending_pick = self.shell.gamescope_probe.take().is_some() | matches!(self.nav.modal, Some(Modal::SharePick { .. }));
        if pending_pick && matches!(self.nav.modal, Some(Modal::SharePick { .. })) {
            self.nav.modal = None;
        }
        if screens.is_empty() && !pending_pick {
            return "no screen is shared, nothing to stop".into();
        }
        let live = screens.iter().filter(|id| self.state.captures.get(*id).is_some_and(|c| !c.destinations.is_empty())).count();
        let staged = screens.len() - live + usize::from(pending_pick);
        let paired = self.state.captures.values().filter(|c| c.audio_for_capture.is_some_and(|v| screens.contains(&v)) && !matches!(c.status, ShareStatus::Stopping(_))).count();
        for id in &screens {
            self.state.stop_share(*id);
            self.shell.staged.retain(|_, s| s.capture != *id);
            if matches!(self.nav.modal, Some(Modal::StreamSettings { capture, .. }) if capture == *id) {
                self.nav.modal = None;
            }
        }
        self.nav.popover = None;
        self.toast(tr("ui.toast.ctl_screen_stopped", "Screen sharing stopped (CTL)").to_string(), "stop_screen_share");
        let mut out = format!("screen sharing stopped ({live} live, {staged} not live yet");
        if paired > 0 {
            out.push_str(&format!(", {paired} paired audio"));
        }
        out.push(')');
        out
    }
}
