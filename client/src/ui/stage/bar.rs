//! The stage's control bar (task I3, the replica's `.ctlrow`): `[hide sidebar][Copy invite link]` on the left,
//! the button groups and Leave in the middle, `[pop out][fullscreen]` on the right.
//!
//! - **Grouped buttons have a backdrop** (`STAGE_BTN` inside a `STAGE_GROUP` pill); the freestanding ones at
//!   the sides (hide sidebar, Copy invite link, pop out, fullscreen) have none. **Split buttons** (Share
//!   screen, Share audio, Webcam, Layout, Room settings) always show their dropdown arrow, cut off from the
//!   button by a separator line; a lit split button is green (live) or accent (ready). Leave is as tall as a
//!   group (50 px). The people button shows and hides the strip; there is no people list and no arrow
//!   above the avatars.
//! - **Share audio and Webcam** are shares of their own, no screen needed. Audio (I7): one click turns Desktop
//!   audio on, the next off; its arrow opens the source list (`share::audio_menu`) and "All audio settings…".
//!   Webcam (I8): one click starts the camera (the saved one, in its saved mode), the next stops it; its arrow only
//!   offers the cameras and "All webcam settings…" (`share::webcam_menu`, the modal is `webcam_settings.rs`). It is
//!   disabled, with the reason as its tooltip, while there is no `v4l2src`, no camera, or off Linux.
//! - **Narrow windows** (800 px minimum, a ~528 px stage): the label of Copy invite link goes first, then the
//!   bar takes two rows (the middle part on top, the side buttons under it); with the Level loudness button the
//!   middle part no longer fits 528 px (548 + 16), so Leave moves to the second row between the side buttons. Icons never move into a menu.
//!   Pure rule: [`arrange`]. Check en and de.
//! - **Menus** are drawn by `menus.rs` from `Nav::popover` (`PopoverKind::Bar`); this file only anchors them.

use egui::emath::GuiRounding;
use egui::{Align, Color32, CornerRadius, Id, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};
use simple_screensharing_client::audio::AudioChoice;
use simple_screensharing_client::capture::webcam;
use simple_screensharing_client::session::RoomHandle;

use super::{Host, StageLayout, StageState, StageView};
use crate::theme::{self, Weight, font};
use crate::ui::share::{self, live_elsewhere};
use crate::ui::{ConnState, Cx, Popover, PopoverKind, RoomEntry, ShareState, UiAction, drive_dump, tr, tr_with};
use crate::widgets::{self, DISABLED_OPACITY, paint_icon, paint_text, text_width};

/// A dropdown of the bar (`PopoverKind::Bar`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarMenu {
    Share,
    Audio,
    Webcam,
    Layout,
    Room,
    /// Right click on Copy invite link: Copy join link, Copy browser direct link.
    Invite,
}

/// Button heights and widths (replica `.cb`, `.splitb`, `.cgroup`).
const BTN_H: f32 = theme::STAGE_BTN_H;
const SPLIT_W: f32 = 68.0;
const CARET_W: f32 = 24.0;
const PEOPLE_W: f32 = 44.0;
/// Level loudness button, the same width as the people button it sits left of.
const LEVEL_W: f32 = 44.0;
const DIM_W: f32 = 40.0;
const LEAVE_W: f32 = 64.0;
/// A group: 4 px padding and a 1 px border around the buttons, 4 px between them.
const GROUP_PAD: f32 = 5.0;
const GROUP_GAP: f32 = 4.0;
const GROUP_H: f32 = BTN_H + 2.0 * GROUP_PAD;
const BAR_GAP: f32 = theme::S2;
/// One-row height, and the two-row one (6 + group + 2 + 40 + 6).
pub const BAR_H: f32 = 72.0;
const BAR_H_TWO: f32 = 6.0 + GROUP_H + 2.0 + BTN_H + 6.0;

fn group_w(parts: &[f32]) -> f32 {
    2.0 * GROUP_PAD + parts.iter().sum::<f32>() + GROUP_GAP * parts.len().saturating_sub(1) as f32
}

/// The widths [`arrange`] decides from.
pub(super) struct Sizes {
    pub mid: f32,
    /// Leave plus its gap, the part of `mid` that drops under the groups when even the middle part is too wide.
    pub leave: f32,
    /// Left side without / with the Copy invite link label.
    pub left_icons: f32,
    pub left_label: f32,
    pub right: f32,
}

