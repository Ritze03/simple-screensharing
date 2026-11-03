//! UI tests: the session-model lookups and bandwidth wiring ported from the
//! old `ui.rs` (their logic is lib logic the UI relies on), the geometry
//! helpers, and the new shell: room list, navigation actions, panel layout.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::*;

/// Points `XDG_CONFIG_HOME` at a private temp dir while held, so nothing
/// here reads or writes the developer's real `rooms.toml` / settings. Same
/// approach as the lib's `session::test_support::isolated_config_home`.
struct IsolatedHome {
    _guard: MutexGuard<'static, ()>,
    dir: PathBuf,
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn isolated_home() -> IsolatedHome {
    static LOCK: Mutex<()> = Mutex::new(());
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("sss-ui-test-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // SAFETY: only these tests touch the variable, serialized by `LOCK`.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &dir) };
    IsolatedHome { _guard: guard, dir }
}

/// A syntactically valid join string on TEST-NET-1 (never routes): a
/// connect to it stays pending, which is the "reconnecting" state.
fn unreachable_join_string(id: u8) -> String {
    simple_screensharing_client::net::format_join_string("192.0.2.1:4433", &[id; 16], &[0x42u8; 32])
}

// ---------------------------------------------------------------------------
// Ported from the old `ui.rs` `session_model_tests`
// ---------------------------------------------------------------------------

mod session_model {
    use indexmap::IndexMap;
    use simple_screensharing_client::capture;
    use simple_screensharing_client::protocol::{StreamId, StreamKind};
    use simple_screensharing_client::session::{
        AppState, CaptureSourceId, DEFAULT_BITRATE_KBPS, DEFAULT_FRAMERATE, DestEdit, Destination, DestinationId, LocalCapture,
        RoomHandle, ShareStatus, capture_feeding, find_destination, room_label,
    };
    use tokio::sync::oneshot;


    fn capture_with(destinations: IndexMap<DestinationId, Destination>) -> LocalCapture {
        let (_tx, rx) = oneshot::channel();
        LocalCapture {
            kind: StreamKind::Video,
            label: "test".to_string(),
            status: ShareStatus::Starting(rx),
            audio_for_capture: None,
            destinations,
        }
    }

    fn destination(room: RoomHandle, stream_id: StreamId) -> Destination {
        let params = capture::destination::VideoParams {
            width: 0,
            height: 0,
            framerate: DEFAULT_FRAMERATE,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            backend: capture::EncoderBackend::default(),
            tune: capture::TuneMode::default(),
            quality_level: capture::QualityLevel::default(),
            gop_millis: capture::DEFAULT_GOP_MILLIS,
            codec: capture::VideoCodec::default(),
        };
        Destination {
            room,
            stream_id,
            params,
            runtime: None,
            pending: None,
            edit: DestEdit {
                scale_percent: 100,
                framerate: DEFAULT_FRAMERATE,
                bitrate_kbps: DEFAULT_BITRATE_KBPS,
                tune: capture::TuneMode::default(),
                quality_level: capture::QualityLevel::default(),
                gop_millis: capture::DEFAULT_GOP_MILLIS,
                codec: capture::VideoCodec::default(),
            },
            apply_rx: None,
        }
    }

    /// Two rooms, each fed by its own capture, both using `stream_id` 0.
    fn two_rooms_same_stream_id() -> (IndexMap<CaptureSourceId, LocalCapture>, RoomHandle, RoomHandle) {
        let (room_a, room_b) = (RoomHandle(0), RoomHandle(1));
        let mut captures = IndexMap::new();
        captures.insert(CaptureSourceId(10), capture_with(IndexMap::from([(DestinationId(100), destination(room_a, 0))])));
        captures.insert(CaptureSourceId(11), capture_with(IndexMap::from([(DestinationId(101), destination(room_b, 0))])));
        (captures, room_a, room_b)
    }

    #[test]
    fn destination_without_a_runtime_reports_starting() {
        let dest = destination(RoomHandle(0), 0);
        assert_eq!(dest.state(), capture::destination::DestinationState::Starting);
    }

    #[test]
    fn attaching_a_runtime_makes_the_destination_report_the_real_state() {
        let mut dest = destination(RoomHandle(0), 0);
        let runtime = capture::destination::DestinationRuntime::shared(std::time::Instant::now(), dest.params);
        dest.runtime = Some(runtime.clone());
        assert_eq!(dest.state(), capture::destination::DestinationState::Starting);
        runtime.lock().unwrap().note_encoded_frame(true, b"idr", std::time::Instant::now());
        assert_eq!(dest.state(), capture::destination::DestinationState::Live);
    }

    /// #95: a live destination reports the backend its branch really runs on (x264 after
    /// a per-branch VA fallback), the probed one before any branch is up.
    #[test]
    fn a_destination_reports_the_backend_its_branch_runs_on() {
        let mut dest = destination(RoomHandle(0), 0);
        assert_eq!(dest.encoder_backend(), capture::detect_encoder_backend(), "no runtime: the probe");
        let runtime = capture::destination::DestinationRuntime::shared(std::time::Instant::now(), dest.params);
        dest.runtime = Some(runtime.clone());
        assert_eq!(dest.encoder_backend(), capture::detect_encoder_backend(), "runtime without a branch: the probe");
        for backend in [capture::EncoderBackend::Software, capture::EncoderBackend::Vaapi] {
            runtime.lock().unwrap().set_actual_backend(backend);
            assert_eq!(dest.encoder_backend(), backend);
        }
    }

    #[test]
    fn capture_feeding_disambiguates_the_same_stream_id_in_two_rooms() {
        let (captures, room_a, room_b) = two_rooms_same_stream_id();
        let (a, a_dest) = capture_feeding(&captures, room_a, 0).expect("room A's stream 0 must resolve");
        let (b, b_dest) = capture_feeding(&captures, room_b, 0).expect("room B's stream 0 must resolve");
        assert!(std::ptr::eq(a, &captures[&CaptureSourceId(10)]));
        assert!(std::ptr::eq(b, &captures[&CaptureSourceId(11)]));
        assert_eq!(a_dest, DestinationId(100));
        assert_eq!(b_dest, DestinationId(101));
        assert!(capture_feeding(&captures, room_a, 1).is_none());
        assert!(capture_feeding(&captures, RoomHandle(2), 0).is_none(), "an unknown room must never match");
    }

    #[test]
    fn find_destination_resolves_a_room_back_reference_to_its_owning_capture() {
        let (captures, room_a, room_b) = two_rooms_same_stream_id();
        let (capture_id, _, dest) = find_destination(&captures, DestinationId(101)).expect("destination must resolve");
        assert_eq!(capture_id, CaptureSourceId(11));
        assert_eq!(dest.room, room_b);
        let (capture_id, _, dest) = find_destination(&captures, DestinationId(100)).expect("destination must resolve");
        assert_eq!(capture_id, CaptureSourceId(10));
        assert_eq!(dest.room, room_a);
        assert!(find_destination(&captures, DestinationId(999)).is_none());
    }

    /// A room label must never contain the join string's key or server.
    #[test]
    fn room_label_falls_back_to_an_id_prefix_and_never_leaks_the_key() {
        let room_id = "00112233445566778899aabbccddeeff";
        let key = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
        let join_string = format!("relay.example:4433|{room_id}|{key}");
        let store = simple_screensharing_client::rooms::RoomStore::default();
        let label = room_label(&store, &join_string);
        assert!(label.contains("00112233"), "expected the room-id prefix in {label:?}");
        assert!(!label.contains(key));
        assert!(!label.contains("relay.example"));
        let mut store = simple_screensharing_client::rooms::RoomStore::default();
        store.upsert("movie night".to_string(), join_string.clone());
        assert_eq!(room_label(&store, &join_string), "movie night");
    }

    #[tokio::test]
    async fn session_ids_are_monotonic_and_allocated_from_separate_spaces() {
        let _home = super::isolated_home();
        let mut state = AppState::default();
        assert_eq!(state.alloc_capture_id(), CaptureSourceId(0));
        assert_eq!(state.alloc_capture_id(), CaptureSourceId(1));
        assert_eq!(state.alloc_destination_id(), DestinationId(0));
        assert_eq!(state.alloc_capture_id(), CaptureSourceId(2));
        assert_eq!(state.alloc_destination_id(), DestinationId(1));
    }

    /// #98: the room is free the moment Stop is clicked, not when the
    /// teardown ends ("Share screen" back at once, no LIVE / "Stop sharing"
    /// left over while a hung driver teardown runs).
    #[tokio::test]
    async fn share_state_is_idle_right_after_stop_share() {
        let _home = super::isolated_home();
        let mut state = AppState::default();
        let room = RoomHandle(0);
        let capture = CaptureSourceId(7);
        let (_tx, rx) = oneshot::channel();
        let mut cap = capture_with(IndexMap::from([(DestinationId(70), destination(room, 0))]));
        cap.status = ShareStatus::Starting(rx);
        state.captures.insert(capture, cap);
        let shell = crate::ui::Shell::default();
        assert!(matches!(crate::ui::share_state(&state, &shell, room), crate::ui::ShareState::Live { .. }));

        state.stop_share(capture);

        assert!(matches!(state.captures[&capture].status, ShareStatus::Stopping(_)), "the teardown is still running");
        assert!(matches!(crate::ui::share_state(&state, &shell, room), crate::ui::ShareState::Idle));
    }
}

// ---------------------------------------------------------------------------
// Ported from the old `ui.rs` `bandwidth_wiring_tests` (unchanged logic)
// ---------------------------------------------------------------------------

mod bandwidth_wiring {
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    use simple_screensharing_client::bandwidth::{AllocInput, DestId, DestSpec};
    use simple_screensharing_client::session::{
        ALLOC_RECOMPUTE_INTERVAL, BandwidthController, DestinationId, MAX_BITRATE_KBPS, needs_reapply,
    };

    fn spec(id: u32, width: u32, height: u32, fps: u32) -> DestSpec {
        DestSpec { id: DestId(id), width, height, fps, floor_kbps: 500, ceiling_kbps: Some(MAX_BITRATE_KBPS) }
    }

    fn input(total_kbps: u32, destinations: Vec<DestSpec>) -> AllocInput {
        AllocInput { total_kbps, audio_reserved_kbps: 0, destinations }
    }

    #[derive(Default)]
    struct FakeEncoders {
        bitrates: HashMap<DestinationId, u32>,
        writes: u32,
    }

    impl FakeEncoders {
        fn apply(&mut self, controller: &mut BandwidthController, now: Instant, input: &AllocInput) {
            let Some(tick) = controller.tick(now, input) else { return };
            for (id, kbps) in tick.writes {
                self.bitrates.insert(id, kbps);
                self.writes += 1;
                controller.note_applied(id, kbps);
            }
        }
    }

    #[test]
    fn absolute_mode_converges_two_destinations_onto_the_allocators_output() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let input = input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        encoders.apply(&mut controller, Instant::now(), &input);
        let expected = simple_screensharing_client::bandwidth::allocate(&input);
        for (id, kbps) in &expected.per_destination {
            assert_eq!(encoders.bitrates.get(&DestinationId(id.0)), Some(kbps));
        }
        let big = encoders.bitrates[&DestinationId(1)];
        let small = encoders.bitrates[&DestinationId(2)];
        assert!(big > small * 3);
        assert_eq!(big + small, 20_000);
        assert_eq!(encoders.writes, 2);
    }

    #[test]
    fn starting_another_stream_anywhere_shrinks_every_other_destinations_share() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let now = Instant::now();
        let two = input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        encoders.apply(&mut controller, now, &two);
        let before = encoders.bitrates.clone();
        let three = input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30), spec(3, 1920, 1080, 30)]);
        encoders.apply(&mut controller, now + ALLOC_RECOMPUTE_INTERVAL, &three);
        for id in [DestinationId(1), DestinationId(2)] {
            assert!(encoders.bitrates[&id] < before[&id]);
        }
        assert_eq!(encoders.bitrates.values().sum::<u32>(), 20_000);
    }

    #[test]
    fn recomputes_are_rate_limited_to_one_per_interval() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let now = Instant::now();
        encoders.apply(&mut controller, now, &input(20_000, vec![spec(1, 1920, 1080, 60)]));
        assert_eq!(encoders.writes, 1);
        let two = input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1920, 1080, 60)]);
        for step in [10u64, 200, 900] {
            encoders.apply(&mut controller, now + Duration::from_millis(step), &two);
            assert_eq!(encoders.writes, 1, "a recompute at +{step}ms should have been rate-limited away");
        }
        encoders.apply(&mut controller, now + ALLOC_RECOMPUTE_INTERVAL, &two);
        assert_eq!(encoders.writes, 3);
        assert_eq!(encoders.bitrates[&DestinationId(1)], 10_000);
    }

    #[test]
    fn hysteresis_suppresses_redundant_and_tiny_reapplications() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let mut now = Instant::now();
        let steady = input(20_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        encoders.apply(&mut controller, now, &steady);
        assert_eq!(encoders.writes, 2);
        for _ in 0..5 {
            now += ALLOC_RECOMPUTE_INTERVAL;
            encoders.apply(&mut controller, now, &steady);
        }
        assert_eq!(encoders.writes, 2);
        now += ALLOC_RECOMPUTE_INTERVAL;
        encoders.apply(&mut controller, now, &input(20_050, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]));
        assert_eq!(encoders.writes, 2);
        now += ALLOC_RECOMPUTE_INTERVAL;
        encoders.apply(&mut controller, now, &input(10_000, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]));
        assert_eq!(encoders.writes, 4);
    }

    #[test]
    fn the_hysteresis_threshold_is_the_looser_of_five_percent_and_100_kbps() {
        assert!(needs_reapply(None, 1_000));
        assert!(!needs_reapply(Some(8_000), 8_090));
        assert!(needs_reapply(Some(8_000), 8_150));
        assert!(!needs_reapply(Some(400), 415));
        assert!(needs_reapply(Some(400), 430));
        assert!(!needs_reapply(Some(1_000), 1_000));
    }

    #[test]
    fn overshoot_is_kept_for_the_warning_banner() {
        let mut controller = BandwidthController::new();
        let input = input(800, vec![spec(1, 1920, 1080, 60), spec(2, 1280, 720, 30)]);
        let tick = controller.tick(Instant::now(), &input).expect("first tick always recomputes");
        assert_eq!(tick.allocation.overshoot_kbps, 200);
        for (_, kbps) in &tick.allocation.per_destination {
            assert_eq!(*kbps, 500);
        }
        assert_eq!(controller.allocation().map(|a| a.overshoot_kbps), Some(200));
    }

    #[test]
    fn resetting_forgets_what_the_allocator_had_applied() {
        let mut controller = BandwidthController::new();
        let mut encoders = FakeEncoders::default();
        let now = Instant::now();
        let steady = input(20_000, vec![spec(1, 1920, 1080, 60)]);
        encoders.apply(&mut controller, now, &steady);
        assert_eq!(controller.applied_kbps(DestinationId(1)), Some(20_000));
        controller.reset();
        assert_eq!(controller.applied_kbps(DestinationId(1)), None);
        encoders.apply(&mut controller, now, &steady);
        assert_eq!(encoders.writes, 2);
    }
}

