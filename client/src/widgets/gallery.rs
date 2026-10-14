//! `simple-screensharing-client --gallery`: every widget helper with realistic fake
//! data, at a narrow (272 px sidebar / 232 px people panel) and a wide width, so slot
//! alignment and row heights can be checked by eye. "Show slots" turns on the debug
//! overlay (hbox amber, fixed slot green, fill grey, vbox blue).
//!
//! For scripted screenshots (no input injection needed), env vars preset the state:
//! `SSS_GALLERY_SLOTS=1` (overlay on), `SSS_GALLERY_MODAL=1` (stream-settings modal
//! open), `SSS_GALLERY_SCROLL=<points>` (initial scroll offset of the wide column).

use egui::{Align, Color32, CornerRadius, Frame, Margin, Rect, Stroke, Ui, vec2};

use super::*;
use crate::theme::{self, Weight, font};

/// Open the gallery window.
pub fn run(native_options: eframe::NativeOptions) -> eframe::Result {
    eframe::run_native(
        "Simple Screensharing — widget gallery",
        native_options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(Gallery::default()))
        }),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BwMode {
    PerStream,
    Absolute,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Codec {
    H264,
    Av1,
    H265,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Preset {
    LowLatency,
    Balanced,
    Quality,
    Gaming,
}

/// Gallery state (fake data only).
pub struct Gallery {
    debug: bool,
    auto_connect: bool,
    mute: bool,
    disabled_check: bool,
    radio: u8,
    bw_mode: BwMode,
    total: u32,
    floor: u32,
    gop: u32,
    fps_idx: usize,
    audio_idx: usize,
    codec: Codec,
    preset: Preset,
    adv_open: bool,
    name: String,
    modal_open: bool,
    last_click: String,
    initial_scroll: Option<f32>,
}

impl Default for Gallery {
    fn default() -> Self {
        let env = |k: &str| std::env::var(k).ok();
        Self {
            debug: env("SSS_GALLERY_SLOTS").is_some_and(|v| v == "1"),
            auto_connect: true,
            mute: false,
            disabled_check: true,
            radio: 1,
            bw_mode: BwMode::Absolute,
            total: 12000,
            floor: 3000,
            gop: 1000,
            fps_idx: 2,
            audio_idx: 1,
            codec: Codec::H264,
            preset: Preset::Balanced,
            adv_open: true,
            name: String::new(),
            modal_open: env("SSS_GALLERY_MODAL").is_some_and(|v| v == "1"),
            last_click: "—".into(),
            initial_scroll: env("SSS_GALLERY_SCROLL").and_then(|v| v.parse().ok()),
        }
    }
}

const FPS: [&str; 5] = ["30 fps", "48 fps", "60 fps", "120 fps", "240 fps"];
const AUDIO: [&str; 4] = ["No audio", "Desktop audio", "Spotify", "Firefox"];

impl eframe::App for Gallery {
    /// The gallery is a QC tool: every launch starts from the same state (no saved
    /// scroll offsets or window geometry under `~/.local/share`).
    fn persist_egui_memory(&self) -> bool {
        false
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        set_debug_slots(ctx, self.debug);

        egui::TopBottomPanel::top("gallery_top").exact_height(theme::BAND_H).frame(Frame::new().fill(theme::SIDEBAR_RAIL)).show(
            ctx,
            |ui| {
                band(ui, theme::SIDEBAR_RAIL, BandEdge::Bottom, theme::S4, |ui| {
                    let slots = [FILL, fixed(160.0), fixed(150.0)];
                    let r = HBox::new("gallery_band", theme::CTL_H).slots(&slots).show(ui);
                    r.text(ui, 0, "Widget gallery  ·  theme + widgets (I8)", font(Weight::Bold, theme::FS_H3), theme::TEXT, Align::Min);
                    r.show(ui, 1, Align::Min, |ui| checkbox(ui, &mut self.debug, "Show slots"));
                    r.show(ui, 2, Align::Max, |ui| {
                        if Btn::secondary("Open modal").icon("open_in_new").show(ui).clicked() {
                            self.modal_open = true;
                        }
                    });
                });
            },
        );

        // Narrow column: a sidebar replica at exactly SIDEBAR_W.
        egui::SidePanel::left("gallery_side")
            .exact_width(theme::SIDEBAR_W)
            .resizable(false)
            .frame(Frame::new().fill(theme::SIDEBAR_RAIL).stroke(Stroke::new(1.0_f32, theme::PANEL_RAISED)))
            .show(ctx, |ui| self.sidebar(ui));

        // People panel at PEOPLE_W.
        egui::SidePanel::right("gallery_people")
            .exact_width(theme::PEOPLE_W)
            .resizable(false)
            .frame(Frame::new().fill(theme::PANEL_SUNKEN).inner_margin(Margin::symmetric(theme::S3 as i8, theme::S4 as i8)))
            .show(ctx, |ui| self.people(ui));

        egui::CentralPanel::default().frame(Frame::new().fill(theme::PANEL_BASE)).show(ctx, |ui| {
            let mut sa = egui::ScrollArea::vertical().auto_shrink([false, false]);
            if let Some(y) = self.initial_scroll.take() {
                sa = sa.vertical_scroll_offset(y);
            }
            sa.show(ui, |ui| {
                Frame::new().inner_margin(Margin::same(theme::S4 as i8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.spacing_mut().item_spacing.y = theme::S4;
                    self.wide(ui);
                });
            });
        });

        if self.modal_open {
            let spec = ModalSpec {
                id: "gallery_modal",
                title: "Stream settings",
                sub: Some("Live in Movie Night · changes apply right away"),
                width: theme::MODAL_W_XWIDE,
                head_pill: Some(Pill::progress("Applying").large()),
            };
            let mut click = FooterClick::None;
            let a = modal_shell(
                ctx,
                &spec,
                |ui| self.settings_columns(ui),
                |ui| {
                    click = modal_footer(
                        ui,
                        &[Btn::danger("Stop sharing in Movie Night").icon("stop_screen_share")],
                        Some("Saved"),
                        &[Btn::primary("Done")],
                    );
                },
            );
            if click != FooterClick::None {
                self.last_click = format!("{click:?}");
            }
            if a.close || click == FooterClick::Trailing(0) {
                self.modal_open = false;
            }
        }
    }
}

impl Gallery {
    fn sidebar(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing.y = 0.0;
        band(ui, theme::SIDEBAR_RAIL, BandEdge::None, theme::S4, |ui| {
            let r = HBox::new("brand", 22.0).slots(&[fixed(30.0), FILL]).show(ui);
            paint_brand_mark(ui, r.cell(0), theme::SIDEBAR_RAIL);
            r.text(ui, 1, "Simple Screensharing", font(Weight::Bold, theme::FS_H3), theme::TEXT, Align::Min);
        });
        let user_h = theme::BAND_H;
        let avail = ui.available_height() - user_h;
        egui::ScrollArea::vertical().id_salt("side_scroll").max_height(avail).auto_shrink([false, false]).show(ui, |ui| {
            Frame::new().inner_margin(Margin { left: 12, right: 12, top: 0, bottom: 12 }).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = theme::S2;
                well(ui, |ui| {
                    room_row(
                        ui,
                        &RoomRow {
                            lead: RoomLead::Icon("grid_view"),
                            name: "Watched streams",
                            live: LiveSlot::Absent,
                            num: Some(2),
                            unit: Unit::Icon("visibility"),
                            selected: false,
                        },
                    );
                });
                ui.add_space(theme::S1 - theme::S2 + theme::S1);
                heading(ui, "Rooms", Some("5"), true);
                well(ui, |ui| self.room_list(ui));
                if btn_row(ui, &[Btn::ghost("Create or join a room").icon("add")]).is_some() {
                    self.last_click = "Create or join".into();
                }
                heading(ui, "Narrow card", None, true);
                card(ui, "Video", None, |ui| self.video_rows(ui));
                card(ui, "Sending to", Some("2 rooms"), |ui| self.dest_rows(ui));
            });
        });
        band(ui, theme::ELEV_14, BandEdge::Top, theme::S3, |ui| {
            let r = HBox::new("user_band", theme::AVATAR).slots(&[fixed(theme::AVATAR), FILL, fixed(theme::CTL_H)]).show(ui);
            paint_avatar(ui, r.cell(0), "m", 0, true);
            let c = r.cell(1);
            let top = Rect::from_x_y_ranges(c.x_range(), c.center().y - 15.0..=c.center().y + 1.0);
            let bot = Rect::from_x_y_ranges(c.x_range(), c.center().y + 1.0..=c.center().y + 15.0);
            paint_text(ui, top, "mo", font(Weight::Bold, theme::FS_ROW), theme::TEXT, Align::Min);
            paint_text(ui, bot, "Live in 2 rooms", font(Weight::Regular, theme::FS_TINY), theme::SUCCESS, Align::Min);
            r.show(ui, 2, Align::Center, |ui| icon_btn(ui, "settings", IconBtnKind::Plain, "Settings"));
        });
    }

    fn room_list(&mut self, ui: &mut Ui) {
        let rooms: [(&str, DotState, bool, Option<u32>, bool); 5] = [
            ("Movie Night", DotState::Joined, true, Some(3), true),
            ("Gaming Night", DotState::Joined, false, Some(2), false),
            ("Book Club with a really long room name", DotState::Joined, true, Some(12), false),
            ("Study Group", DotState::Reconnecting, false, None, false),
            ("Old Room", DotState::Disconnected, false, None, false),
        ];
        for (i, (name, st, live, num, sel)) in rooms.iter().enumerate() {
            let unit = if num.is_some() { Unit::Icon("screen_share") } else { Unit::None };
            let live = if *live { LiveSlot::On } else { LiveSlot::Off };
            if room_row(ui, &RoomRow { lead: RoomLead::Dot(*st), name, live, num: *num, unit, selected: *sel }).clicked() {
                self.last_click = format!("room {name}");
            }
            let sharers: &[(&str, IconTone, &str, u32)] = match i {
                0 => &[
                    ("cast", IconTone::Live, "You", 60),
                    ("visibility", IconTone::Watch, "Ana", 60),
                    ("screen_share", IconTone::Dim, "Jonas", 144),
                ],
                1 => &[("visibility", IconTone::Watch, "Ben", 240), ("screen_share", IconTone::Dim, "Cleo", 144)],
                2 => &[("cast", IconTone::Live, "You", 30), ("screen_share", IconTone::Dim, "Lea with an overly long display name", 30)],
                _ => &[],
            };
            for (icon, tone, who, fps) in sharers {
                sharer_row(ui, &SharerRow { icon, tone: *tone, name: who, num: Some(*fps), unit: "fps" });
            }
        }
    }

    fn people(&mut self, ui: &mut Ui) {
        ui.spacing_mut().item_spacing.y = theme::S3;
        heading(ui, "People", Some("4"), false);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            let people = [
                PersonRow { name: "mo", color: 0, you: Some("you"), live: true, live_label: "Live", watching: false, idle: false, stale: false },
                PersonRow { name: "Ana", color: 1, you: None, live: true, live_label: "Live", watching: true, idle: false, stale: false },
                PersonRow { name: "Jonas", color: 3, you: None, live: true, live_label: "Live", watching: false, idle: false, stale: false },
                PersonRow { name: "Mia", color: 4, you: None, live: false, live_label: "Live", watching: false, idle: true, stale: false },
                PersonRow { name: "Maximilian-Alexander", color: 2, you: None, live: true, live_label: "Live", watching: true, idle: false, stale: false },
            ];
            for p in &people {
                person_row(ui, p);
            }
        });
        heading(ui, "People (last known)", Some("2"), false);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            person_row(ui, &PersonRow { name: "Noah", color: 2, you: None, live: true, live_label: "Live", watching: false, idle: false, stale: true });
            person_row(ui, &PersonRow { name: "Ivy", color: 3, you: None, live: false, live_label: "Live", watching: false, idle: true, stale: true });
        });
        heading(ui, "Popup", None, false);
        popup_frame(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            paint_popup_heading(ui, "Audio source");
            pop_item(ui, &PopItem::new("volume_off", "No audio").selected(false));
            pop_item(ui, &PopItem::new("speaker", "Desktop audio").sub("Everything you hear").selected(true).meter(3));
            pop_item(ui, &PopItem::new("music_note", "Spotify").sub("App audio").meter(5));
            pop_item(ui, &PopItem::new("mic", "Microphone").enabled(false));
            pop_item(ui, &PopItem::new("stop_screen_share", "Stop sharing").danger());
        });
    }

    fn video_rows(&mut self, ui: &mut Ui) {
        ctrl_row(ui, "Framerate", None, |ui| select(ui, "fps", &mut self.fps_idx, &FPS));
        ctrl_row_value(
            ui,
            "Bitrate",
            Some("Absolute maximum mode: the upload budget splits the bitrate across all your streams."),
            |ui| readout(ui, "Set by upload budget", Align::Max),
            |ui| value_box(ui, "6000"),
        );
        ctrl_row(ui, "Codec", None, |ui| {
            segmented(ui, &mut self.codec, &[(Codec::H264, "H.264"), (Codec::Av1, "AV1"), (Codec::H265, "H.265")])
        });
    }

    fn dest_rows(&mut self, ui: &mut Ui) {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = theme::S1;
            let rows = [
                DestRow { current: true, name: "Movie Night", note: "this room", remove_tip: "Stop sending", pill: Pill::live("Live"), kbps: "6000 kbps", removable: true },
                DestRow { current: false, name: "Book Club", note: "this room", remove_tip: "Stop sending", pill: Pill::live("Live"), kbps: "6000 kbps", removable: true },
                DestRow { current: false, name: "Gaming Night", note: "this room", remove_tip: "Stop sending", pill: Pill::progress("Starting"), kbps: "12000 kbps", removable: true },
                DestRow { current: false, name: "Study Group (staged)", note: "this room", remove_tip: "Stop sending", pill: Pill::muted("Not live"), kbps: "—", removable: false },
            ];
            for d in &rows {
                if dest_row(ui, d) == DestAction::Remove {
                    self.last_click = format!("remove {}", d.name);
                }
            }
        });
        if let Some(i) = btn_row(ui, &[Btn::secondary("Also send to…").icon("add"), Btn::secondary("Move to…").icon("swap_horiz")]) {
            self.last_click = format!("dest btn {i}");
        }
    }

    fn settings_columns(&mut self, ui: &mut Ui) {
        columns(ui, 2, theme::S3, |c| {
            let (l, r) = c.split_at_mut(1);
            let l = &mut l[0];
            let r = &mut r[0];
            l.spacing_mut().item_spacing.y = theme::S3;
            r.spacing_mut().item_spacing.y = theme::S3;
            card(l, "Source", None, |ui| {
                ctrl_row_value(
                    ui,
                    "Screen",
                    None,
                    |ui| readout(ui, "Display 1 · 1920×1080", Align::Min),
                    |ui| Btn::secondary("Change").show_sized(ui, theme::VALUE_W),
                );
                ctrl_row_value(
                    ui,
                    "Audio",
                    None,
                    |ui| select(ui, "audio", &mut self.audio_idx, &AUDIO),
                    |ui| value_box_with(ui, |ui| meter(ui, 3)),
                );
                ctrl_row(ui, "", None, |ui| checkbox(ui, &mut self.mute, "Mute shared audio"));
            });
            card(l, "Sending to", Some("2 rooms"), |ui| self.dest_rows(ui));
            card(l, "Upload bandwidth", Some("all your streams"), |ui| {
                ctrl_row(ui, "Mode", None, |ui| {
                    segmented(ui, &mut self.bw_mode, &[(BwMode::PerStream, "Per-stream maximum"), (BwMode::Absolute, "Absolute maximum")])
                });
                slider_row(ui, "Total maximum", Some("kbps across every stream you send"), &mut self.total, 2000..=40000, 500.0, 0, "");
                slider_row(ui, "Floor per stream", Some("kbps no stream drops below"), &mut self.floor, 500..=10000, 250.0, 0, "");
                let n = 2;
                if self.floor * n > self.total {
                    banner(
                        ui,
                        BannerKind::Hatched,
                        "warning",
                        "Over budget: 2 streams × floor is more than the total. No stream drops below its floor.",
                        None,
                    );
                } else {
                    let per = (self.total / n).max(self.floor).to_string();
                    ctrl_row_value(
                        ui,
                        "Per stream",
                        None,
                        |ui| readout(ui, "2 streams share the budget", Align::Max),
                        |ui| value_box(ui, &per),
                    );
                }
            });
            card(r, "Video", None, |ui| self.video_rows(ui));
            card(r, "Encoder", None, |ui| {
                ctrl_row(ui, "Preset", None, |ui| {
                    segmented(
                        ui,
                        &mut self.preset,
                        &[
                            (Preset::LowLatency, "Low-Latency"),
                            (Preset::Balanced, "Balanced"),
                            (Preset::Quality, "Quality"),
                            (Preset::Gaming, "Gaming"),
                        ],
                    )
                });
                collapser(ui, &mut self.adv_open, "Advanced tuning");
                if self.adv_open {
                    ctrl_row(ui, "Tune", None, |ui| segmented(ui, &mut self.radio, &[(0, "Low latency"), (1, "Best quality")]));
                    slider_row(ui, "Keyframe interval", Some("GOP length."), &mut self.gop, 250..=4000, 250.0, 0, " ms");
                }
            });
        });
    }

    fn wide(&mut self, ui: &mut Ui) {
        heading(ui, "Buttons", Some("32 / 36 px"), true);
        for large in [false, true] {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = theme::S2;
                let l = |b: Btn<'static>| if large { b.large() } else { b };
                l(Btn::primary("Watch").icon("play_arrow")).show(ui);
                l(Btn::secondary("Copy invite link").icon("link")).show(ui);
                l(Btn::danger("Leave").icon("logout")).show(ui);
                l(Btn::danger_solid("Stop sharing").icon("stop_screen_share")).show(ui);
                l(Btn::secondary("2 rooms").icon("cast").trailing("expand_more")).show(ui);
                l(Btn::ghost("Add a room").icon("add")).show(ui);
                l(Btn::primary("Go live").icon("cast").enabled(false)).show(ui);
                l(Btn::secondary("Disabled").enabled(false)).show(ui);
            });
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = theme::S2;
            icon_btn(ui, "close", IconBtnKind::Plain, "Close");
            icon_btn(ui, "settings", IconBtnKind::Plain, "Settings");
            icon_btn(ui, "more_horiz", IconBtnKind::Bordered, "Room options");
            icon_btn(ui, "close", IconBtnKind::Small, "Stop sending");
            vsep(ui);
            checkbox(ui, &mut self.auto_connect, "Auto-connect");
            checkbox_enabled(ui, &mut self.disabled_check, "Disabled", false);
            radio(ui, &mut self.radio, 0, "Low latency");
            radio(ui, &mut self.radio, 1, "Best quality");
            ui.label(egui::RichText::new(format!("last click: {}", self.last_click)).color(theme::TEXT_FAINT));
        });

        heading(ui, "Pills · dots · avatars · meters · chips", None, true);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = theme::S2;
            Pill::live("Connected").show(ui);
            Pill::live("Live").lead(PillLead::None).show(ui);
            Pill::progress("Starting").show(ui);
            Pill::degraded("Reconnecting").show(ui);
            Pill::muted("Not connected").show(ui);
            Pill::live("Live").large().show(ui);
            Pill::progress("Applying").large().show(ui);
            Pill::muted("Paused").lead(PillLead::Icon("pause")).large().show(ui);
            vsep(ui);
            status_dot(ui, DotState::Joined);
            status_dot(ui, DotState::Reconnecting);
            status_dot(ui, DotState::Disconnected);
            vsep(ui);
            for (i, n) in ["M", "A", "C", "J", "B"].iter().enumerate() {
                avatar(ui, n, theme::AVATAR_SM, i, false);
            }
            avatar(ui, "M", theme::AVATAR, 0, true);
            vsep(ui);
            for lit in [0, 2, 4, 5] {
                meter(ui, lit);
            }
        });
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = theme::S2;
            ui.scope(|ui| {
                // Chips are meant for video; show them over a tile-coloured strip.
                let (r, _) = ui.allocate_exact_size(vec2(260.0, 30.0), egui::Sense::hover());
                paint_hatch(ui.painter(), r, CornerRadius::same(6), 8.0, theme::ELEV_19, theme::ELEV_14);
                let mut c = ui.new_child(egui::UiBuilder::new().max_rect(r.shrink(4.0)).layout(egui::Layout::left_to_right(Align::Center)));
                chip_dark(&mut c, None, true, "LIVE");
                chip_dark(&mut c, Some("volume_up"), false, "");
                chip_dark(&mut c, Some("volume_off"), false, "No audio shared");
            });
        });

        heading(ui, "Banners", None, true);
        if banner(
            ui,
            BannerKind::Hatched,
            "sync_problem",
            "Connection lost. Reconnecting, attempt 3. Streams in this room resume on their own.",
            Some("Retry now"),
        ) {
            self.last_click = "retry".into();
        }
        banner(
            ui,
            BannerKind::Warn,
            "warning",
            "Over budget: 3 streams × 3000 kbps floor is more than 8000 kbps. No stream drops below its floor.",
            None,
        );
        banner(ui, BannerKind::Info, "info", "Your screen is live in Movie Night and Book Club.", None);

        heading(ui, "Stream settings cards (theme::columns 2)", None, true);
        self.settings_columns(ui);

        heading(ui, "Sidebar rows at a wide width (same helpers as the left column)", None, true);
        well(ui, |ui| self.room_list(ui));

        heading(ui, "Form controls", None, true);
        card(ui, "Fields", None, |ui| {
            ctrl_row(ui, "Room name", Some("Shown to everyone in the room"), |ui| text_field(ui, &mut self.name, "e.g. Movie Night"));
            ctrl_row(ui, "Language", None, |ui| select(ui, "lang", &mut self.fps_idx, &FPS));
            ctrl_row_value(
                ui,
                "Long label that truncates",
                None,
                |ui| readout(ui, "readout that is far too long for its slot and must be cut", Align::Min),
                |ui| value_box(ui, "40000"),
            );
        });

        heading(ui, "Card grid (grid_rows: cols = clamp(floor((w+12)/272), 1, 4))", Some("5 cards"), true);
        let shares = [
            ("Ana", "Firefox: Dune Part Two", "1920×1080 · 60 fps · H.264", 1usize),
            ("Jonas", "Blender viewport", "2560×1440 · 144 fps · AV1", 3),
            ("Ben", "Elden Ring", "2560×1440 · 240 fps · H.265", 4),
            ("Cleo", "Rocket League", "1920×1080 · 144 fps · AV1", 2),
            ("Lea", "Slides: Chapter 7 — a title long enough to be truncated", "1280×720 · 30 fps · H.264", 2),
        ];
        grid_rows(ui, "share_grid", shares.len(), |ui, i| {
            let (who, title, specs, color) = shares[i];
            share_card(ui, who, title, specs, color);
        });

        heading(ui, "Modal frame (inline, 640)", None, true);
        let spec = ModalSpec {
            id: "inline_modal",
            title: "Leave Movie Night?",
            sub: Some("You stop watching and sharing in this room"),
            width: theme::MODAL_W_WIDE,
            head_pill: None,
        };
        modal_frame(
            ui,
            &spec,
            420.0,
            |ui| {
                banner(ui, BannerKind::Hatched, "warning", "Your screen is still live here. Leaving stops it in this room only.", None);
                ctrl_row(ui, "", None, |ui| checkbox(ui, &mut self.auto_connect, "Keep it in my room list"));
            },
            |ui| {
                modal_footer(ui, &[], None, &[Btn::secondary("Cancel"), Btn::danger_solid("Leave room").icon("logout")]);
            },
        );
    }
}