/// How the bar lays out in a stage `w` wide: its height, the invite label, the side padding. One row when the
/// middle part and both sides fit (the label first to go), else two rows. Pure (tested).
pub(super) struct Arrange {
    pub two_rows: bool,
    pub label: bool,
    pub pad: f32,
    /// Two rows and the middle part still too wide (the 800 px window with the Level loudness button): Leave moves
    /// to the second row, between the side buttons.
    pub leave_down: bool,
}

pub(super) fn arrange(w: f32, s: &Sizes) -> Arrange {
    let fits = |left: f32| 2.0 * left.max(s.right) + s.mid + 2.0 * BAR_GAP + 2.0 * theme::S4 <= w;
    if fits(s.left_label) {
        Arrange { two_rows: false, label: true, pad: theme::S4, leave_down: false }
    } else if fits(s.left_icons) {
        Arrange { two_rows: false, label: false, pad: theme::S4, leave_down: false }
    } else {
        let leave_down = s.mid + 2.0 * theme::S2 > w;
        let mid = if leave_down { s.mid - s.leave } else { s.mid };
        Arrange { two_rows: true, label: false, pad: if mid + 2.0 * theme::S4 <= w { theme::S4 } else { theme::S2 }, leave_down }
    }
}

impl Arrange {
    pub fn height(&self) -> f32 {
        if self.two_rows { BAR_H_TWO } else { BAR_H }
    }
}

/// What the bar shows for a stage, read once per frame.
pub(super) struct Bar {
    pub view: StageView,
    pub host: Host,
    pub fullscreen: bool,
    room: Option<RoomEntry>,
    joined: Option<RoomHandle>,
    share: Option<(RoomHandle, ShareState)>,
    layout: StageLayout,
    strip_on: bool,
    popped: bool,
    sidebar_hidden: bool,
}

impl Bar {
    pub fn new(cx: &Cx, st: &StageState, view: &StageView, host: Host, fullscreen: bool) -> Self {
        let room = match view {
            StageView::Room(h) => cx.rooms().into_iter().find(|r| r.handle == Some(*h)),
            StageView::Watched => None,
        };
        Self {
            view: view.clone(),
            host,
            fullscreen,
            joined: room.as_ref().and_then(|r| r.handle).filter(|_| room.as_ref().is_some_and(|r| r.conn == ConnState::Joined)),
            room,
            share: share::share_target(cx, view),
            layout: cx.shell.stage.layout(view),
            strip_on: !st.strip_hidden,
            popped: st.popped.contains(view),
            sidebar_hidden: cx.nav.sidebar_hidden,
        }
    }

    fn has_sidebar_btn(&self) -> bool {
        self.host == Host::Main && !self.fullscreen
    }

    fn mid_w(&self, leave_down: bool) -> f32 {
        let g1 = if self.share.is_some() { group_w(&[SPLIT_W; 3]) } else { 0.0 };
        let g2 = if self.room.is_some() { group_w(&[SPLIT_W, SPLIT_W, LEVEL_W, PEOPLE_W]) } else { group_w(&[SPLIT_W, LEVEL_W, PEOPLE_W]) };
        let leave = if self.room.is_some() && !leave_down { LEAVE_W } else { 0.0 };
        let parts: Vec<f32> = [g1, g2, leave].into_iter().filter(|w| *w > 0.0).collect();
        parts.iter().sum::<f32>() + BAR_GAP * parts.len().saturating_sub(1) as f32
    }

    fn invite_label_w(&self, ui: &Ui) -> f32 {
        text_width(ui, &tr("ui.room.copy_invite", "Copy invite link"), font(Weight::Bold, theme::FS_SMALL), 0.0).ceil()
    }

    pub fn arrange(&self, ui: &Ui, w: f32) -> Arrange {
        let n = f32::from(u8::from(self.has_sidebar_btn()));
        let invite = f32::from(u8::from(self.room.is_some()));
        let icons = (n + invite) * DIM_W + GROUP_GAP * (n + invite - 1.0).max(0.0);
        let label = icons + if invite > 0.0 { 6.0 + self.invite_label_w(ui) } else { 0.0 };
        arrange(w, &Sizes { mid: self.mid_w(false), leave: if self.room.is_some() { LEAVE_W + BAR_GAP } else { 0.0 }, left_icons: icons, left_label: label, right: 2.0 * DIM_W + GROUP_GAP })
    }

