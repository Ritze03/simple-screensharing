//! The webcam menu (task I8, the replica's `camModal`): the big menu the bar's webcam dropdown opens with "All webcam
//! settings…" ([`Modal::WebcamSettings`]), like the stream settings are the screen share's and the audio settings
//! the audio share's.
//!
//! - **Source** card: **Camera** (every camera `v4l2` lists) and **Mode** (what that camera offers: size, frame
//!   rate and pixel format, motion-JPEG first, `capture::webcam::list_cameras`). Changing the camera resets the
//!   mode to that camera's default, and a running webcam restarts on whatever was picked (a different capture).
//! - **Compression** card: **Codec** (only when this machine can encode more than one) and **Bitrate** (live: a codec
//!   or bitrate change applies to the running webcam without a restart). The encoder itself is fixed (Performance,
//!   keyframe 2000 ms; plan D8), and the card says so.
//! - Footer: `[Start webcam | Stop webcam][fill][Done]`. It does not need a screen share and goes to the room it
//!   was opened from.
//!
//! The rig's `SSS_UI_DUMP` gets `webcam.camera`, `webcam.mode`, `webcam.bitrate`, `webcam.footer`, `webcam_codec.<i>` rects and
//! `webcam` / `webcam_modes` lines.

use egui::{Align, Ui};
use simple_screensharing_client::capture::VideoCodec;
use simple_screensharing_client::capture::webcam::Mode;
use simple_screensharing_client::presets::{self, CODEC_DISPLAY_ORDER, MIN_BITRATE_KBPS, codec_description, codec_label};
use simple_screensharing_client::rooms::{BandwidthMode, WebcamSettings};
use simple_screensharing_client::session::{AppState, RoomHandle};

use super::stream_settings::{commit, seg_gated};
use super::{Cx, UiAction, drive_dump, menu, tr, tr_with};
use crate::theme;
use crate::widgets::{self, Btn, FooterClick, ModalSpec};

/// Webcam bitrate range (kbps): the replica's slider.
const BITRATE_MAX_KBPS: u32 = 12_000;

/// Webcam menu state.
#[derive(Default)]
pub struct WebcamSettingsState {
    /// The room the menu was last drawn for: a new one re-lists the cameras.
    opened_for: Option<RoomHandle>,
    /// The bitrate slider's value while it is dragged (applied on release).
    bitrate: Option<u32>,
}

/// A mode as the dropdown shows it: `1280×720 · 30 fps · MJPG`.
pub(super) fn mode_label(m: &Mode) -> String {
    tr_with("ui.webcam.mode_label", "%{size} · %{fps} fps · %{format}", &[("size", &format!("{}×{}", m.width, m.height)), ("fps", &fps_text(m)), ("format", m.format_label())])
}