// ---------------------------------------------------------------------------
// The new shell
// ---------------------------------------------------------------------------

fn app_with_rooms(names: &[(&str, String)]) -> App {
    let mut state = AppState::default();
    state.room_store = Default::default();
    for (name, js) in names {
        state.room_store.upsert(name.to_string(), js.clone());
    }
    App::with_state(state)
}

fn frame(app: &mut App, ctx: &egui::Context, size: egui::Vec2) {
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    };
    let _ = ctx.run(input, |ctx| {
        let actions = app.draw(ctx);
        for a in actions {
            app.apply(ctx, a);
        }
    });
}

fn ctx() -> egui::Context {
    let ctx = egui::Context::default();
    theme::apply(&ctx);
    ctx
}

#[tokio::test]
async fn room_list_is_saved_order_and_a_pending_connect_reads_as_connecting() {
    let _home = isolated_home();
    let a = unreachable_join_string(1);
    let b = unreachable_join_string(2);
    let mut app = app_with_rooms(&[("Alpha", a.clone()), ("Beta", b.clone())]);
    let rooms = room_entries(&app.state);
    assert_eq!(rooms.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["Alpha", "Beta"]);
    assert!(rooms.iter().all(|r| r.conn == ConnState::Disconnected && r.handle.is_none()));
    // First saved room is selected at startup.
    assert_eq!(app.nav.view, View::Room(a.clone()));

    app.state.start_connect(b.clone(), false).unwrap();
    let rooms = room_entries(&app.state);
    assert_eq!(rooms[1].conn, ConnState::Connecting);
    // A connect to a link that isn't saved still gets a row.
    app.state.cancel_connect();
    let c = unreachable_join_string(3);
    app.state.start_connect(c.clone(), false).unwrap();
    let rooms = room_entries(&app.state);
    assert_eq!(rooms.len(), 3);
    assert_eq!(rooms[2].key, c);
    assert_eq!(rooms[2].conn, ConnState::Connecting);
    assert!(!rooms[2].name.contains("AAAA"), "a row name must never contain the key");
    app.state.cancel_connect();
}