    /// Draw the bar into `rect` (its height is `arrange(..).height()`). Menus are not drawn here.
    pub fn show(&self, ui: &mut Ui, cx: &mut Cx, st: &mut StageState, rect: Rect, a: &Arrange) {
        let mid_w = self.mid_w(a.leave_down);
        let (mid_rect, side_rect) = if a.two_rows {
            let mid = Rect::from_min_size(pos2(rect.center().x - mid_w / 2.0, rect.top() + 6.0), vec2(mid_w, GROUP_H));
            (mid, Rect::from_min_max(pos2(rect.left() + a.pad, mid.bottom() + 2.0), pos2(rect.right() - a.pad, rect.bottom() - 6.0)))
        } else {
            let mid = Rect::from_center_size(pos2(rect.center().x, rect.center().y), vec2(mid_w, GROUP_H));
            (mid, Rect::from_x_y_ranges(rect.left() + a.pad..=rect.right() - a.pad, rect.y_range()))
        };
        self.mid(ui, cx, st, mid_rect, a.leave_down.then(|| Rect::from_center_size(side_rect.center(), vec2(LEAVE_W, BTN_H))));
        self.left(ui, cx, side_rect, a.label);
        self.right(ui, cx, st, side_rect);
    }

    fn left(&self, ui: &mut Ui, cx: &mut Cx, side: Rect, label: bool) {
        let mut x = side.left();
        let y = side.center().y - BTN_H / 2.0;
        if self.has_sidebar_btn() {
            let hidden = self.sidebar_hidden;
            let tip = if hidden { tr("ui.bar.show_sidebar", "Show the sidebar") } else { tr("ui.bar.hide_sidebar", "Hide the sidebar") };
            let icon = if hidden { "left_panel_open" } else { "left_panel_close" };
            if dim_btn(ui, Rect::from_min_size(pos2(x, y), vec2(DIM_W, BTN_H)), "sidebar", icon, None, &tip) {
                cx.act(UiAction::ToggleSidebar);
            }
            x += DIM_W + GROUP_GAP;
        }
        if self.room.is_some() {
            let text = tr("ui.room.copy_invite", "Copy invite link");
            let w = if label { 8.0 + 22.0 + 6.0 + self.invite_label_w(ui) + 8.0 } else { DIM_W };
            let r = Rect::from_min_size(pos2(x, y), vec2(w, BTN_H));
            let tip = if self.joined.is_none() { tr("ui.room.copy_invite_offline_tip", "Connect first: the link includes the relay's certificate") } else { text.clone() };
            let resp = dim_btn_resp(ui, r, "invite", "person_add", label.then_some(text.as_str()), &tip);
            if let Some(h) = self.joined {
                if resp.clicked() {
                    cx.act(UiAction::CopyInviteLink(h));
                } else if resp.secondary_clicked() {
                    let p = resp.interact_pointer_pos().unwrap_or(r.left_bottom());
                    cx.act(UiAction::OpenPopover(Popover { kind: PopoverKind::Bar(BarMenu::Invite), anchor: Rect::from_min_size(p, vec2(1.0, 1.0)) }));
                }
            }
        }
    }

    fn right(&self, ui: &mut Ui, cx: &mut Cx, st: &mut StageState, side: Rect) {
        let y = side.center().y - BTN_H / 2.0;
        let fs = Rect::from_min_size(pos2(side.right() - DIM_W, y), vec2(DIM_W, BTN_H));
        let pop = fs.translate(vec2(-(DIM_W + GROUP_GAP), 0.0));
        let (pop_icon, pop_tip) = if self.host == Host::Window {
            ("close_fullscreen", tr("ui.bar.unpop_stage", "Bring the stage back to the main window"))
        } else {
            ("open_in_new", tr("ui.bar.pop_stage", "Pop the stage out to its own window"))
        };
        if dim_btn(ui, pop, "pop", pop_icon, None, &pop_tip) {
            if self.popped {
                st.popped.remove(&self.view);
            } else {
                st.popped.insert(self.view.clone());
            }
        }
        let (fs_icon, fs_tip) = if self.fullscreen {
            ("fullscreen_exit", tr("ui.stage.exit_fullscreen", "Exit fullscreen"))
        } else {
            ("fullscreen", tr("ui.stage.fullscreen", "Fullscreen"))
        };
        if dim_btn(ui, fs, "fs", fs_icon, None, &fs_tip) {
            match self.host {
                Host::Main if self.fullscreen => cx.act(UiAction::ExitStageFullscreen),
                Host::Main => cx.act(UiAction::EnterStageFullscreen),
                Host::Window => ui.ctx().send_viewport_cmd(egui::ViewportCommand::Fullscreen(!self.fullscreen)),
            }
        }
    }

