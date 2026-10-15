//! The stream-settings modal (owner: I15), for [`Modal::StreamSettings`]:
//! staged ("Set up your stream", footer `[Discard][fill][Later][Go live in X]`)
//! when the capture has no destination yet, live ("Stream settings",
//! `[Stop sharing in X][fill][✓ Saved][Done]`) otherwise. Width
//! `MODAL_W_XWIDE` (1120, capped at window − 48); head band
//! `[fill title/sub][96 Applying pill][32 ×]`.
//!
//! Body: two equal columns (gap 12), each a vbox of cards (gap 12):
//! - where it goes: Source (Screen / Audio + meter / Mute), Sending to (dest
//!   rows `[18][fill][≥84 pill][76 kbps][32 ×]` + an Info banner "an idle screen
//!   sends nothing until it changes" while a Starting row waits for the
//!   capture's first frame + equal `[Also send to…][Move to…]`),
//!   Upload bandwidth (Mode / Total / Floor / Per stream or over-budget banner);
//! - how it's encoded: Video (Framerate / Bitrate / Codec; no encoder-limit
//!   banner or "Use H.265" offer since 2026-10-15, user: "just annoying"), Encoder (Preset ×4
//!   + collapsible Advanced: Tune / Quality level / Keyframe interval).
//!
//! Every control row is `[120 label][fill control][72 value]` (the widgets
//! row helpers), so value boxes share one right edge.
//!
//! Narrow windows: when half the body is narrower than [`column_need`] (the
//! widest segmented control's labels at full length, or the Screen readout up
//! to [`READOUT_NEED_CAP`]), the two columns stack into one in reading order
//! (where it goes, then how it's encoded); the modal body already scrolls.
//! The Preset and Codec rows ([`seg_gated`]) size their segments to their
//! labels when equal segments would truncate (German's four presets), so they
//! never force the stack on their own (#47: they did, at every width).
//! In practice (checked on the real app, #47): two columns at 1280 and up in
//! en and de, one column at the 960 minimum (912 px modal). Nothing truncates
//! either way, except a window title longer than the cap (full text on hover).
//!
//! `SSS_UI_DUMP=<file>` (set by `client/scripts/launch-lib.sh`) makes the
//! modal write its driven controls' rects and values for `drive.sh`; see
//! [`drive_dump`].
//!
//! Staged edits go through [`UiAction::SetNextStream`] /
//! [`UiAction::ApplyPresetToNextStream`]; live edits through
//! [`UiAction::EditDestination`] / [`UiAction::ApplyPresetToDestination`] and
//! apply immediately (no Apply button; the head shows "Applying" while the
//! lib's `apply_rx` is pending or the branch is `Rebuilding`). Sliders keep
//! their value in [`StreamSettingsState`] while dragged and push one action on
//! release. Codecs this machine's encoder backend can't produce are greyed
//! out (`AppState::supported_codecs`, as the former Electron app did).
//!
//! Owns [`PopoverKind::AlsoSendTo`] / [`PopoverKind::MoveTo`] (same lists as
//! the bar's "N rooms ▾", see `action_bar::RoomTargets`).

