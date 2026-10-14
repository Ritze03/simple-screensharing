//! The stream-settings modal (owner: I15), for [`Modal::StreamSettings`]:
//! staged ("Set up your stream", footer `[Discard][fill][Later][Go live in X]`)
//! when the capture has no destination yet, live ("Stream settings",
//! `[Stop sharing in X][fill][✓ Saved][Done]`) otherwise. Width
//! `MODAL_W_XWIDE` (1120, capped at window − 48); head band
//! `[fill title/sub][96 Applying pill][32 ×]`.
//!
//! Body: two equal columns (gap 12), each a vbox of cards (gap 12):
//! - where it goes: Source (Screen / Audio + meter / Mute), Sending to (dest
//!   rows `[18][fill][84 pill][76 kbps][32 ×]` + equal `[Also send to…][Move to…]`),
//!   Upload bandwidth (Mode / Total / Floor / Per stream or over-budget banner);
//! - how it's encoded: Video (Framerate / Bitrate / Codec), Encoder (Preset ×4
//!   + collapsible Advanced: Tune / Quality level / Rate control / Keyframe interval).
//!
//! Every control row is `[120 label][fill control][72 value]` (the widgets
//! row helpers), so value boxes share one right edge.
//!
//! Staged edits go through [`UiAction::SetNextStream`] /
//! [`UiAction::ApplyPresetToNextStream`]; live edits through
//! [`UiAction::EditDestination`] / [`UiAction::ApplyPresetToDestination`] and
//! apply immediately (no Apply button; the head shows "Applying" while the
//! lib's `apply_rx` is pending or the branch is `Rebuilding`). Sliders keep
//! their value in [`StreamSettingsState`] while dragged and push one action on
//! release. Codecs this machine's encoder backend can't produce are greyed
//! out (`AppState::supported_codecs`, as the Electron app did).
//!
//! Owns [`PopoverKind::AlsoSendTo`] / [`PopoverKind::MoveTo`] (same lists as
//! the bar's "N rooms ▾", see `action_bar::RoomTargets`).