    fn mid(&self, ui: &mut Ui, cx: &mut Cx, st: &mut StageState, mid: Rect, leave_at: Option<Rect>) {
        let mut x = mid.left();
        let y = mid.top();
        // Group 1: Share screen, Share audio, Webcam.
        if let Some((room, state)) = self.share {
            let r = Rect::from_min_size(pos2(x, y), vec2(group_w(&[SPLIT_W; 3]), GROUP_H));
            group_frame(ui, r);
            self.share_buttons(ui, cx, r, room, state);
            x = r.right() + BAR_GAP;
        }
        // Group 2: Layout, Room settings, People.
        let parts: &[f32] = if self.room.is_some() { &[SPLIT_W, SPLIT_W, LEVEL_W, PEOPLE_W] } else { &[SPLIT_W, LEVEL_W, PEOPLE_W] };
        let r = Rect::from_min_size(pos2(x, y), vec2(group_w(parts), GROUP_H));
        group_frame(ui, r);
        let mut bx = r.left() + GROUP_PAD;
        let by = r.top() + GROUP_PAD;
        let open = |cx: &Cx, m: BarMenu| cx.popover_of(&PopoverKind::Bar(m)).is_some();
        let layout_tip = tr_with("ui.bar.layout_tip", "Layout: %{layout}", &[("layout", &layout_name(self.layout))]);
        let lr = Rect::from_min_size(pos2(bx, by), vec2(SPLIT_W, BTN_H));
        let (main, caret) = split(ui, lr, &Split { name: "layout", icon: layout_icon(self.layout), tone: Tone::Off, enabled: true, tip: &layout_tip, caret_tip: &tr("ui.bar.layout_caret", "Choose a layout"), open: open(cx, BarMenu::Layout) });
        if main || caret {
            cx.toggle_popover(PopoverKind::Bar(BarMenu::Layout), lr);
        }
        bx += SPLIT_W + GROUP_GAP;
        if self.room.is_some() {
            let rr = Rect::from_min_size(pos2(bx, by), vec2(SPLIT_W, BTN_H));
            let tip = tr("ui.bar.room_settings", "Room settings");
            let (main, caret) = split(ui, rr, &Split { name: "room", icon: "tune", tone: Tone::Off, enabled: true, tip: &tip, caret_tip: &tip, open: open(cx, BarMenu::Room) });
            if main || caret {
                cx.toggle_popover(PopoverKind::Bar(BarMenu::Room), rr);
            }
            bx += SPLIT_W + GROUP_GAP;
        }
        // Level loudness is a global viewer setting (`AppState::level_loudness`), so it shows wherever the people
        // button does, in a room and in Watched.
        let lvl_on = cx.state.level_loudness;
        let lvr = Rect::from_min_size(pos2(bx, by), vec2(LEVEL_W, BTN_H));
        let lvl_tip = format!(
            "{}\n{}",
            if lvl_on { tr("ui.bar.level_loudness_on", "Level loudness: on") } else { tr("ui.bar.level_loudness_off", "Level loudness: off") },
            tr("ui.settings.level_loudness_tip", "Raises quiet audio slowly and lowers loud audio quickly, and never lets a sudden loud sound blast past a safe level, so every stream sounds about equally loud at 100%. Your volume slider on each stream works on top of it. Applies right away.")
        );
        if group_btn(ui, lvr, "level", "graphic_eq", lvl_on, &lvl_tip) {
            cx.act(UiAction::SetLevelLoudness(!lvl_on));
        }
        bx += LEVEL_W + GROUP_GAP;
        let pr = Rect::from_min_size(pos2(bx, by), vec2(PEOPLE_W, BTN_H));
        let tip = if self.strip_on { tr("ui.bar.hide_people", "Hide the people") } else { tr("ui.bar.show_people", "Show the people") };
        if group_btn(ui, pr, "people", "group", self.strip_on, &tip) {
            // The strip is UI state of the stage, not shared (`StageState::strip_hidden`).
            st.strip_hidden = !st.strip_hidden;
        }
        // Leave, as tall as a group.
        if let Some(room) = &self.room {
            let lr = leave_at.unwrap_or_else(|| Rect::from_min_size(pos2(r.right() + BAR_GAP, y), vec2(LEAVE_W, GROUP_H)));
            let tip = tr_with("ui.bar.leave_room", "Leave %{room}", &[("room", &room.name)]);
            if leave_btn(ui, lr, &tip, room.conn != ConnState::Disconnected) {
                match (room.conn, room.handle) {
                    (ConnState::Joined, Some(h)) => cx.act(UiAction::Leave(h)),
                    (ConnState::Connecting | ConnState::Reconnecting, _) => cx.act(UiAction::CancelReconnect(room.key.clone())),
                    _ => {}
                }
            }
        }
    }