use egui::{Align, CornerRadius, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use simple_screensharing_client::capture::{QualityLevel, TuneMode, VideoCodec};
use simple_screensharing_client::presets::{
    self, BITRATE_STEP_KBPS, CODEC_DISPLAY_ORDER, EncoderPreset, EncoderTuning, MIN_BITRATE_KBPS, codec_description, codec_label,
};
use simple_screensharing_client::rooms::BandwidthMode;
use simple_screensharing_client::session::{
    AppState, CaptureSourceId, DestEdit, Destination, DestinationId, LocalCapture, MAX_BITRATE_KBPS, RoomHandle,
};

use super::action_bar::{
    PillKind, RoomTargets, awaiting_first_frame, dest_kbps, dest_pill, find_source, level_lit, pop_heading, rooms_count, screen_name,
    source_size, video_destination_count, waiting_hint,
};
use super::{Cx, NextStream, Popover, PopoverKind, UiAction, tr, tr_with};
use crate::theme::{self, Weight, font};
use crate::widgets::{
    self, BannerKind, Btn, DestAction, DestRow, FILL, FooterClick, HBox, ModalSpec, Pill, fade, paint_gradient, paint_galley, text_galley,
};

/// Bandwidth slider ranges (carried over from the former Electron app).
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
/// Most of the Screen readout's width that [`column_need`] makes room for
/// (fits "Bildschirmfreigabe · 3840×2160"); longer truncates with a tooltip.
const READOUT_NEED_CAP: f32 = 260.0;

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
    /// Audio source ids the level meters were started for (staged modal
    /// only; `None` = no taps started by this modal).
    taps: Option<Vec<u32>>,
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
        *st = StreamSettingsState { adv_open: st.adv_open, opened_for: Some(capture), taps: st.taps.take(), ..Default::default() };
        cx.act(UiAction::RefreshAudioSources);
    }
    let staged = c.destinations.is_empty();
    sync_level_taps(cx, st, staged);
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
    drive_dump::begin();
    let action = widgets::modal_shell(
        ctx,
        &spec,
        |ui| body(ui, cxr, st, c, capture, room, target, &mut out, &mut anchors),
        |ui| {
            // The footer row's rect (`modal_footer` is one full-width HBox).
            let row = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), theme::CTL_H_LG));
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
            // Footer row: leading button(s) from its left edge, trailing from its right.
            drive_dump::rect(|| "footer".into(), row);
        },
    );
    if out.iter().any(|a| matches!(a, UiAction::EditDestination { .. } | UiAction::ApplyPresetToDestination { .. })) {
        st.applied_at = Some(now);
    }
    for a in out {
        cx.act(a);
    }
    // Any footer button, Escape or a click outside ends the modal: release
    // the meters it started (nothing else stops them; the bar only stops the
    // taps of its own popover).
    if (!matches!(click, FooterClick::None) || (action.close && cx.nav.popover.is_none())) && st.taps.take().is_some() {
        cx.act(UiAction::StopLevelTaps);
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
    let closing = !matches!(click, FooterClick::None) || (action.close && cx.nav.popover.is_none());
    drive_dump::line(|| format!("mode {}\nready {}\nopen {}", if staged { "staged" } else { "live" }, u8::from(ready), u8::from(!closing)));
    drive_dump::flush(ctx);

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
    if cx.paired_audio(capture).is_some() || st.taps.is_some() {
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

/// Test hook for `client/scripts/drive.sh` (#47): with `SSS_UI_DUMP=<file>`
/// set, the modal writes where its driven controls are and what they hold,
/// one `key value…` per line, whenever that changes:
/// `rect NAME x0 y0 x1 y1` (logical points, window-relative; NAME `fps`,
/// `codec.I` / `preset.I` indexing `codec_order` / `preset_order`,
/// `advanced`, `footer`), `fps` / `fps_options` / `codec` / `preset` /
/// `advanced` / `mode` / `ready` / `open`, plus `ppp` and `screen W H` so the
/// script can map points to pixels. Unset (always, outside the scripts):
/// nothing is recorded.
///
/// `Why` (#47): `drive.sh` clicked fixed pixel positions read off old
/// screenshots; the modal grew (the encoder-limit banner) and `setup` silently
/// kept 30 fps while `golive` missed. Positions from the live layout, and the
/// values read back, make a miss impossible to overlook.
mod drive_dump {
    use std::cell::RefCell;
    use std::fmt::Write as _;
    use std::path::PathBuf;
    use std::sync::OnceLock;

    use egui::Rect;

    fn path() -> Option<&'static PathBuf> {
        static P: OnceLock<Option<PathBuf>> = OnceLock::new();
        P.get_or_init(|| std::env::var_os("SSS_UI_DUMP").map(PathBuf::from)).as_ref()
    }

    thread_local! {
        static BUF: RefCell<String> = const { RefCell::new(String::new()) };
        static LAST: RefCell<String> = const { RefCell::new(String::new()) };
    }

    pub fn begin() {
        if path().is_some() {
            BUF.with_borrow_mut(String::clear);
        }
    }

    pub fn line(text: impl FnOnce() -> String) {
        if path().is_some() {
            BUF.with_borrow_mut(|b| {
                b.push_str(&text());
                b.push('\n');
            });
        }
    }

    pub fn rect(name: impl FnOnce() -> String, r: Rect) {
        line(|| format!("rect {} {:.0} {:.0} {:.0} {:.0}", name(), r.min.x, r.min.y, r.max.x, r.max.y));
    }

    /// Write the frame's lines if they differ from the last write (atomic rename).
    pub fn flush(ctx: &egui::Context) {
        let Some(p) = path() else { return };
        let screen = ctx.content_rect();
        let mut text = BUF.with_borrow(Clone::clone);
        let _ = write!(text, "ppp {}\nscreen {:.0} {:.0}\n", ctx.pixels_per_point(), screen.width(), screen.height());
        if LAST.with_borrow(|l| *l == text) {
            return;
        }
        let tmp = p.with_extension("tmp");
        if std::fs::write(&tmp, &text).and_then(|()| std::fs::rename(&tmp, p)).is_ok() {
            LAST.set(text);
        }
    }
}

/// `codec` key for [`drive_dump`]: `h264` / `av1` / `h265`.
fn codec_key(c: VideoCodec) -> String {
    codec_label(c).to_lowercase().replace('.', "")
}

/// Keep a level tap running for every audio source while the *staged* modal
/// is open, so the Source card's meter moves before anything is shared (a
/// staged capture has no audio share to read a level from). Same idea as the
/// bar's audio popover (`action_bar::sync_level_taps`): start when it opens,
/// restart when the source list changes, stop when it is no longer needed.
///
/// `Why the modal has to do this itself:` taps only exist after an explicit
/// [`UiAction::StartLevelTaps`], and the bar starts them only for its own
/// popover. The live modal reads the running share's level instead, so only
/// the staged one needs them.
fn sync_level_taps(cx: &mut Cx, st: &mut StreamSettingsState, staged: bool) {
    if !staged {
        if st.taps.take().is_some() {
            cx.act(UiAction::StopLevelTaps);
        }
        return;
    }
    let ids: Vec<u32> = cx.state.audio_sources.iter().map(|s| s.id).collect();
    if st.taps.as_ref() != Some(&ids) && !ids.is_empty() {
        cx.act(UiAction::StartLevelTaps(ids.clone()));
        st.taps = Some(ids);
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
    let col_w = (ui.available_width() - theme::S3) / 2.0;
    if col_w >= column_need(ui, c) {
        widgets::columns(ui, 2, theme::S3, |cols| {
            let (l, r) = cols.split_at_mut(1);
            let (l, r) = (&mut l[0], &mut r[0]);
            l.spacing_mut().item_spacing.y = theme::S3;
            r.spacing_mut().item_spacing.y = theme::S3;
            source_card(l, cx, c, capture, room, out);
            destinations_card(l, cx, c, capture, room, out, anchors);
            bandwidth_card(l, cx, st, c, target, out);
            video_card(r, cx, st, c, target, out);
            encoder_card(r, cx, st, target, out);
        });
    } else {
        // Narrow window: one column in reading order (the body scrolls).
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = theme::S3;
            source_card(ui, cx, c, capture, room, out);
            destinations_card(ui, cx, c, capture, room, out, anchors);
            bandwidth_card(ui, cx, st, c, target, out);
            video_card(ui, cx, st, c, target, out);
            encoder_card(ui, cx, st, target, out);
        });
    }
}