use egui::{Align, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use simple_screensharing_client::capture::{QualityLevel, RateControlMode, TuneMode, VideoCodec};
use simple_screensharing_client::presets::{
    self, BITRATE_STEP_KBPS, CODEC_DISPLAY_ORDER, EncoderPreset, EncoderTuning, MIN_BITRATE_KBPS, codec_description, codec_label,
};
use simple_screensharing_client::rooms::BandwidthMode;
use simple_screensharing_client::session::{
    AppState, CaptureSourceId, DestEdit, Destination, DestinationId, LocalCapture, MAX_BITRATE_KBPS, RoomHandle,
};

use super::action_bar::{
    PillKind, RoomTargets, dest_kbps, dest_pill, find_source, level_lit, pop_heading, rooms_count, screen_name, source_size,
    video_destination_count,
};
use super::{Cx, NextStream, Popover, PopoverKind, UiAction, tr, tr_with};
use crate::theme::{self, Weight, font};
use crate::widgets::{
    self, BannerKind, Btn, DestAction, DestRow, FILL, FooterClick, HBox, ModalSpec, Pill, fade, paint_gradient, paint_galley, text_galley,
};

/// Bandwidth slider ranges (the Electron app's).
const TOTAL_RANGE: std::ops::RangeInclusive<u32> = 500..=40_000;
const FLOOR_RANGE: std::ops::RangeInclusive<u32> = 100..=20_000;
const BW_STEP: f64 = 100.0;
/// Keyframe interval slider (mockup).
const GOP_RANGE: std::ops::RangeInclusive<u32> = 250..=4000;
const GOP_STEP: f64 = 250.0;
/// Minimum time the "Applying" pill stays up after a live edit, seconds
/// (mockup `touch()`): a bitrate-only change applies instantly and would
/// otherwise never show any feedback.
const APPLYING_MIN_SECS: f64 = 0.7;

/// Stream-settings modal state.
#[derive(Default)]
pub struct StreamSettingsState {
    adv_open: bool,
    /// The capture the modal was last drawn for: a new one resets the drag
    /// buffers and refreshes the audio source list once.
    opened_for: Option<CaptureSourceId>,
    /// Values of sliders while dragged (pushed on release).
    bitrate: Option<u32>,
    gop: Option<u32>,
    total: Option<u32>,
    floor: Option<u32>,
    /// `ctx` time of the last live edit pushed (min "Applying" duration).
    applied_at: Option<f64>,
}

/// What the modal edits: one live destination, or the next-stream settings
/// a staged capture goes live with.
#[derive(Clone, Copy)]
enum Target {
    Live(DestinationId, DestEdit),
    Staged,
}

impl Target {
    fn values(&self, state: &AppState) -> NextStream {
        match self {
            Target::Live(_, e) => NextStream {
                framerate: e.framerate,
                bitrate_kbps: e.bitrate_kbps,
                tune: e.tune,
                quality_level: e.quality_level,
                rate_control: e.rate_control,
                gop_millis: e.gop_millis,
                codec: e.codec,
            },
            Target::Staged => NextStream::of(state),
        }
    }

    fn action(&self, n: NextStream) -> UiAction {
        match self {
            Target::Live(d, e) => UiAction::EditDestination {
                destination: *d,
                edit: DestEdit {
                    scale_percent: e.scale_percent,
                    framerate: n.framerate,
                    bitrate_kbps: n.bitrate_kbps,
                    tune: n.tune,
                    quality_level: n.quality_level,
                    rate_control: n.rate_control,
                    gop_millis: n.gop_millis,
                    codec: n.codec,
                },
            },
            Target::Staged => UiAction::SetNextStream(n),
        }
    }

    fn preset_action(&self, p: EncoderPreset) -> UiAction {
        match self {
            Target::Live(d, _) => UiAction::ApplyPresetToDestination { destination: *d, preset: p },
            Target::Staged => UiAction::ApplyPresetToNextStream(p),
        }
    }
}

/// Anchors of the two popover buttons, recorded while drawing the body.
#[derive(Default)]
struct Anchors {
    also: Option<Rect>,
    move_to: Option<Rect>,
}

/// Draw the modal for `capture` as seen from `room`.
pub fn show(ctx: &egui::Context, cx: &mut Cx, st: &mut StreamSettingsState, capture: CaptureSourceId, room: RoomHandle) {
    let Some(c) = cx.capture(capture) else { return };
    if st.opened_for != Some(capture) {
        *st = StreamSettingsState { adv_open: st.adv_open, opened_for: Some(capture), ..Default::default() };
        cx.act(UiAction::RefreshAudioSources);
    }
    let staged = c.destinations.is_empty();
    let here: Option<(DestinationId, &Destination)> = c.destinations.iter().find(|(_, d)| d.room == room).map(|(id, d)| (*id, d));
    let target = match here {
        Some((id, d)) => Target::Live(id, d.edit),
        None if staged => Target::Staged,
        // A live capture without a destination in `room` (moved away under
        // the modal): the shell re-points or closes it next frame.
        None => return,
    };
    let room_name = cx.room_name(room);
    let screen = screen_name(c);
    let title = if staged { tr("ui.stream.title_staged", "Set up your stream") } else { tr("ui.stream.title_live", "Stream settings") };
    let sub = if staged {
        tr_with("ui.stream.sub_staged", "%{screen} · goes live in %{room}", &[("screen", &screen), ("room", &room_name)])
    } else {
        tr_with("ui.stream.sub_live", "Live in %{room} · changes apply right away", &[("room", &room_name)])
    };

    let now = ctx.input(|i| i.time);
    let pending = here.is_some_and(|(_, d)| d.apply_rx.is_some() || matches!(d.state(), simple_screensharing_client::capture::destination::DestinationState::Rebuilding { .. }));
    let recent = st.applied_at.is_some_and(|t| now - t < APPLYING_MIN_SECS);
    let applying = !staged && (pending || recent);
    if recent {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
    let applying_t = tr("ui.stream.applying", "Applying");
    let spec = ModalSpec {
        id: "stream_settings",
        title: &title,
        sub: Some(&sub),
        width: theme::MODAL_W_XWIDE,
        head_pill: applying.then(|| Pill::progress(&applying_t).large()),
    };

    let discard = tr("ui.bar.discard", "Discard");
    let later = tr("ui.stream.later", "Later");
    let go_live = tr_with("ui.stream.go_live_in", "Go live in %{room}", &[("room", &room_name)]);
    let stop = tr_with("ui.stream.stop_in", "Stop sharing in %{room}", &[("room", &room_name)]);
    let saved = tr("ui.stream.saved", "Saved");
    let done = tr("ui.stream.done", "Done");
    let ready = matches!(c.status, simple_screensharing_client::session::ShareStatus::Active(_));

    let mut out: Vec<UiAction> = Vec::new();
    let mut anchors = Anchors::default();
    let mut click = FooterClick::None;
    let cxr: &Cx = cx;
    let action = widgets::modal_shell(
        ctx,
        &spec,
        |ui| body(ui, cxr, st, c, capture, room, target, &mut out, &mut anchors),
        |ui| {
            click = if staged {
                widgets::modal_footer(
                    ui,
                    &[Btn::secondary(&discard)],
                    None,
                    &[Btn::secondary(&later), Btn::primary(&go_live).icon("cast").enabled(ready)],
                )
            } else {
                widgets::modal_footer(ui, &[Btn::danger(&stop).icon("stop_screen_share")], Some(&saved), &[Btn::primary(&done)])
            };
        },
    );
    if out.iter().any(|a| matches!(a, UiAction::EditDestination { .. } | UiAction::ApplyPresetToDestination { .. })) {
        st.applied_at = Some(now);
    }
    for a in out {
        cx.act(a);
    }
    match (staged, click) {
        (true, FooterClick::Leading(0)) => cx.act(UiAction::Discard(capture)),
        (true, FooterClick::Trailing(1)) => cx.act(UiAction::GoLive { capture, room }),
        (false, FooterClick::Leading(0)) => {
            if let Some((d, _)) = here {
                cx.act(UiAction::StopDestination(d));
            }
        }
        (_, FooterClick::Trailing(_)) => cx.act(UiAction::CloseModal),
        // Escape with a popover open only closes the popover (the shell
        // consumes it first); a click outside the modal while a popover is
        // open belongs to the popover, not the modal.
        _ if action.close && cx.nav.popover.is_none() => cx.act(UiAction::CloseModal),
        _ => {}
    }

    // The open list first, then the buttons: a click outside the list that
    // lands on the other button must close the list before that button's
    // OpenPopover is queued (actions apply in order).
    rooms_popover(ctx, cx, capture, room);
    if let Some(r) = anchors.also.take() {
        cx.toggle_popover(PopoverKind::AlsoSendTo { capture, room }, r);
    }
    if let Some(r) = anchors.move_to.take() {
        cx.toggle_popover(PopoverKind::MoveTo { capture, room }, r);
    }
    // Audio meter in the Source card.
    if cx.paired_audio(capture).is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

#[allow(clippy::too_many_arguments)]
fn body(
    ui: &mut Ui,
    cx: &Cx,
    st: &mut StreamSettingsState,
    c: &LocalCapture,
    capture: CaptureSourceId,
    room: RoomHandle,
    target: Target,
    out: &mut Vec<UiAction>,
    anchors: &mut Anchors,
) {
    widgets::columns(ui, 2, theme::S3, |cols| {
        let (l, r) = cols.split_at_mut(1);
        let (l, r) = (&mut l[0], &mut r[0]);
        l.spacing_mut().item_spacing.y = theme::S3;
        r.spacing_mut().item_spacing.y = theme::S3;
        source_card(l, cx, c, capture, room, out);
        destinations_card(l, cx, c, room, out, anchors);
        bandwidth_card(l, cx, st, c, target, out);
        video_card(r, cx, st, c, target, out);
        encoder_card(r, cx, st, target, out);
    });
}

// ---------------------------------------------------------------------------
// Where it goes
// ---------------------------------------------------------------------------

fn source_card(ui: &mut Ui, cx: &Cx, c: &LocalCapture, capture: CaptureSourceId, room: RoomHandle, out: &mut Vec<UiAction>) {
    let staged = c.destinations.is_empty();
    let state = cx.state;
    let mut screen = screen_name(c);
    if let Some((w, h)) = source_size(c) {
        screen = format!("{screen} · {w}×{h}");
    }
    let audio = cx.paired_audio(capture);
    let current: Option<String> = if staged {
        cx.shell.staged.get(&room).and_then(|s| s.audio.as_ref().map(|a| a.name.clone()))
    } else {
        audio.map(|(_, a)| a.label.clone())
    };
    let muted = audio.is_some_and(|(id, _)| state.is_audio_share_muted(id));
    let lit = match audio {
        Some((id, _)) if !muted => state.audio_share_level(id).map(level_lit).unwrap_or(0),
        _ => 0,
    };
    let mut names: Vec<String> = vec![tr("ui.stream.no_audio", "No audio")];
    names.extend(state.audio_sources.iter().map(|s| s.name.clone()));
    if let Some(cur) = &current
        && !names[1..].contains(cur)
    {
        names.push(cur.clone());
    }
    let mut idx = current.as_ref().and_then(|cur| names[1..].iter().position(|n| n == cur).map(|i| i + 1)).unwrap_or(0);
    let before = idx;
    let mut mute = muted;
    let change = tr("ui.stream.change", "Change");
    let mute_l = tr("ui.stream.mute_audio", "Mute shared audio");
    widgets::card(ui, &tr("ui.stream.source", "Source"), None, |ui| {
        let label = tr("ui.stream.screen", "Screen");
        if staged {
            let (_, clicked) = widgets::ctrl_row_value(
                ui,
                &label,
                None,
                |ui| widgets::readout(ui, &screen, Align::Min),
                |ui| Btn::secondary(&change).show_sized(ui, theme::VALUE_W).clicked(),
            );
            if clicked {
                // Re-pick: throw this capture away and open the picker again
                // (the modal re-opens by itself once it resolves).
                out.push(UiAction::Discard(capture));
                out.push(UiAction::StageShare(room));
            }
        } else {
            widgets::ctrl_row(ui, &label, None, |ui| widgets::readout(ui, &screen, Align::Min));
        }
        let opts: Vec<&str> = names.iter().map(String::as_str).collect();
        widgets::ctrl_row_value(
            ui,
            &tr("ui.stream.audio", "Audio"),
            None,
            |ui| widgets::select(ui, "stream_audio", &mut idx, &opts),
            |ui| widgets::value_box_with(ui, |ui| widgets::meter(ui, lit)),
        );
        let enabled = audio.is_some();
        let r = widgets::ctrl_row(ui, "", None, |ui| widgets::checkbox_enabled(ui, &mut mute, &mute_l, enabled));
        if r.changed() {
            out.push(UiAction::SetSharedAudioMuted { capture, muted: mute });
        }
    });
    if idx != before {
        if idx == 0 {
            out.push(UiAction::StopSharingAudio { capture });
        } else if let Some(source) = find_source(cx, &names[idx]) {
            out.push(UiAction::ShareAudio { capture, source });
        }
    }
}

fn destinations_card(
    ui: &mut Ui,
    cx: &Cx,
    c: &LocalCapture,
    room: RoomHandle,
    out: &mut Vec<UiAction>,
    anchors: &mut Anchors,
) {
    let staged = c.destinations.is_empty();
    let (title, count) = if staged {
        (tr("ui.stream.goes_live_in", "Goes live in"), rooms_count(1))
    } else {
        (tr("ui.stream.sending_to", "Sending to"), rooms_count(c.destinations.len()))
    };
    struct Row {
        id: Option<DestinationId>,
        current: bool,
        name: String,
        pill: (PillKind, String),
        kbps: String,
    }
    let rows: Vec<Row> = if staged {
        vec![Row {
            id: None,
            current: true,
            name: cx.room_name(room),
            pill: (PillKind::Muted, tr("ui.pill.not_live", "Not live")),
            kbps: "\u{2014}".into(),
        }]
    } else {
        c.destinations
            .iter()
            .map(|(id, d)| Row {
                id: Some(*id),
                current: d.room == room,
                name: cx.room_name(d.room),
                pill: dest_pill(c, d),
                kbps: format!("{} kbps", dest_kbps(cx, *id, d)),
            })
            .collect()
    };
    let also = tr("ui.stream.also_send_to", "Also send to…");
    let move_to = tr("ui.stream.move_to", "Move to…");
    widgets::card(ui, &title, Some(&count), |ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = theme::S1;
            let note = tr("ui.stream.this_room", "this room");
            for r in &rows {
                let remove_tip = tr_with("ui.stream.stop_in", "Stop sharing in %{room}", &[("room", &r.name)]);
                let row = DestRow {
                    current: r.current,
                    name: &r.name,
                    note: &note,
                    remove_tip: &remove_tip,
                    pill: r.pill.0.pill(&r.pill.1),
                    kbps: &r.kbps,
                    removable: r.id.is_some(),
                };
                if widgets::dest_row(ui, &row) == DestAction::Remove
                    && let Some(id) = r.id
                {
                    out.push(UiAction::StopDestination(id));
                }
            }
        });
        if !staged {
            // `btn_row`'s exact shape (equal fills, one height), built here
            // so the two cells' rects can anchor their popovers.
            let btns = [Btn::secondary(&also).icon("add"), Btn::secondary(&move_to).icon("swap_horiz")];
            let row = HBox::new("btn_row", theme::CTL_H).slots(&[FILL, FILL]).show(ui);
            for (i, b) in btns.iter().enumerate() {
                let w = row.cell(i).width();
                if row.show(ui, i, Align::Min, |ui| b.show_sized(ui, w)).clicked() {
                    if i == 0 {
                        anchors.also = Some(row.cell(i));
                    } else {
                        anchors.move_to = Some(row.cell(i));
                    }
                }
            }
        }
    });
}