    /// Share screen, Share audio and Webcam. Share and audio act on `room` (the open room, or in "Watched" the
    /// room the share is in).
    fn share_buttons(&self, ui: &mut Ui, cx: &mut Cx, group: Rect, room: RoomHandle, state: ShareState) {
        let by = group.top() + GROUP_PAD;
        let mut bx = group.left() + GROUP_PAD;
        let open = |cx: &Cx, m: BarMenu| cx.popover_of(&PopoverKind::Bar(m)).is_some();
        // --- Share screen ---
        let (tone, tip, enabled) = match state {
            ShareState::Live { .. } => (Tone::On, tr("ui.bar.stream_settings", "Stream settings"), true),
            ShareState::Staged { .. } => (Tone::Ready, tr("ui.stream.title_staged", "Set up your stream"), true),
            ShareState::Picking { .. } => (Tone::Off, tr("ui.bar.picking_title", "Choose a screen in the system picker"), false),
            _ => (Tone::Off, tr("ui.bar.share_screen_tip", "Share your screen"), true),
        };
        drive_dump::line(|| {
            let name = match state {
                ShareState::Idle => "idle",
                ShareState::Picking { .. } => "picking",
                ShareState::Staged { .. } => "staged",
                ShareState::Live { .. } => "live",
                ShareState::Paused => "paused",
            };
            format!("share {name}")
        });
        let rect = Rect::from_min_size(pos2(bx, by), vec2(SPLIT_W, BTN_H));
        let (main, caret) = split(ui, rect, &Split { name: "share", icon: "screen_share", tone, enabled, tip: &tip, caret_tip: &tr("ui.bar.share_caret", "Framerate, source and preset"), open: open(cx, BarMenu::Share) });
        if main {
            match state {
                ShareState::Live { capture, .. } | ShareState::Staged { capture } => cx.act(UiAction::OpenModal(crate::ui::Modal::StreamSettings { capture, room })),
                ShareState::Idle if live_elsewhere(cx, room) => cx.toggle_popover(PopoverKind::Bar(BarMenu::Share), rect),
                ShareState::Idle => cx.act(UiAction::ShareNow(room)),
                _ => {}
            }
        } else if caret {
            cx.toggle_popover(PopoverKind::Bar(BarMenu::Share), rect);
        }
        bx += SPLIT_W + GROUP_GAP;
        // --- Share audio: a share of its own, no screen needed (task I7, D1). One click turns Desktop audio
        // on, the next turns it off; the arrow opens the source list and "All audio settings…". ---
        let cur = share::audio_current(cx, room);
        let (tone, tip) = match &cur {
            Some(name) => (Tone::On, tr_with("ui.bar.stop_audio_tip", "Stop sharing audio (%{source})", &[("source", name)])),
            None => (Tone::Off, tr("ui.bar.share_desktop_audio", "Share desktop audio")),
        };
        let rect = Rect::from_min_size(pos2(bx, by), vec2(SPLIT_W, BTN_H));
        let icon = if cur.is_some() { "music_note" } else { "music_off" };
        let (main, caret) = split(ui, rect, &Split { name: "audio", icon, tone, enabled: true, tip: &tip, caret_tip: &tr("ui.bar.audio_caret", "Pick an audio source"), open: open(cx, BarMenu::Audio) });
        if main {
            if cur.is_some() {
                cx.act(UiAction::StopSharingAudio { room });
            } else {
                cx.act(UiAction::ShareAudio { room, choice: AudioChoice::Desktop });
            }
        } else if caret {
            cx.toggle_popover(PopoverKind::Bar(BarMenu::Audio), rect);
        }
        bx += SPLIT_W + GROUP_GAP;
        // --- Webcam (task I8): its own share. One click starts or stops it; the arrow offers the camera and
        // "All webcam settings…". ---
        if !cx.state.webcams_requested {
            cx.act(UiAction::RefreshWebcams);
        }
        let rect = Rect::from_min_size(pos2(bx, by), vec2(SPLIT_W, BTN_H));
        let live_here = cx.webcam_share(room).is_some();
        let camera = cx.state.webcam_choice().map(|(c, _)| cx.state.webcams[c].name.clone()).unwrap_or_default();
        let with_camera = |key: &str, en: &'static str| tr_with(key, en, &[("camera", &camera)]);
        let (tone, icon, tip, enabled) = match webcam::unavailable() {
            Some(webcam::Unavailable::Platform) => (Tone::Off, "videocam_off", tr("ui.bar.webcam_platform", "Webcam sharing is not available on this system yet"), false),
            Some(webcam::Unavailable::NoPlugin) => (Tone::Off, "videocam_off", tr("ui.bar.webcam_no_plugin", "Webcam sharing needs GStreamer's video4linux2 plugin (v4l2src), which is not installed"), false),
            None if live_here => (Tone::On, "videocam", with_camera("ui.bar.stop_webcam_tip", "Stop sharing your webcam (%{camera})"), true),
            None if cx.state.webcams.is_empty() => {
                let looking = !cx.state.webcams_requested || cx.state.webcams_listing();
                (Tone::Off, "videocam_off", if looking { tr("ui.bar.webcam_looking", "Looking for cameras…") } else { tr("ui.bar.webcam_none", "No camera found") }, false)
            }
            None if cx.state.webcam_share().is_some() => (Tone::Off, "videocam_off", with_camera("ui.bar.move_webcam_tip", "Move your webcam here (%{camera})"), true),
            None => (Tone::Off, "videocam_off", with_camera("ui.bar.share_webcam_tip", "Share your webcam (%{camera})"), true),
        };
        let (main, caret) = split(ui, rect, &Split { name: "webcam", icon, tone, enabled, tip: &tip, caret_tip: &tr("ui.bar.webcam_caret", "Pick a camera"), open: open(cx, BarMenu::Webcam) });
        if main {
            cx.act(if live_here { UiAction::StopWebcam } else { UiAction::StartWebcam(room) });
        } else if caret {
            if cx.popover_of(&PopoverKind::Bar(BarMenu::Webcam)).is_none() {
                cx.act(UiAction::RefreshWebcams);
            }
            cx.toggle_popover(PopoverKind::Bar(BarMenu::Webcam), rect);
        }
    }
}