/// #31: a join string re-spelled with a (different) relay cert hash is the
/// same room: one row, found by either spelling, never queued twice.
#[tokio::test]
async fn a_cert_hash_respelling_is_the_same_room() {
    let _home = isolated_home();
    let a = unreachable_join_string(1);
    let hashed = format!("{a}|aZzvDUgnp0ZwLozhaaRsgsdWKtMyGPIwNikFgwbCJlg");
    assert!(same_room(&a, &hashed));
    let mut app = app_with_rooms(&[("Alpha", a.clone())]);
    let ctx = ctx();
    app.state.start_connect(hashed.clone(), false).unwrap();
    let rooms = room_entries(&app.state);
    assert_eq!(rooms.len(), 1, "the in-flight respelling must not add a second row");
    assert_eq!(rooms[0].conn, ConnState::Connecting);
    app.apply(&ctx, UiAction::SelectRoom(hashed.clone()));
    assert!(view_is_room(&app.nav.view, &a));
    app.apply(&ctx, UiAction::Connect(a.clone()));
    app.apply(&ctx, UiAction::Connect(hashed.clone()));
    assert_eq!(app.state.auto_connect_queue.len(), 1, "queued once whatever the spelling");
    app.state.cancel_connect();
}

#[tokio::test]
async fn auto_connect_rooms_are_queued_at_startup() {
    let _home = isolated_home();
    let a = unreachable_join_string(1);
    let mut state = AppState::default();
    state.room_store = Default::default();
    state.room_store.upsert("A".into(), a.clone());
    state.room_store.upsert("B".into(), unreachable_join_string(2));
    state.room_store.set_auto_connect(&a, true);
    let app = App::with_state(state);
    assert_eq!(app.state.auto_connect_queue, vec![a]);
}