fn bandwidth_card(ui: &mut Ui, cx: &Cx, st: &mut StreamSettingsState, c: &LocalCapture, target: Target, out: &mut Vec<UiAction>) {
    let state = cx.state;
    let mut mode = state.bandwidth_mode;
    let per = tr("ui.stream.bw_per_stream", "Per-stream maximum");
    let abs = tr("ui.stream.bw_absolute", "Absolute maximum");
    let staged = c.destinations.is_empty();
    widgets::card(ui, &tr("ui.stream.bandwidth", "Upload bandwidth"), Some(&tr("ui.stream.bandwidth_all", "all your streams")), |ui| {
        let changed = widgets::ctrl_row(ui, &tr("ui.stream.bw_mode", "Mode"), None, |ui| {
            widgets::segmented(ui, &mut mode, &[(BandwidthMode::PerStream, per.as_str()), (BandwidthMode::Absolute, abs.as_str())])
        });
        if changed {
            out.push(UiAction::SetBandwidthMode(mode));
        }
        if mode != BandwidthMode::Absolute {
            return;
        }
        let mut total = st.total.unwrap_or(state.absolute_total_kbps);
        let r = widgets::slider_row(
            ui,
            &tr("ui.stream.bw_total", "Total maximum"),
            Some(&tr("ui.stream.bw_total_tip", "kbps across every stream you send")),
            &mut total,
            TOTAL_RANGE,
            BW_STEP,
            0,
            "",
        );
        if let Some(v) = commit(r, total, &mut st.total) {
            out.push(UiAction::SetBandwidthTotal(v));
        }
        let mut floor = st.floor.unwrap_or(state.destination_floor_kbps);
        let r = widgets::slider_row(
            ui,
            &tr("ui.stream.bw_floor", "Floor per stream"),
            Some(&tr("ui.stream.bw_floor_tip", "kbps no stream drops below")),
            &mut floor,
            FLOOR_RANGE,
            BW_STEP,
            0,
            "",
        );
        if let Some(v) = commit(r, floor, &mut st.floor) {
            out.push(UiAction::SetBandwidthFloor(v));
        }
        let n = video_destination_count(cx) + usize::from(staged);
        let over = state.bandwidth_overshoot_kbps();
        if over > 0 || (n as u32).saturating_mul(floor) > total {
            let text = tr_with("ui.stream.bw_over", "Over budget: %{n} streams × %{floor} kbps floor is more than %{total} kbps. No stream drops below its floor.", &[("n", &n.to_string()), ("floor", &floor.to_string()), ("total", &total.to_string())],
            );
            widgets::banner(ui, BannerKind::Hatched, "warning", &text, None);
        } else {
            let share = match target {
                Target::Live(d, _) => state.allocated_kbps(d),
                Target::Staged => None,
            }
            .unwrap_or_else(|| even_share(total, floor, n));
            let text = if n == 1 {
                tr("ui.stream.bw_one_stream", "1 stream gets the budget")
            } else {
                tr_with("ui.stream.bw_n_streams", "%{n} streams share the budget", &[("n", &n.to_string())])
            };
            widgets::ctrl_row_value(
                ui,
                &tr("ui.stream.bw_share", "Per stream"),
                None,
                |ui| widgets::readout(ui, &text, Align::Max),
                |ui| widgets::value_box(ui, &share.to_string()),
            );
        }
    });
}