/// Width one body column needs so no segmented label and no screen readout
/// truncates: card chrome + `[120 label][8][control]`, the control being the
/// widest of every segmented row ([`seg_need`]: equal segments for
/// `widgets::segmented`, label-proportional ones for the Preset and Codec rows,
/// which [`seg_gated`] draws that way when equal ones would truncate) and the
/// Screen readout (+ the 72 value slot when
/// staged, for "Change"). The readout counts up to [`READOUT_NEED_CAP`] only:
/// a long window title truncates (full text on hover) instead of forcing one
/// column at every width.
///
/// `Why` measured and not a fixed window width: German labels are ~20 %
/// longer, so a fixed breakpoint either stacks English needlessly or still
/// truncates German.
fn column_need(ui: &Ui, c: &LocalCapture) -> f32 {
    let readout_w = widgets::text_width(ui, &screen_readout(c), font(Weight::Mono, theme::FS_SMALL), 0.0);
    column_need_for(ui, readout_w, c.destinations.is_empty())
}

/// [`column_need`] from the Screen readout's text width (`staged`: + the
/// "Change" value slot).
fn column_need_for(ui: &Ui, readout_w: f32, staged: bool) -> f32 {
    let t = |pairs: &[(&'static str, &'static str)]| pairs.iter().map(|(k, en)| tr(k, en)).collect::<Vec<String>>();
    let need = |labels: &[String], equal: bool| seg_need(ui, &labels.iter().map(String::as_str).collect::<Vec<_>>(), equal);
    let presets: Vec<String> = EncoderPreset::ALL.iter().map(|p| p.label().to_string()).collect();
    let codecs: Vec<String> = CODEC_DISPLAY_ORDER.iter().map(|c| codec_label(*c).to_string()).collect();
    let control = [
        // `widgets::segmented`: always equal segments.
        need(&t(&[("ui.stream.bw_per_stream", "Per-stream maximum"), ("ui.stream.bw_absolute", "Absolute maximum")]), true),
        need(&t(&[("ui.stream.tune_low", "Low latency"), ("ui.stream.tune_best", "Best quality")]), true),
        need(&t(&[("ui.stream.ql_performance", "Performance"), ("ui.stream.ql_balanced", "Balanced"), ("ui.stream.ql_quality", "Quality")]), true),
        // [`seg_gated`]: label-proportional when equal would truncate.
        need(&presets, false),
        need(&codecs, false),
    ]
    .into_iter()
    .fold(0.0, f32::max);
    let readout = readout_w.ceil().min(READOUT_NEED_CAP) + if staged { theme::S2 + theme::VALUE_W } else { 0.0 };
    // Card: 1 px border + 12 padding each side.
    2.0 * (1.0 + theme::S3) + theme::LABEL_W + theme::S2 + control.max(readout)
}