#[tokio::test]
async fn opening_a_modal_closes_the_popover_and_escape_like_close_actions_apply() {
    let _home = isolated_home();
    let mut app = app_with_rooms(&[]);
    let ctx = ctx();
    assert_eq!(app.nav.view, View::Welcome);
    let anchor = Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(20.0, 20.0));
    app.apply(&ctx, UiAction::OpenPopover(Popover { kind: PopoverKind::Outgoing, anchor }));
    assert!(app.nav.popover.is_some());
    app.apply(&ctx, UiAction::OpenModal(Modal::Settings));
    assert_eq!(app.nav.modal, Some(Modal::Settings));
    assert!(app.nav.popover.is_none(), "a modal must close the popover");
    app.apply(&ctx, UiAction::CloseModal);
    assert!(app.nav.modal.is_none());
    app.apply(&ctx, UiAction::ShowWatching);
    assert_eq!(app.nav.view, View::Watching);
}

#[tokio::test]
async fn copy_logs_puts_the_dump_on_the_clipboard_and_toasts() {
    let _home = isolated_home();
    let mut app = app_with_rooms(&[]);
    let ctx = ctx();
    app.apply(&ctx, UiAction::CopyLogs);
    let copied = ctx.output(|o| {
        o.commands.iter().find_map(|c| match c {
            egui::OutputCommand::CopyText(t) => Some(t.clone()),
            _ => None,
        })
    });
    assert!(copied.is_some_and(|t| t.starts_with("Simple Screensharing client log\n")));
    assert_eq!(app.shell.toasts.len(), 1);
}

#[tokio::test]
async fn the_send_diagnostics_checkbox_is_off_by_default_and_persists() {
    // The upload switch is process-wide; the diag_upload tests assert on it under this lock.
    let _lock = simple_screensharing_client::logbuf::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let _home = isolated_home();
    let mut app = app_with_rooms(&[]);
    let ctx = ctx();
    assert!(!app.state.send_diagnostics, "opt-in: off until ticked");
    assert!(!simple_screensharing_client::diag_upload::enabled());
    app.apply(&ctx, UiAction::SetSendDiagnostics(true));
    assert!(app.state.send_diagnostics && simple_screensharing_client::diag_upload::enabled());
    assert!(simple_screensharing_client::rooms::AppSettings::load().send_diagnostics, "saved to settings.toml");
    app.apply(&ctx, UiAction::SetSendDiagnostics(false));
    assert!(!app.state.send_diagnostics && !simple_screensharing_client::diag_upload::enabled());
    assert!(!simple_screensharing_client::rooms::AppSettings::load().send_diagnostics);
}

#[tokio::test]
async fn toggle_popover_opens_then_closes_the_same_kind() {
    let _home = isolated_home();
    let app = app_with_rooms(&[]);
    let anchor = Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0));
    let mut actions = Vec::new();
    {
        let mut cx = Cx::new(&app.state, &app.nav, &app.shell, &app.videos, &mut actions);
        cx.toggle_popover(PopoverKind::Outgoing, anchor);
    }
    assert!(matches!(actions.as_slice(), [UiAction::OpenPopover(_)]));
    let nav = Nav { popover: Some(Popover { kind: PopoverKind::Outgoing, anchor }), ..app.nav.clone() };
    let mut actions = Vec::new();
    {
        let mut cx = Cx::new(&app.state, &nav, &app.shell, &app.videos, &mut actions);
        cx.toggle_popover(PopoverKind::Outgoing, anchor);
    }
    assert!(matches!(actions.as_slice(), [UiAction::ClosePopover]));
}