// ---------------------------------------------------------------------------
// How it's encoded
// ---------------------------------------------------------------------------

fn video_card(ui: &mut Ui, cx: &Cx, st: &mut StreamSettingsState, c: &LocalCapture, target: Target, out: &mut Vec<UiAction>) {
    let state = cx.state;
    let v = target.values(state);
    let fps_opts = presets::framerate_options_for(v.framerate);
    let fps_labels: Vec<String> = fps_opts.iter().map(|f| tr_with("ui.stream.fps", "%{fps} fps", &[("fps", &f.to_string())])).collect();
    let mut fps_idx = fps_opts.iter().position(|f| *f == v.framerate).unwrap_or(0);
    let supported = AppState::supported_codecs();
    widgets::card(ui, &tr("ui.stream.video", "Video"), None, |ui| {
        let labels: Vec<&str> = fps_labels.iter().map(String::as_str).collect();
        let r = widgets::ctrl_row(ui, &tr("ui.stream.framerate", "Framerate"), None, |ui| widgets::select(ui, "stream_fps", &mut fps_idx, &labels));
        if r.changed() {
            out.push(target.action(NextStream { framerate: fps_opts[fps_idx], ..v }));
        }
        let bitrate_l = tr("ui.stream.bitrate", "Bitrate");
        if state.bandwidth_mode == BandwidthMode::Absolute {
            let value = match target {
                Target::Live(d, _) => c.destinations.get(&d).map(|dd| dest_kbps(cx, d, dd)),
                Target::Staged => Some(staged_share(cx)),
            }
            .map(|k| k.to_string())
            .unwrap_or_else(|| "\u{2014}".into());
            widgets::ctrl_row_value(
                ui,
                &bitrate_l,
                Some(&tr("ui.stream.bitrate_abs_tip", "Absolute maximum mode: the upload budget splits the bitrate across all your streams.")),
                |ui| widgets::readout(ui, &tr("ui.stream.bitrate_by_budget", "Set by upload budget"), Align::Max),
                |ui| widgets::value_box(ui, &value),
            );
        } else {
            let mut kbps = st.bitrate.unwrap_or(v.bitrate_kbps);
            let r = widgets::slider_row(
                ui,
                &bitrate_l,
                Some(&tr("ui.stream.bitrate_tip", "kbps for this room. Per-stream mode: you set each room's bitrate yourself.")),
                &mut kbps,
                MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS,
                f64::from(BITRATE_STEP_KBPS),
                0,
                "",
            );
            if let Some(k) = commit(r, kbps, &mut st.bitrate) {
                out.push(target.action(NextStream { bitrate_kbps: k, ..v }));
            }
        }
        let opts: Vec<(VideoCodec, &str, bool, &str)> =
            CODEC_DISPLAY_ORDER.iter().map(|c| (*c, codec_label(*c), supported.contains(c), codec_description(*c))).collect();
        let mut codec = v.codec;
        let unsupported = tr("ui.stream.codec_unsupported", "This machine's encoder can't produce this codec.");
        let changed = widgets::ctrl_row(ui, &tr("ui.stream.codec", "Codec"), None, |ui| seg_gated(ui, &mut codec, &opts, &unsupported));
        if changed {
            out.push(target.action(NextStream { codec, ..v }));
        }
    });
}