/// The Screen readout; the full text on hover when it truncates (a long
/// window title past [`READOUT_NEED_CAP`]).
fn screen_ro(ui: &mut Ui, text: &str) -> Response {
    let full = widgets::text_width(ui, text, font(Weight::Mono, theme::FS_SMALL), 0.0);
    let r = widgets::readout(ui, text, Align::Min);
    if full > r.rect.width() + 0.5 { r.on_hover_text(text) } else { r }
}

/// The Source card's Screen readout: "Display 1 · 1920×1080".
fn screen_readout(c: &LocalCapture) -> String {
    let screen = screen_name(c);
    match source_size(c) {
        Some((w, h)) => format!("{screen} · {w}×{h}"),
        None => screen,
    }
}

// ---------------------------------------------------------------------------
// Where it goes
// ---------------------------------------------------------------------------

fn source_card(ui: &mut Ui, cx: &Cx, c: &LocalCapture, capture: CaptureSourceId, room: RoomHandle, out: &mut Vec<UiAction>) {
    let staged = c.destinations.is_empty();
    let state = cx.state;
    let screen = screen_readout(c);
    let audio = cx.paired_audio(capture);
    let current: Option<String> = if staged {
        cx.shell.staged.get(&room).and_then(|s| s.audio.as_ref().map(|a| a.name.clone()))
    } else {
        audio.map(|(_, a)| a.label.clone())
    };
    let muted = audio.is_some_and(|(id, _)| state.is_audio_share_muted(id));
    let lit = match audio {
        Some((id, _)) if !muted => state.audio_share_level(id).map(level_lit).unwrap_or(0),
        // Staged: nothing is shared yet, read the chosen source's own tap.
        None if staged => cx
            .shell
            .staged
            .get(&room)
            .and_then(|s| s.audio.as_ref())
            .and_then(|a| cx.source_level(a.id))
            .map(level_lit)
            .unwrap_or(0),
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
                |ui| screen_ro(ui, &screen),
                |ui| Btn::secondary(&change).show_sized(ui, theme::VALUE_W).clicked(),
            );
            if clicked {
                // Re-pick: throw this capture away and open the picker again
                // (the modal re-opens by itself once it resolves).
                out.push(UiAction::Discard(capture));
                out.push(UiAction::StageShare(room));
            }
        } else {
            widgets::ctrl_row(ui, &label, None, |ui| screen_ro(ui, &screen));
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
    capture: CaptureSourceId,
    room: RoomHandle,
    out: &mut Vec<UiAction>,
    anchors: &mut Anchors,
) {
    let staged = c.destinations.is_empty();
    // Starting only because an idle monitor hasn't sent a first frame yet.
    let waiting = c.destinations.values().any(|d| awaiting_first_frame(cx, capture, c, d));
    let (title, count) = if staged {
        (tr("ui.stream.goes_live_in", "Goes live in"), rooms_count(1))
    } else {
        (tr("ui.stream.sending_to", "Sending to"), rooms_count(c.destinations.len()))
    };
    // "this room" means the room on screen behind the modal, never just the
    // modal's target: after "Move to…" from room B the modal follows the
    // stream to room C while B is still the room being viewed (stage-6 QC).
    let viewed = cx.selected_room().and_then(|r| r.handle);
    struct Row {
        id: Option<DestinationId>,
        current: bool,
        viewed: bool,
        name: String,
        pill: (PillKind, String),
        kbps: String,
    }
    let rows: Vec<Row> = if staged {
        vec![Row {
            id: None,
            current: true,
            viewed: viewed == Some(room),
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
                viewed: viewed == Some(d.room),
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
                    note: if r.viewed { &note } else { "" },
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
        if waiting {
            widgets::banner(ui, BannerKind::Info, "info", &waiting_hint(), None);
            // The flag flips on the capture thread; poll so Live shows promptly.
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
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
        drive_dump::rect(|| "fps".into(), r.rect);
        drive_dump::line(|| format!("fps {}\nfps_options {}", v.framerate, fps_opts.iter().map(u32::to_string).collect::<Vec<_>>().join(" ")));
        drive_dump::line(|| format!("codec {}\ncodec_order {}", codec_key(v.codec), CODEC_DISPLAY_ORDER.iter().map(|c| codec_key(*c)).collect::<Vec<_>>().join(" ")));
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
            // The value box is the row's right 72 px.
            drive_dump::rect(|| "bitrate".into(), r.rect);
            drive_dump::line(|| format!("bitrate {}", v.bitrate_kbps));
            if let Some(k) = commit(r, kbps, &mut st.bitrate) {
                out.push(target.action(NextStream { bitrate_kbps: k, ..v }));
            }
        }
        let opts: Vec<(VideoCodec, &str, bool, &str)> =
            CODEC_DISPLAY_ORDER.iter().map(|c| (*c, codec_label(*c), supported.contains(c), codec_description(*c))).collect();
        let mut codec = v.codec;
        let unsupported = tr("ui.stream.codec_unsupported", "This machine's encoder can't produce this codec.");
        let changed = widgets::ctrl_row(ui, &tr("ui.stream.codec", "Codec"), None, |ui| seg_gated(ui, "codec", &mut codec, &opts, &unsupported));
        if changed {
            out.push(target.action(NextStream { codec, ..v }));
        }
    });
}

fn encoder_card(ui: &mut Ui, cx: &Cx, st: &mut StreamSettingsState, target: Target, out: &mut Vec<UiAction>) {
    let v = target.values(cx.state);
    let tuning = EncoderTuning { tune: v.tune, quality_level: v.quality_level, gop_millis: v.gop_millis };
    let mut preset = EncoderPreset::detect(tuning);
    drive_dump::line(|| {
        let order = EncoderPreset::ALL.iter().map(|p| p.value()).collect::<Vec<_>>().join(" ");
        format!("preset {}\npreset_order {order}\nadvanced {}", preset.map_or("custom", EncoderPreset::value), u8::from(st.adv_open))
    });
    let preset_opts: Vec<(Option<EncoderPreset>, &str, bool, &str)> =
        EncoderPreset::ALL.iter().map(|p| (Some(*p), p.label(), true, p.description())).collect();
    let adv = tr("ui.stream.advanced", "Advanced tuning");
    let collapsed_extra = widgets::card(ui, &tr("ui.stream.encoder", "Encoder"), None, |ui| {
        let changed = widgets::ctrl_row(ui, &tr("ui.stream.preset", "Preset"), None, |ui| seg_gated(ui, "preset", &mut preset, &preset_opts, ""));
        if changed && let Some(p) = preset {
            out.push(target.preset_action(p));
        }
        let r = widgets::collapser(ui, &mut st.adv_open, &adv);
        drive_dump::rect(|| "advanced".into(), r.rect);
        if st.adv_open {
            adv_rows(ui, st, target, v, out);
            0.0
        } else {
            // What the rows would add to the card: their height + the gap above them.
            adv_rows_height(ui, target, v) + ui.spacing().item_spacing.y
        }
    });
    // Reserve it below the card instead (see `adv_rows_height`). `add_space` only moves
    // the cursor, which already sits one item gap below the card, so that gap is part
    // of the reservation. (Not at all when open: even a zero spacer would add the gap
    // to the column's height.)
    if collapsed_extra > 0.0 {
        ui.add_space((collapsed_extra - ui.spacing().item_spacing.y).max(0.0));
    }
}

/// The Advanced tuning rows (Tune, Quality level, Keyframe interval).
fn adv_rows(ui: &mut Ui, st: &mut StreamSettingsState, target: Target, v: NextStream, out: &mut Vec<UiAction>) {
    let (low, best) = (tr("ui.stream.tune_low", "Low latency"), tr("ui.stream.tune_best", "Best quality"));
    let (perf, bal, qual) = (
        tr("ui.stream.ql_performance", "Performance"),
        tr("ui.stream.ql_balanced", "Balanced"),
        tr("ui.stream.ql_quality", "Quality"),
    );
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

}

/// Height the Advanced rows take, laid out once in an invisible child that advances
/// nothing. The Encoder card reserves it as empty space below itself while Advanced is
/// collapsed, so the modal is as tall closed as open.
///
/// `Why:` the user disliked the modal growing and shrinking (and jumping, it is
/// centred) when Advanced was toggled (2026-10-15). A hidden layout pass is
/// deterministic (no one-frame resize on first open) and follows font and language;
/// the space sits outside the card (empty body space, not an oversized card). In the
/// two-column layout it only matters when the right column is the taller one.
fn adv_rows_height(ui: &mut Ui, target: Target, v: NextStream) -> f32 {
    let rect = Rect::from_min_size(ui.cursor().min, vec2(ui.available_width(), f32::INFINITY));
    let mut ghost = ui.new_child(egui::UiBuilder::new().max_rect(rect).invisible().sizing_pass());
    // Scratch drag buffers and action sink: the ghost must not touch the real ones.
    adv_rows(&mut ghost, &mut StreamSettingsState::default(), target, v, &mut Vec::new());
    ghost.min_rect().height()
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
/// Bold width of each segment label (the selected one is bold, the widest case).
fn seg_label_widths(ui: &Ui, labels: &[&str]) -> Vec<f32> {
    let bold = font(Weight::Bold, theme::FS_SMALL);
    labels.iter().map(|l| widgets::text_width(ui, l, bold.clone(), 0.0).ceil()).collect()
}

/// Outer width a segmented control needs so no label truncates: 3 px inset
/// each side, 3 px gaps, each segment its label + 8 text inset (+ 2 slack).
/// `equal`: every segment as wide as the widest label (the `segmented` look);
/// otherwise each segment as wide as its own label ([`seg_spans`]' fallback).
fn seg_need(ui: &Ui, labels: &[&str], equal: bool) -> f32 {
    let w = seg_label_widths(ui, labels);
    let n = w.len() as f32;
    let segs = if equal { n * w.iter().copied().fold(0.0, f32::max) } else { w.iter().sum() };
    6.0 + segs + n * (8.0 + 2.0) + 3.0 * (n - 1.0).max(0.0)
}

/// Segment spans `(x, w)` inside a [`seg_gated`] control's inner rect: equal
/// widths while the widest label fits, else label-proportional (each segment
/// its label + 8, the slack shared equally) so nothing truncates.
///
/// `Why` (#47): German's "Niedrige Latenz" made four equal Preset segments
/// need a 585 px column, more than the 1120 px modal's ~534 px half, so the
/// modal stacked into one column at every window width. Sizing segments to
/// their labels only when equal ones would truncate keeps the equal look in
/// English and every roomy layout, and needs ~110 px less in German.
fn seg_spans(ui: &Ui, inner_w: f32, labels: &[&str]) -> Vec<(f32, f32)> {
    let n = labels.len().max(1);
    let w = seg_label_widths(ui, labels);
    let gaps = 3.0 * (n as f32 - 1.0);
    let max = w.iter().copied().fold(0.0, f32::max);
    if (inner_w - gaps) / n as f32 >= max + 8.0 {
        return widgets::slot_spans(inner_w, 3.0, &vec![widgets::Slot::Fill; n]);
    }
    let natural: f32 = w.iter().map(|w| w + 8.0).sum();
    let free = inner_w - gaps - natural;
    // Too narrow even for that (never in this modal: the column stacks
    // first): shrink proportionally and let the labels truncate.
    let (extra, scale) = if free >= 0.0 { (free / n as f32, 1.0) } else { (0.0, ((inner_w - gaps) / natural).max(0.0)) };
    let mut x = 0.0;
    w.iter()
        .map(|w| {
            let span = (x, (w + 8.0) * scale + extra);
            x += span.1 + 3.0;
            span
        })
        .collect()
}

fn seg_gated<T: PartialEq + Copy>(ui: &mut Ui, dump: &str, current: &mut T, options: &[(T, &str, bool, &str)], disabled_tip: &str) -> bool {
    let w = ui.available_width();
    let (outer, resp) = ui.allocate_exact_size(vec2(w, theme::CTL_H), Sense::hover());
    let cr = |r: u8| CornerRadius::same(r);
    ui.painter().rect(outer, cr(theme::ROUNDING_MD), theme::ELEV_1B, Stroke::new(1.0_f32, theme::INSET_BORDER), StrokeKind::Inside);
    let inner = outer.shrink(3.0);
    let labels: Vec<&str> = options.iter().map(|o| o.1).collect();
    let spans = seg_spans(ui, inner.width(), &labels);
    let mut changed = false;
    for (i, ((value, label, enabled, tip), (x, sw))) in options.iter().zip(spans).enumerate() {
        let seg = Rect::from_min_size(pos2(inner.left() + x, inner.top()), vec2(sw, inner.height()));
        drive_dump::rect(|| format!("{dump}.{i}"), seg);
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #47: at the modal's full width (1120 − 2 × 20 frame padding, gap 12)
    /// every shipped locale must get two columns, even with the Screen
    /// readout at its cap and the staged "Change" slot.
    ///
    /// `Why each locale runs in a child process (task #78, flaky tests):` the
    /// locale is process-global (`rust_i18n::set_locale`) and the other tests
    /// run in parallel reading it, so switching to `de` here made
    /// `share_label_translates_known_defaults_only` and others fail at random
    /// (`left: "Bildschirmfreigabe", right: "Screen share"`). In its own
    /// process the switch touches nobody.
    #[test]
    fn every_locale_fits_two_columns_at_full_modal_width() {
        const NAME: &str = "ui::stream_settings::tests::every_locale_fits_two_columns_at_full_modal_width";
        if let Ok(loc) = std::env::var("SSS_TEST_LOCALE") {
            rust_i18n::set_locale(&loc);
            let col = (theme::MODAL_W_XWIDE - 40.0 - theme::S3) / 2.0;
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            let mut need = 0.0;
            // Two frames: fonts load on the first.
            for _ in 0..2 {
                let _ = ctx.run(egui::RawInput::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| need = column_need_for(ui, READOUT_NEED_CAP, true));
                });
            }
            assert!(need <= col, "{loc}: a column needs {need} px, the full-width modal gives {col}");
            return;
        }
        for loc in ["en", "de"] {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", NAME, "--test-threads=1"])
                .env("SSS_TEST_LOCALE", loc)
                .output()
                .expect("spawn the test binary");
            let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
            assert!(out.status.success() && text.contains("1 passed"), "{loc}: child run failed:
{text}");
        }
    }

    /// The Encoder card plus its reserved space is as tall closed as open, so
    /// toggling Advanced never resizes (or moves) the centred modal.
    #[test]
    fn encoder_card_height_does_not_depend_on_advanced() {
        let state = AppState::default();
        let (nav, shell, videos) = (super::super::Nav::default(), super::super::Shell::default(), super::super::VideoViews::default());
        let height = |adv_open: bool| {
            let ctx = egui::Context::default();
            theme::apply(&ctx);
            let mut st = StreamSettingsState { adv_open, ..Default::default() };
            let mut h = 0.0;
            // Two frames: fonts load on the first.
            for _ in 0..2 {
                let _ = ctx.run(egui::RawInput::default(), |ctx| {
                    egui::CentralPanel::default().show(ctx, |ui| {
                        let mut actions = Vec::new();
                        let cx = Cx::new(&state, &nav, &shell, &videos, &mut actions);
                        // A scope's rect is its content, like a body column's.
                        h = ui.scope(|ui| encoder_card(ui, &cx, &mut st, Target::Staged, &mut Vec::new())).response.rect.height();
                    });
                });
            }
            h
        };
        let (open, closed) = (height(true), height(false));
        assert!((open - closed).abs() < 0.5, "open {open} px vs closed {closed} px");
    }
}