#[tokio::test]
async fn forgetting_the_selected_room_removes_it_and_selects_the_next() {
    let _home = isolated_home();
    let a = unreachable_join_string(1);
    let b = unreachable_join_string(2);
    let mut app = app_with_rooms(&[("Alpha", a.clone()), ("Beta", b.clone())]);
    let ctx = ctx();
    app.apply(&ctx, UiAction::OpenModal(Modal::ForgetConfirm { room: a.clone() }));
    app.apply(&ctx, UiAction::Forget(a.clone()));
    assert!(app.state.room_store.name_for(&a).is_none());
    assert_eq!(app.nav.view, View::Room(b.clone()));
    assert!(app.nav.modal.is_none());
}

#[tokio::test]
async fn joining_a_bad_link_reports_inline_and_starts_nothing() {
    let _home = isolated_home();
    let mut app = app_with_rooms(&[]);
    let ctx = ctx();
    app.apply(&ctx, UiAction::JoinRoom { join_string: "not a link".into(), room_name: String::new() });
    assert!(app.shell.join_error.is_some());
    assert!(app.state.pending_connect.is_none());
    let good = unreachable_join_string(9);
    app.apply(&ctx, UiAction::JoinRoom { join_string: good.clone(), room_name: "Nine".into() });
    assert!(app.shell.join_error.is_none());
    assert_eq!(app.shell.awaiting_join.as_deref(), Some(good.as_str()));
    assert_eq!(app.state.room_store.name_for(&good), Some("Nine"));
    app.state.cancel_connect();
}

/// The shell geometry the spec pins: sidebar 272, people 232, every band 56,
/// the user band and the action band on one line, at the 800x600 minimum
/// window (PR #4) and three common sizes.
#[tokio::test]
async fn shell_regions_match_the_spec_sizes() {
    let _home = isolated_home();
    let a = unreachable_join_string(1);
    let mut app = app_with_rooms(&[("Alpha", a.clone())]);
    // A pending first connect makes the selected room "connecting": People
    // panel and action bar are shown (Paused, with connecting wording).
    app.state.start_connect(a.clone(), false).unwrap();
    let ctx = ctx();
    for (w, h) in [(800.0, 600.0), (1280.0, 800.0), (1600.0, 900.0), (1920.0, 1080.0)] {
        frame(&mut app, &ctx, egui::vec2(w, h));
        frame(&mut app, &ctx, egui::vec2(w, h));
        let regions: HashMap<&str, Rect> = shell_regions(&ctx).into_iter().collect();
        let get = |n: &str| *regions.get(n).unwrap_or_else(|| panic!("region {n} missing at {w}x{h}: {regions:?}"));
        let (brand, body, user) = (get("brand"), get("sidebar_body"), get("user_band"));
        assert_eq!(brand.width(), theme::SIDEBAR_W - 1.0, "sidebar inner width (1 px border) at {w}");
        assert_eq!((brand.top(), brand.height()), (0.0, theme::BAND_H));
        assert_eq!((user.bottom(), user.height()), (h, theme::BAND_H));
        assert_eq!((body.top(), body.bottom()), (theme::BAND_H, h - theme::BAND_H));
        let (header, bar, people, center) = (get("room_header"), get("action_bar"), get("people"), get("room_body"));
        assert_eq!((header.left(), header.right(), header.top(), header.height()), (theme::SIDEBAR_W, w, 0.0, theme::BAND_H));
        assert_eq!((bar.left(), bar.right(), bar.height()), (theme::SIDEBAR_W, w, theme::BAND_H));
        assert_eq!(bar.top(), user.top(), "action band and user band share one top line");
        assert_eq!(people.width(), theme::PEOPLE_W - 1.0, "people inner width (1 px border)");
        assert_eq!((people.top(), people.bottom(), people.right()), (theme::BAND_H, h - theme::BAND_H, w));
        assert_eq!((center.left(), center.right()), (theme::SIDEBAR_W, w - theme::PEOPLE_W));
        assert_eq!((center.top(), center.bottom()), (theme::BAND_H, h - theme::BAND_H));
    }
    // Disconnected: no People panel, no action bar; the body takes the rest.
    app.state.cancel_connect();
    frame(&mut app, &ctx, egui::vec2(1280.0, 800.0));
    frame(&mut app, &ctx, egui::vec2(1280.0, 800.0));
    let regions: HashMap<&str, Rect> = shell_regions(&ctx).into_iter().collect();
    assert!(!regions.contains_key("people") && !regions.contains_key("action_bar"));
    assert_eq!(regions["room_body"], Rect::from_min_max(egui::pos2(theme::SIDEBAR_W, theme::BAND_H), egui::pos2(1280.0, 800.0)));
}

/// Mockup `positionPopup`: below and left-aligned; above when it would cross
/// the bottom; pushed left (never flipped to right-aligned) at the right edge.
#[test]
fn popover_hangs_off_the_anchor_like_the_mockup() {
    let screen = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
    let size = egui::vec2(340.0, 200.0);
    let a = Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(120.0, 32.0));
    assert_eq!(popover_pos(screen, a, size), egui::pos2(100.0, 88.0));
    // A bottom-bar button near the right edge: above it, pushed left to 8 px in.
    let b = Rect::from_min_size(egui::pos2(1000.0, 750.0), egui::vec2(124.0, 36.0));
    assert_eq!(popover_pos(screen, b, size), egui::pos2(1280.0 - 8.0 - 340.0, 750.0 - 6.0 - 200.0));
}