fn encoder_card(ui: &mut Ui, cx: &Cx, st: &mut StreamSettingsState, target: Target, out: &mut Vec<UiAction>) {
    let v = target.values(cx.state);
    let tuning = EncoderTuning { tune: v.tune, quality_level: v.quality_level, rate_control: v.rate_control, gop_millis: v.gop_millis };
    let mut preset = EncoderPreset::detect(tuning);
    let preset_opts: Vec<(Option<EncoderPreset>, &str, bool, &str)> =
        EncoderPreset::ALL.iter().map(|p| (Some(*p), p.label(), true, p.description())).collect();
    let adv = tr("ui.stream.advanced", "Advanced tuning");
    let (low, best) = (tr("ui.stream.tune_low", "Low latency"), tr("ui.stream.tune_best", "Best quality"));
    let (perf, bal, qual) = (
        tr("ui.stream.ql_performance", "Performance"),
        tr("ui.stream.ql_balanced", "Balanced"),
        tr("ui.stream.ql_quality", "Quality"),
    );
    let (eff, maxq) = (tr("ui.stream.rc_efficient", "Efficient"), tr("ui.stream.rc_max_quality", "Max quality"));
    widgets::card(ui, &tr("ui.stream.encoder", "Encoder"), None, |ui| {
        let changed = widgets::ctrl_row(ui, &tr("ui.stream.preset", "Preset"), None, |ui| seg_gated(ui, &mut preset, &preset_opts, ""));
        if changed && let Some(p) = preset {
            out.push(target.preset_action(p));
        }
        widgets::collapser(ui, &mut st.adv_open, &adv);
        if !st.adv_open {
            return;
        }
        let mut tune = v.tune;
        if widgets::ctrl_row(ui, &tr("ui.stream.tune", "Tune"), None, |ui| {
            widgets::segmented(ui, &mut tune, &[(TuneMode::LowLatency, low.as_str()), (TuneMode::BestQuality, best.as_str())])
        }) {
            out.push(target.action(NextStream { tune, ..v }));
        }
        let mut ql = v.quality_level;
        if widgets::ctrl_row(ui, &tr("ui.stream.quality_level", "Quality level"), None, |ui| {
            widgets::segmented(
                ui,
                &mut ql,
                &[(QualityLevel::Performance, perf.as_str()), (QualityLevel::Balanced, bal.as_str()), (QualityLevel::Quality, qual.as_str())],
            )
        }) {
            out.push(target.action(NextStream { quality_level: ql, ..v }));
        }
        let mut rc = v.rate_control;
        if widgets::ctrl_row(
            ui,
            &tr("ui.stream.rate_control", "Rate control"),
            Some(&tr("ui.stream.rate_control_tip", "Efficient = QVBR, Max quality = CQP")),
            |ui| widgets::segmented(ui, &mut rc, &[(RateControlMode::Qvbr, eff.as_str()), (RateControlMode::Cqp, maxq.as_str())]),
        ) {
            out.push(target.action(NextStream { rate_control: rc, ..v }));
        }
        let mut gop = st.gop.unwrap_or(v.gop_millis);
        let r = widgets::slider_row(
            ui,
            &tr("ui.stream.keyframe", "Keyframe interval"),
            Some(&tr("ui.stream.keyframe_tip", "GOP length. Shorter recovers faster from fast motion, costs more bitrate.")),
            &mut gop,
            GOP_RANGE,
            GOP_STEP,
            0,
            " ms",
        );
        if let Some(g) = commit(r, gop, &mut st.gop) {
            out.push(target.action(NextStream { gop_millis: g, ..v }));
        }
    });
}