/// The layout the Layout button shows.
fn layout_icon(l: StageLayout) -> &'static str {
    match l {
        StageLayout::Split => "grid_view",
        StageLayout::Crop => "crop",
        StageLayout::Floating => "filter_none",
    }
}

pub(super) fn layout_name(l: StageLayout) -> String {
    match l {
        StageLayout::Split => tr("ui.stage.layout_split", "Split"),
        StageLayout::Crop => tr("ui.stage.layout_crop", "Crop"),
        StageLayout::Floating => tr("ui.stage.layout_floating", "Floating"),
    }
}

/// `.cgroup`: the pill behind grouped buttons.
fn group_frame(ui: &Ui, r: Rect) {
    drive_dump::rect(|| format!("ctl.group.{:.0}", r.min.x), r);
    ui.painter().rect(r, CornerRadius::same(12), theme::STAGE_GROUP, Stroke::new(1.0_f32, theme::STAGE_GROUP_BORDER), StrokeKind::Inside);
}

#[derive(Clone, Copy, PartialEq)]
enum Tone {
    /// The backdrop colour.
    Off,
    /// Live or switched on: green.
    On,
    /// Staged, ready to go live: accent.
    Ready,
}

struct Split<'a> {
    name: &'a str,
    icon: &'a str,
    tone: Tone,
    enabled: bool,
    tip: &'a str,
    caret_tip: &'a str,
    open: bool,
}