/// Stage-6 QC: our capture's label is stored in its creation language; the
/// UI shows any locale's default in the current one, and other labels as is.
#[test]
fn share_label_translates_known_defaults_only() {
    let here = tr("share.video_label", "Screen share");
    assert_eq!(share_label("Screen share"), here);
    assert_eq!(share_label("Bildschirmfreigabe"), here);
    assert_eq!(share_label(""), here);
    assert_eq!(share_label("Screen"), tr("ui.stream.screen", "Screen"));
    assert_eq!(share_label("Spotify"), "Spotify");
}

/// A failed Connect's reason is kept per room (any spelling of it) and
/// cleared by the next attempt.
#[test]
fn connect_errors_are_per_room_and_cleared_by_a_new_attempt() {
    let a = unreachable_join_string(1);
    let hashed = format!("{a}|aZzvDUgnp0ZwLozhaaRsgsdWKtMyGPIwNikFgwbCJlg");
    let mut shell = Shell::default();
    shell.connect_errors.push((a.clone(), "timed out".into()));
    assert_eq!(shell.connect_error(&hashed), Some("timed out"));
    assert_eq!(shell.connect_error(&unreachable_join_string(2)), None);
    shell.clear_connect_error(&hashed);
    assert_eq!(shell.connect_error(&a), None);
    assert_eq!(short_error("\n  first line\nsecond"), "first line");
    assert_eq!(short_error(&"x".repeat(200)).chars().count(), 90);
}

// ---------------------------------------------------------------------------
// "Choose your name" modal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn choose_name_modal_opens_once_while_the_name_is_unset() {
    let _home = isolated_home();
    let mut app = app_with_rooms(&[]);
    let ctx = ctx();
    assert!(!app.state.display_name_set, "a fresh config has no saved name");
    app.prompt_for_name();
    assert_eq!(app.nav.modal, Some(Modal::ChooseName));
    // Later (CloseModal): not offered again this run.
    app.apply(&ctx, UiAction::CloseModal);
    app.prompt_for_name();
    assert!(app.nav.modal.is_none());
    // A new start (fresh App, still nothing saved): offered again.
    let mut app = app_with_rooms(&[]);
    app.prompt_for_name();
    assert_eq!(app.nav.modal, Some(Modal::ChooseName));
}

#[tokio::test]
async fn choose_name_modal_waits_for_another_open_modal() {
    let _home = isolated_home();
    let mut app = app_with_rooms(&[]);
    app.nav.modal = Some(Modal::Settings);
    app.prompt_for_name();
    assert_eq!(app.nav.modal, Some(Modal::Settings));
    app.nav.modal = None;
    app.prompt_for_name();
    assert_eq!(app.nav.modal, Some(Modal::ChooseName));
}

#[tokio::test]
async fn saving_a_name_closes_the_modal_and_it_never_comes_back() {
    let _home = isolated_home();
    let mut app = app_with_rooms(&[]);
    let ctx = ctx();
    app.prompt_for_name();
    // What the modal's Save pushes.
    app.apply(&ctx, UiAction::SetDisplayName("Mo".into()));
    app.apply(&ctx, UiAction::CloseModal);
    assert!(app.state.display_name_set);
    assert_eq!(app.state.display_name, "Mo");
    assert!(app.nav.modal.is_none());
    // Restart: the name is saved, so no prompt even on a fresh App.
    let mut app = app_with_rooms(&[]);
    assert!(app.state.display_name_set);
    app.prompt_for_name();
    assert!(app.nav.modal.is_none());
}

/// #73: "Live in <room>" is announced once per on-air spell, never for a
/// share that is only starting or has failed (those are not in `on_air`).
#[test]
fn live_in_toast_fires_once_per_on_air_spell() {
    use super::newly_on_air;
    use simple_screensharing_client::session::RoomHandle;
    let mut announced = Vec::new();
    assert!(newly_on_air(&mut announced, &[]).is_empty(), "starting or failed share: nothing on air");
    assert_eq!(newly_on_air(&mut announced, &[RoomHandle(7)]), vec![RoomHandle(7)]);
    assert!(newly_on_air(&mut announced, &[RoomHandle(7)]).is_empty(), "no repeat while it stays on air");
    assert!(newly_on_air(&mut announced, &[]).is_empty());
    assert_eq!(newly_on_air(&mut announced, &[RoomHandle(7)]), vec![RoomHandle(7)], "announced again after leaving air");
}

/// #75: the user saw the bar's pill clipped to "CONNECTI" (fixed 84 px slot).
/// The bar's pill slot fits every label it can show, in every locale, and the
/// small LIVE pill fits `person_row`'s 44 px live slot.
#[test]
fn pill_slots_fit_every_label_in_every_locale() {
    use super::action_bar::{BAR_PILLS, bar_pill_w_with};
    use crate::widgets::{Pill, PillLead};
    let ctx = ctx();
    let mut checked = 0;
    for _ in 0..2 {
        checked = 0;
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                for loc in ["en", "de"] {
                    let text = |k: &str, en: &'static str| {
                        let s = t!(k, locale = loc);
                        if s.as_ref() == k || s.ends_with(k) { en.to_string() } else { s.into_owned() }
                    };
                    let slot = bar_pill_w_with(ui, text);
                    for (lead, k, en) in BAR_PILLS {
                        let label = text(k, en);
                        let w = Pill::muted(&label).lead(lead).large().width(ui);
                        assert!(w <= slot, "{loc}: bar pill {label:?} is {w} px, slot {slot}");
                        checked += 1;
                    }
                    let live = text("ui.pill.live", "Live");
                    let w = Pill::live(&live).lead(PillLead::None).width(ui);
                    assert!(w <= 44.0, "{loc}: people-row LIVE pill is {w} px, slot 44");
                }
                let connecting = Pill::progress("Connecting").large().width(ui);
                assert!(bar_pill_w_with(ui, |_, en| en.to_string()) >= connecting, "regression: CONNECTING must fit");
                assert!(connecting > 84.0, "the old 84 px slot really was too narrow ({connecting})");
            });
        });
    }
    assert_eq!(checked, 2 * BAR_PILLS.len());
}