/// A popup surface (`.popup`, 340 wide max) — demo only; real popups use egui popups.
fn popup_frame(ui: &mut Ui, body: impl FnOnce(&mut Ui)) {
    Frame::new()
        .fill(theme::PANEL_RAISED)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(CornerRadius::same(10))
        .inner_margin(Margin::same(6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        });
}

fn paint_popup_heading(ui: &mut Ui, title: &str) {
    let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 24.0), egui::Sense::hover());
    let g = text_galley(ui, &title.to_uppercase(), font(Weight::Bold, theme::FS_SECTION), theme::ACCENT, r.width() - 16.0, 1.1);
    paint_galley(ui, r.shrink2(vec2(theme::S2, 0.0)), g, Align::Min);
}

/// Demo share card (`.scard`): thumb, who/title, specs, two equal buttons.
fn share_card(ui: &mut Ui, who: &str, title: &str, specs: &str, color: usize) {
    Frame::new()
        .fill(theme::PANEL_RAISED)
        .stroke(Stroke::new(1.0_f32, theme::BORDER))
        .corner_radius(CornerRadius::same(theme::ROUNDING_WELL))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 0.0;
            let w = ui.available_width();
            let (thumb, _) = ui.allocate_exact_size(vec2(w, w * 9.0 / 16.0), egui::Sense::hover());
            let r = CornerRadius { nw: theme::ROUNDING_WELL - 1, ne: theme::ROUNDING_WELL - 1, sw: 0, se: 0 };
            paint_gradient(ui.painter(), thumb, r, 160.0, Color32::from_rgb(0x3A, 0x1E, 0x2C), Color32::from_rgb(0xB4, 0x55, 0x2F));
            let mut c =
                ui.new_child(egui::UiBuilder::new().max_rect(thumb.shrink(theme::S2)).layout(egui::Layout::left_to_right(Align::Min)));
            chip_dark(&mut c, None, true, "LIVE");
            Frame::new().inner_margin(Margin::same(theme::S3 as i8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.spacing_mut().item_spacing.y = theme::S2;
                let row = HBox::new("who", 22.0)
                    .slots(&[fixed(theme::AVATAR_SM), fixed(text_width(ui, who, font(Weight::Bold, 13.5), 0.0)), FILL])
                    .show(ui);
                paint_avatar(ui, row.cell(0), &who[..1], color, false);
                row.text(ui, 1, who, font(Weight::Bold, 13.5), theme::TEXT, Align::Min);
                row.text(ui, 2, title, font(Weight::Regular, theme::FS_SMALL), theme::TEXT_DIM, Align::Min);
                let (sr, _) = ui.allocate_exact_size(vec2(ui.available_width(), 16.0), egui::Sense::hover());
                paint_text(ui, sr, specs, font(Weight::Mono, theme::FS_TINY), theme::TEXT_DIM, Align::Min);
                btn_row(ui, &[Btn::primary("Watch").icon("play_arrow"), Btn::secondary("Pop out").icon("open_in_new")]);
            });
        });
}