/// A split button: `[icon | arrow]`, 68 x 40. Returns `(button clicked, arrow clicked)`. The arrow is always
/// there, cut off by a separator line; disabled buttons are dimmed and click nothing.
fn split(ui: &mut Ui, rect: Rect, s: &Split) -> (bool, bool) {
    let rect = rect.round_to_pixels(ui.pixels_per_point());
    let main = Rect::from_min_max(rect.min, pos2(rect.right() - CARET_W, rect.bottom()));
    let caret = Rect::from_min_max(pos2(rect.right() - CARET_W, rect.top()), rect.max);
    let sense = if s.enabled { Sense::click() } else { Sense::hover() };
    let id = Id::new(("bar_split", s.name));
    drive_dump::rect(|| format!("ctl.{}", s.name), main);
    drive_dump::rect(|| format!("ctl.{}.arrow", s.name), caret);
    let (rm, rc) = (ui.interact(main, id.with("main"), sense), ui.interact(caret, id.with("caret"), sense));
    let alpha = if s.enabled { 1.0 } else { DISABLED_OPACITY };
    let (fill, line, ink, wash) = match s.tone {
        Tone::Off => (theme::STAGE_BTN, theme::STAGE_BTN_SEP, theme::TEXT, theme::STAGE_BTN_HOVER),
        Tone::On => (theme::PILL_LIVE_BG, theme::PILL_LIVE_BORDER, theme::SUCCESS, theme::PILL_LIVE_BG),
        Tone::Ready => (theme::ACCENT_CHIP_BG, theme::ACCENT_CHIP_BORDER, theme::ACCENT_LIGHTER, theme::ACCENT_CHIP_BG),
    };
    let p = ui.painter();
    let r9 = CornerRadius::same(9);
    p.rect_filled(rect, r9, fade(fill, alpha));
    if s.tone != Tone::Off {
        p.rect_stroke(rect, r9, Stroke::new(1.0_f32, fade(line, alpha)), StrokeKind::Inside);
    }
    if s.enabled {
        if rm.hovered() {
            p.rect_filled(main, CornerRadius { nw: 9, sw: 9, ne: 0, se: 0 }, wash);
            if s.tone != Tone::Off {
                p.rect_filled(main, CornerRadius { nw: 9, sw: 9, ne: 0, se: 0 }, Color32::from_white_alpha(10));
            }
        }
        if rc.hovered() {
            p.rect_filled(caret, CornerRadius { nw: 0, sw: 0, ne: 9, se: 9 }, wash);
            if s.tone != Tone::Off {
                p.rect_filled(caret, CornerRadius { nw: 0, sw: 0, ne: 9, se: 9 }, Color32::from_white_alpha(10));
            }
        }
    }
    p.vline(caret.left() + 0.5, caret.y_range(), Stroke::new(1.0_f32, fade(line, alpha)));
    paint_icon(ui, main, s.icon, 22.0, fade(ink, alpha));
    paint_icon(ui, caret, if s.open { "expand_less" } else { "expand_more" }, 18.0, fade(theme::TEXT_DIM, alpha));
    let hand = |r: egui::Response, tip: &str| if s.enabled { r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip.to_owned()) } else { r.on_hover_text(tip.to_owned()) };
    let (m, c) = (hand(rm, s.tip), hand(rc, s.caret_tip));
    (s.enabled && m.clicked(), s.enabled && c.clicked())
}

fn fade(c: Color32, alpha: f32) -> Color32 {
    if alpha >= 1.0 { c } else { c.gamma_multiply(alpha) }
}

/// A freestanding bar button (no backdrop, dim icon): hide sidebar, Copy invite link, pop out, fullscreen.
/// With `label` it is `[icon][label]`, 8 px padding and 6 between.
fn dim_btn(ui: &mut Ui, rect: Rect, id: &str, icon: &str, label: Option<&str>, tip: &str) -> bool {
    dim_btn_resp(ui, rect, id, icon, label, tip).clicked()
}