#[tokio::test]
async fn stream_fullscreen_needs_a_watched_stream_and_exit_restores_the_window_state() {
    let _home = isolated_home();
    let mut app = app_with_rooms(&[]);
    let ctx = ctx();
    let key = (RoomHandle(1), 5, 1);
    // No such watched stream: nothing happens.
    app.apply(&ctx, UiAction::EnterStreamFullscreen(key));
    assert!(app.nav.fullscreen.is_none());
    // A stale state (stream gone) is dropped by the next frame.
    app.nav.fullscreen = Some(FullscreenStream { key, was_fullscreen: true });
    app.apply(&ctx, UiAction::ExitStreamFullscreen);
    assert!(app.nav.fullscreen.is_none());
    app.nav.fullscreen = Some(FullscreenStream { key, was_fullscreen: false });
    frame(&mut app, &ctx, egui::vec2(1000.0, 700.0));
    assert!(app.nav.fullscreen.is_none(), "the stream is not watched, so the layout comes back");
}

// ---------------------------------------------------------------------------
// Audio boost (task #114, capped at 200 % and calibration-free since #119) and the viewer's
// "Level loudness" switch (#119). No PipeWire stream is ever created.
// ---------------------------------------------------------------------------

mod audio_boost {
    use super::*;
    use simple_screensharing_client::audio_gain;
    use simple_screensharing_client::rooms::AppSettings;

    #[tokio::test]
    async fn the_boost_defaults_to_one_persists_and_is_clamped_to_200_percent() {
        let _home = isolated_home();
        let mut app = app_with_rooms(&[]);
        let ctx = ctx();
        assert_eq!(app.state.audio_gain, 1.0, "no boost until the user sets one");
        app.apply(&ctx, UiAction::SetAudioGain(1.5));
        assert_eq!(app.state.audio_gain, 1.5);
        assert_eq!(AppSettings::load().audio_gain(), 1.5, "saved to settings.toml");
        app.apply(&ctx, UiAction::SetAudioGain(100.0));
        assert_eq!(app.state.audio_gain, audio_gain::MAX_GAIN);
        assert_eq!(audio_gain::MAX_GAIN, 2.0);
        app.apply(&ctx, UiAction::SetAudioGain(0.0));
        assert_eq!(app.state.audio_gain, audio_gain::MIN_GAIN);
        // A new start picks the saved value up (the next audio share starts with it).
        app.apply(&ctx, UiAction::SetAudioGain(1.8));
        let again = AppState::default();
        assert_eq!(again.audio_gain, 1.8);
    }

    /// Levelling is on by default, the switch is persisted and drives the process-wide flag
    /// every audio pipeline reads, and Settings draws with it on and off.
    #[tokio::test]
    async fn level_loudness_defaults_on_persists_and_drives_the_leveller_switch() {
        use std::sync::atomic::Ordering;
        let _home = isolated_home();
        let mut app = app_with_rooms(&[]);
        let ctx = ctx();
        let size = egui::vec2(1000.0, 900.0);
        assert!(app.state.level_loudness, "on by default");
        app.apply(&ctx, UiAction::OpenModal(Modal::Settings));
        frame(&mut app, &ctx, size);
        app.apply(&ctx, UiAction::SetLevelLoudness(false));
        assert!(!app.state.level_loudness);
        assert!(!simple_screensharing_client::leveller::ENABLED.load(Ordering::Relaxed));
        assert!(!AppSettings::load().level_loudness, "saved to settings.toml");
        frame(&mut app, &ctx, size);
        assert!(!AppState::default().level_loudness, "the next start remembers it");
        app.apply(&ctx, UiAction::SetLevelLoudness(true));
        assert!(simple_screensharing_client::leveller::ENABLED.load(Ordering::Relaxed));
        assert!(AppSettings::load().level_loudness);
        frame(&mut app, &ctx, size);
        assert_eq!(app.nav.modal, Some(Modal::Settings));
    }
}

/// Task #113: the mouse-wheel step of the viewer volume slider.
#[test]
fn wheel_step_is_5_up_to_200_and_10_above_and_symmetric_at_the_boundary() {
    // Up: 5 steps below 200, 10 from 200 on.
    assert_eq!(wheel_step(0, true), 5);
    assert_eq!(wheel_step(95, true), 100);
    assert_eq!(wheel_step(100, true), 105, "passes through the 100 % detent");
    assert_eq!(wheel_step(195, true), 200);
    assert_eq!(wheel_step(200, true), 210);
    assert_eq!(wheel_step(205, true), 210, "an off-grid 205 lands on the 10 grid");
    assert_eq!(wheel_step(490, true), 500);
    assert_eq!(wheel_step(500, true), 500, "clamped");
    // Down: the mirror image.
    assert_eq!(wheel_step(500, false), 490);
    assert_eq!(wheel_step(220, false), 210);
    assert_eq!(wheel_step(210, false), 200);
    assert_eq!(wheel_step(205, false), 200);
    assert_eq!(wheel_step(200, false), 195);
    assert_eq!(wheel_step(105, false), 100);
    assert_eq!(wheel_step(5, false), 0);
    assert_eq!(wheel_step(0, false), 0, "clamped");
    // Up then down is the identity everywhere on the grid.
    for p in (0..500).step_by(5).filter(|p| *p <= 195 || p % 10 == 0) {
        assert_eq!(wheel_step(wheel_step(p, true), false), p, "p={p}");
    }
}