// ---------------------------------------------------------------------------
// Popovers
// ---------------------------------------------------------------------------

/// "Also send to…" / "Move to…" (one at a time).
fn rooms_popover(ctx: &egui::Context, cx: &mut Cx, capture: CaptureSourceId, room: RoomHandle) {
    let Some(pop): Option<Popover> = cx.nav.popover.clone() else { return };
    let also = match pop.kind {
        PopoverKind::AlsoSendTo { capture: c, room: r } if c == capture && r == room => true,
        PopoverKind::MoveTo { capture: c, room: r } if c == capture && r == room => false,
        _ => return,
    };
    let targets = RoomTargets::of(cx, capture, room);
    let heading = if also {
        tr("ui.stream.also_heading", "Also send this stream to")
    } else {
        tr("ui.stream.move_heading", "Move this stream to")
    };
    let mut picked = None;
    let open = super::popover(ctx, &pop, super::action_bar::POPOVER_W, |ui| {
        pop_heading(ui, &heading);
        if also {
            targets.also_items(ui, &mut picked);
        } else {
            targets.move_items(ui, &mut picked);
        }
    });
    targets.finish(cx, picked, open);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// An even split of the Absolute budget over `n` streams, rounded down to
/// 100 kbps, never below the floor: the estimate shown before the allocator
/// has run for a stream (a staged one goes live with roughly this). The real
/// allocator weights by resolution × framerate.
fn even_share(total: u32, floor: u32, n: usize) -> u32 {
    (total / n.max(1) as u32 / 100 * 100).max(floor)
}

/// [`even_share`] for a staged capture joining the current streams.
fn staged_share(cx: &Cx) -> u32 {
    let n = video_destination_count(cx) + 1;
    even_share(cx.state.absolute_total_kbps, cx.state.destination_floor_kbps, n)
}

/// A slider row's value to push: on release of a drag, or on a non-drag
/// change (a click on the track, a typed / dragged value box). While a drag
/// is running the value lives in `buf`.
fn commit(r: Response, value: u32, buf: &mut Option<u32>) -> Option<u32> {
    if r.dragged() && !r.drag_stopped() {
        *buf = Some(value);
        return None;
    }
    if r.drag_stopped() || r.changed() {
        *buf = None;
        return Some(value);
    }
    if !r.dragged() {
        *buf = None;
    }
    None
}

/// [`widgets::segmented`] with per-option enable + tooltip: the codec picker
/// greys out codecs the encoder backend can't produce, and presets carry
/// their description. Same look as `segmented` (32 px, equal segments, gap
/// 3 inside a 3 px inset); a disabled segment is faded and ignores clicks.
/// Returns `true` when the selection changed.
fn seg_gated<T: PartialEq + Copy>(ui: &mut Ui, current: &mut T, options: &[(T, &str, bool, &str)], disabled_tip: &str) -> bool {
    let w = ui.available_width();
    let (outer, resp) = ui.allocate_exact_size(vec2(w, theme::CTL_H), Sense::hover());
    let cr = |r: u8| CornerRadius::same(r);
    ui.painter().rect(outer, cr(theme::ROUNDING_MD), theme::ELEV_1B, Stroke::new(1.0_f32, theme::INSET_BORDER), StrokeKind::Inside);
    let inner = outer.shrink(3.0);
    let n = options.len().max(1);
    let spans = widgets::slot_spans(inner.width(), 3.0, &vec![widgets::Slot::Fill; n]);
    let mut changed = false;
    for (i, ((value, label, enabled, tip), (x, sw))) in options.iter().zip(spans).enumerate() {
        let seg = Rect::from_min_size(pos2(inner.left() + x, inner.top()), vec2(sw, inner.height()));
        let sense = if *enabled { Sense::click() } else { Sense::hover() };
        let r = ui.interact(seg, resp.id.with(i), sense);
        let selected = *current == *value;
        let fg = if selected {
            paint_gradient(ui.painter(), seg, cr(theme::ROUNDING_XS), 180.0, theme::ACCENT_LIGHT, theme::ACCENT);
            theme::PRIMARY_TEXT
        } else if r.hovered() && *enabled {
            ui.painter().rect_filled(seg, cr(theme::ROUNDING_XS), theme::WASH_HOVER_1);
            theme::TEXT
        } else {
            fade(theme::TEXT_DIM, *enabled)
        };
        let f = if selected { font(Weight::Bold, theme::FS_SMALL) } else { font(Weight::Regular, theme::FS_SMALL) };
        let g = text_galley(ui, label, f, fg, sw - 8.0, 0.0);
        paint_galley(ui, seg, g, Align::Center);
        let tip = if *enabled { *tip } else { disabled_tip };
        let r = if tip.is_empty() { r } else { r.on_hover_text(tip) };
        if *enabled {
            let r = r.on_hover_cursor(egui::CursorIcon::PointingHand);
            if r.clicked() && !selected {
                *current = *value;
                changed = true;
            }
        }
    }
    changed
}