/// [], returning the response (for a right click as well).
fn dim_btn_resp(ui: &mut Ui, rect: Rect, id: &str, icon: &str, label: Option<&str>, tip: &str) -> egui::Response {
    drive_dump::rect(|| format!("ctl.{id}"), rect);
    let resp = ui.interact(rect, Id::new(("bar_dim", id)), Sense::click());
    let hov = resp.hovered();
    let col = if hov { theme::TEXT } else { theme::TEXT_DIM };
    if hov {
        ui.painter().rect_filled(rect, CornerRadius::same(9), theme::STAGE_BTN_HOVER);
    }
    match label {
        None => paint_icon(ui, rect, icon, 22.0, col),
        Some(text) => {
            paint_icon(ui, Rect::from_min_size(pos2(rect.left() + 8.0, rect.top()), vec2(22.0, rect.height())), icon, 22.0, col);
            paint_text(ui, Rect::from_x_y_ranges(rect.left() + 8.0 + 22.0 + 6.0..=rect.right() - 8.0, rect.y_range()), text, font(Weight::Bold, theme::FS_SMALL), col, Align::Min);
        }
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip.to_owned())
}

/// A single button in a group (the people button): backdrop, accent when `on`.
fn group_btn(ui: &mut Ui, rect: Rect, id: &str, icon: &str, on: bool, tip: &str) -> bool {
    drive_dump::rect(|| format!("ctl.{id}"), rect);
    let resp = ui.interact(rect, Id::new(("bar_group_btn", id)), Sense::click());
    let (fill, ink) = if on { (theme::ACCENT_CHIP_BG, theme::ACCENT_LIGHTER) } else if resp.hovered() { (theme::STAGE_BTN_HOVER, theme::TEXT) } else { (theme::STAGE_BTN, theme::TEXT) };
    ui.painter().rect_filled(rect, CornerRadius::same(9), fill);
    paint_icon(ui, rect, icon, 22.0, ink);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(tip.to_owned()).clicked()
}

/// Leave: a red 64 x 50 button (a group's height), `call_end`.
fn leave_btn(ui: &mut Ui, rect: Rect, tip: &str, enabled: bool) -> bool {
    drive_dump::rect(|| "ctl.leave".into(), rect);
    let resp = ui.interact(rect, Id::new("bar_leave"), if enabled { Sense::click() } else { Sense::hover() });
    let fill = if resp.hovered() && enabled { widgets::lerp_color(theme::DANGER, Color32::WHITE, 0.12) } else { theme::DANGER };
    let a = if enabled { 1.0 } else { DISABLED_OPACITY };
    ui.painter().rect_filled(rect, CornerRadius::same(12), fade(fill, a));
    paint_icon(ui, rect, "call_end", 22.0, fade(theme::PRIMARY_TEXT, a));
    let r = if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp };
    r.on_hover_text(tip.to_owned()).clicked() && enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sizes() -> Sizes {
        Sizes { leave: LEAVE_W + BAR_GAP, mid: group_w(&[SPLIT_W; 3]) + group_w(&[SPLIT_W, SPLIT_W, LEVEL_W, PEOPLE_W]) + LEAVE_W + 2.0 * BAR_GAP, left_icons: 2.0 * DIM_W + GROUP_GAP, left_label: 2.0 * DIM_W + GROUP_GAP + 6.0 + 110.0, right: 2.0 * DIM_W + GROUP_GAP }
    }

    #[test]
    fn the_label_goes_first_then_the_bar_takes_two_rows() {
        let s = sizes();
        let wide = arrange(1400.0, &s);
        assert!(!wide.two_rows && wide.label);
        // Just under what the label needs: the label goes, one row stays.
        let need_label = 2.0 * s.left_label + s.mid + 2.0 * BAR_GAP + 2.0 * theme::S4;
        let a = arrange(need_label - 1.0, &s);
        assert!(!a.two_rows && !a.label, "label dropped before the bar wraps");
        // The 800 px window's 528 px stage: the middle part alone, two rows, and it still fits.
        let narrow = arrange(528.0, &s);
        assert!(narrow.two_rows && !narrow.label);
        assert!(narrow.leave_down, "with Level loudness the middle part alone is too wide ({}), Leave goes under", s.mid);
        assert!(s.mid - s.leave + 2.0 * narrow.pad <= 528.0, "the rest fits the 800 px window: {} + 2 x {}", s.mid - s.leave, narrow.pad);
        assert!(!wide.leave_down);
        assert_eq!(narrow.height(), BAR_H_TWO);
        assert_eq!(wide.height(), BAR_H);
    }

    #[test]
    fn leave_is_as_tall_as_a_group() {
        assert_eq!(GROUP_H, 50.0);
        assert_eq!(group_w(&[SPLIT_W; 3]), 3.0 * 68.0 + 2.0 * 4.0 + 10.0);
    }
}