/// Task #113: real egui events. A wheel notch over the slider steps the value and
/// is consumed (the `ScrollArea` around it does not move); a notch beside it
/// scrolls the area and leaves the value alone.
#[test]
fn wheel_over_the_volume_slider_steps_it_and_does_not_scroll_the_panel() {
    let ctx = ctx();
    let size = egui::vec2(300.0, 200.0);
    let mut percent = 195u32;
    let mut changed = false;
    let mut offset = 0.0f32;
    let mut slider_rect = Rect::NOTHING;
    let run = |events: Vec<egui::Event>, percent: &mut u32, changed: &mut bool, offset: &mut f32, slider_rect: &mut Rect| {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, size)), events, ..Default::default() };
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let out = egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.set_width(200.0);
                    let r = volume_slider(ui, percent);
                    *slider_rect = r.rect;
                    *changed = r.changed();
                    ui.allocate_space(egui::vec2(10.0, 2000.0));
                });
                *offset = out.state.offset.y;
            });
        });
    };
    let wheel = |y: f32| egui::Event::MouseWheel { unit: egui::MouseWheelUnit::Line, delta: egui::vec2(0.0, y), modifiers: egui::Modifiers::NONE };
    run(vec![], &mut percent, &mut changed, &mut offset, &mut slider_rect);
    let on = slider_rect.center();
    run(vec![egui::Event::PointerMoved(on)], &mut percent, &mut changed, &mut offset, &mut slider_rect);
    // Wheel forward over the slider: 195 -> 200 -> 210, then back 210 -> 200 -> 195.
    for want in [200, 210] {
        run(vec![wheel(1.0)], &mut percent, &mut changed, &mut offset, &mut slider_rect);
        assert_eq!(percent, want);
        assert!(changed);
    }
    for want in [200, 195] {
        run(vec![wheel(-1.0)], &mut percent, &mut changed, &mut offset, &mut slider_rect);
        assert_eq!(percent, want);
    }
    // Let the smoothing tail drain; the area never moved.
    for _ in 0..5 {
        run(vec![], &mut percent, &mut changed, &mut offset, &mut slider_rect);
    }
    assert_eq!(offset, 0.0, "the wheel was consumed by the slider");
    // Beside the slider the wheel scrolls the area and leaves the value alone.
    let off = egui::pos2(slider_rect.center().x, 150.0);
    run(vec![egui::Event::PointerMoved(off)], &mut percent, &mut changed, &mut offset, &mut slider_rect);
    run(vec![wheel(-1.0)], &mut percent, &mut changed, &mut offset, &mut slider_rect);
    for _ in 0..5 {
        run(vec![], &mut percent, &mut changed, &mut offset, &mut slider_rect);
    }
    assert_eq!(percent, 195);
    assert!(offset > 0.0, "the panel scrolls when the pointer is not on the slider");
}

/// #126: the loudness-levelling button next to the volume slider reports a click (the
/// caller then sends `SetLevelLoudness(!on)`) and nothing else.
#[test]
fn level_toggle_reports_a_click_only_on_the_button() {
    let ctx = ctx();
    let rect = Rect::from_min_size(egui::pos2(20.0, 20.0), egui::vec2(25.0, 25.0));
    let run = |events: Vec<egui::Event>, on: bool| {
        let input = egui::RawInput { screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(200.0, 100.0))), events, ..Default::default() };
        let mut clicked = false;
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| clicked = level_toggle(ui, rect, egui::Id::new("lt"), on));
        });
        clicked
    };
    let btn = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
    for on in [true, false] {
        run(vec![egui::Event::PointerMoved(rect.center())], on);
        run(vec![btn(rect.center(), true)], on);
        assert!(run(vec![btn(rect.center(), false)], on), "click on the button, on={on}");
        let away = egui::pos2(150.0, 80.0);
        run(vec![egui::Event::PointerMoved(away)], on);
        run(vec![btn(away, true)], on);
        assert!(!run(vec![btn(away, false)], on), "click beside it, on={on}");
    }
}

/// #115: after an audio source switch the old share is `Stopping` for a second
/// or more, and both audio captures point at the video capture. The bar must
/// follow the new one (its level, its mute), not the dying one.
#[test]
fn the_bar_follows_the_live_audio_share_not_the_stopping_one() {
    use simple_screensharing_client::session::{LocalCapture, ShareStatus};
    let audio = |status: ShareStatus, label: &str| LocalCapture {
        kind: StreamKind::Audio,
        label: label.to_string(),
        status,
        audio_for_capture: Some(CaptureSourceId(10)),
        destinations: Default::default(),
    };
    let (_tx_old, rx_old) = tokio::sync::oneshot::channel();
    let (_tx_new, rx_new) = tokio::sync::oneshot::channel();
    let mut state = AppState::default();
    state.captures.insert(CaptureSourceId(20), audio(ShareStatus::Stopping(rx_old), "old"));
    assert_eq!(paired_audio(&state, CaptureSourceId(10)), Some(CaptureSourceId(20)), "a lone stopping share is still shown");
    state.captures.insert(CaptureSourceId(21), audio(ShareStatus::Starting(rx_new), "new"));
    assert_eq!(paired_audio(&state, CaptureSourceId(10)), Some(CaptureSourceId(21)));
    assert_eq!(paired_audio(&state, CaptureSourceId(99)), None);
}