/// The frame rate as a number: `30`, `7.5`, `29.97`.
pub(super) fn fps_text(m: &Mode) -> String {
    let f = format!("{:.2}", m.fps());
    f.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Draw the menu for the webcam of `room`.
pub fn show(ctx: &egui::Context, cx: &mut Cx, st: &mut WebcamSettingsState, room: RoomHandle) {
    if st.opened_for != Some(room) {
        *st = WebcamSettingsState { opened_for: Some(room), ..Default::default() };
        cx.act(UiAction::RefreshWebcams);
    }
    let live = cx.webcam_share(room);
    let choice = cx.state.webcam_choice();
    let title = tr("ui.webcam.title", "Webcam settings");
    let mode = choice.map(|(c, m)| cx.state.webcams[c].modes[m].clone());
    let mut sub = mode.as_ref().map(|m| format!("{}×{} · {} fps", m.width, m.height, fps_text(m))).unwrap_or_else(|| tr("ui.bar.webcam_none", "No camera found"));
    if live.is_some() {
        sub = tr_with("ui.webcam.sub_live", "%{mode} · live in %{room}", &[("mode", &sub), ("room", &cx.room_name(room))]);
    }
    let spec = ModalSpec { id: "webcam_settings", title: &title, sub: Some(&sub), width: theme::MODAL_W_WIDE, head_pill: None };
    let (start, stop, done) = (tr("ui.webcam.start", "Start webcam"), tr("ui.webcam.stop", "Stop webcam"), tr("ui.stream.done", "Done"));

    let mut out: Vec<UiAction> = Vec::new();
    let mut click = FooterClick::None;
    let cxr: &Cx = cx;
    let a = widgets::modal_shell(
        ctx,
        &spec,
        |ui| body(ui, cxr, st, &mut out),
        |ui| {
            // The leading button is the first 96+ px of this row (`uclick N webcam.footer L60`).
            drive_dump::rect(|| "webcam.footer".into(), ui.max_rect());
            let lead = if live.is_some() { Btn::danger(&stop).icon("videocam_off") } else { Btn::secondary(&start).icon("videocam").enabled(choice.is_some()) };
            click = widgets::modal_footer(ui, &[lead], None, &[Btn::primary(&done)]);
        },
    );
    for act in out {
        cx.act(act);
    }
    match click {
        FooterClick::Leading(0) => cx.act(if live.is_some() { UiAction::StopWebcam } else { UiAction::StartWebcam(room) }),
        FooterClick::Trailing(_) => cx.act(UiAction::CloseModal),
        // Escape with a dropdown open only closes that dropdown (the shell consumes it first).
        _ if a.close => cx.act(UiAction::CloseModal),
        _ => {}
    }
}

fn body(ui: &mut Ui, cx: &Cx, st: &mut WebcamSettingsState, out: &mut Vec<UiAction>) {
    let state: &AppState = cx.state;
    let settings = state.webcam.clone();
    let choice = state.webcam_choice();

    widgets::card(ui, &tr("ui.webcam.source_card", "Source"), None, |ui| {
        if state.webcams.is_empty() {
            let text = if state.webcams_listing() { tr("ui.bar.webcam_looking", "Looking for cameras…") } else { tr("ui.bar.webcam_none", "No camera found") };
            widgets::ctrl_row(ui, &tr("ui.webcam.camera", "Camera"), None, |ui| widgets::readout(ui, &text, Align::Min));
            return;
        }
        let (ci, mi) = choice.unwrap_or((0, 0));
        // Camera.
        let names: Vec<menu::Opt> = state.webcams.iter().map(|c| menu::Opt { label: &c.name, sub: Some(&c.device), icon: Some("videocam"), group: None }).collect();
        let mut idx = ci;
        let r = widgets::ctrl_row(ui, &tr("ui.webcam.camera", "Camera"), None, |ui| menu::select(ui, "webcam_camera", &mut idx, &names));
        drive_dump::rect(|| "webcam.camera".into(), r.rect);
        if r.changed() {
            // A new camera starts at its own default mode.
            out.push(UiAction::SetWebcam(WebcamSettings { camera: state.webcams[idx].name.clone(), mode: String::new(), ..settings.clone() }));
        }
        // Mode.
        let cam = &state.webcams[ci];
        let labels: Vec<String> = cam.modes.iter().map(mode_label).collect();
        let opts: Vec<menu::Opt> = labels.iter().map(|l| menu::Opt::new(l)).collect();
        let mut midx = mi;
        let r = widgets::ctrl_row(ui, &tr("ui.webcam.mode", "Mode"), Some(&tr("ui.webcam.mode_tip", "What this camera offers (v4l2)")), |ui| menu::select(ui, "webcam_mode", &mut midx, &opts));
        drive_dump::rect(|| "webcam.mode".into(), r.rect);
        drive_dump::line(|| format!("webcam_modes {}\nwebcam_mode {}", cam.modes.len(), cam.modes[mi].key()));
        if r.changed() {
            out.push(UiAction::SetWebcam(WebcamSettings { camera: cam.name.clone(), mode: cam.modes[midx].key(), ..settings.clone() }));
        }
    });

    widgets::card(ui, &tr("ui.stream.compression", "Compression"), None, |ui| {
        let supported = AppState::supported_codecs();
        if presets::codec_choice_offered(supported) {
            let opts: Vec<(VideoCodec, &str, bool, &str)> =
                CODEC_DISPLAY_ORDER.iter().filter(|c| supported.contains(c)).map(|c| (*c, codec_label(*c), true, codec_description(*c))).collect();
            // A stored codec this machine cannot encode shows as the first one it can: what would be used.
            let mut codec = if supported.contains(&settings.codec) { settings.codec } else { opts[0].0 };
            let changed = widgets::ctrl_row(ui, &tr("ui.stream.codec", "Codec"), None, |ui| seg_gated(ui, "webcam_codec", &mut codec, &opts, ""));
            if changed {
                out.push(UiAction::SetWebcam(WebcamSettings { codec, ..settings.clone() }));
            }
        }
        let bitrate_l = tr("ui.stream.bitrate", "Bitrate");
        if state.bandwidth_mode == BandwidthMode::Absolute {
            widgets::ctrl_row_value(
                ui,
                &bitrate_l,
                Some(&tr("ui.stream.bitrate_abs_tip", "Absolute maximum mode: the upload budget splits the bitrate across all your streams.")),
                |ui| widgets::readout(ui, &tr("ui.stream.bitrate_by_budget", "Set by upload budget"), Align::Max),
                |ui| widgets::value_box(ui, "\u{2014}"),
            );
        } else {
            let mut kbps = st.bitrate.unwrap_or(settings.bitrate_kbps);
            let r = widgets::slider_row(ui, &bitrate_l, Some(&tr("ui.webcam.bitrate_tip", "kbps for this webcam")), &mut kbps, MIN_BITRATE_KBPS..=BITRATE_MAX_KBPS, f64::from(presets::BITRATE_STEP_KBPS), 0, "");
            drive_dump::rect(|| "webcam.bitrate".into(), r.rect);
            if let Some(k) = commit(r, kbps, &mut st.bitrate) {
                out.push(UiAction::SetWebcam(WebcamSettings { bitrate_kbps: k, ..settings.clone() }));
            }
        }
        drive_dump::line(|| format!("webcam camera={:?} codec={:?} kbps={}", settings.camera, settings.codec, settings.bitrate_kbps));
        // What is fixed, said once (plan D8).
        let row = widgets::HBox::new("webcam_encoder", 16.0).slots(&[widgets::fixed(theme::LABEL_W), widgets::FILL]).show(ui);
        widgets::paint_text(ui, row.cell(1), &tr("ui.webcam.encoder_note", "Encoder: Performance · Keyframe 2000 ms"), crate::theme::font(theme::Weight::Regular, theme::FS_TINY), theme::TEXT_DIM, Align::Min);
    });
}
