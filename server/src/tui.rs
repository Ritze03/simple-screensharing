//! Ratatui admin status interface.
//!
//! Runs on its own dedicated OS thread (not a tokio task) so that
//! crossterm's blocking `poll`/`read` calls never block the tokio
//! networking runtime. It reads the shared [`Registry`] via
//! `blocking_read`/`blocking_lock`, which is safe here precisely because
//! this thread is *not* a tokio worker thread — it never enters the
//! runtime's async execution context.
//!
//! Three tabs: Overview (bandwidth/uptime/room count), Rooms (table +
//! per-participant sub-view of the highlighted room), and Room Detail
//! (drill in via Enter: per-stream bandwidth, subscriber counts, and a
//! scrolling event-log tail for that room).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Borders, Cell, Chart, Dataset, GraphType, List, ListItem, Paragraph, Row, Table,
    Tabs,
};

use crate::protocol::RoomId;
use crate::room::Registry;

const TAB_TITLES: [&str; 3] = ["Overview", "Rooms", "Room Detail"];
const REDRAW_INTERVAL: Duration = Duration::from_millis(200);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// How often the combined (baseline + session) lifetime counters are
/// flushed to disk while running. Deliberately coarser than the 1s
/// bandwidth sample: this is a durability write, not something the UI
/// needs fresh — periodic-plus-best-effort-on-clean-exit is enough (an
/// unclean exit loses at most this much of the current session, which is
/// re-derived from `ByteCounters` on the next graceful save anyway).
const LIFETIME_SAVE_INTERVAL: Duration = Duration::from_secs(30);
/// Where the persisted lifetime rx/tx baseline lives, relative to the
/// working directory — same "relative to cwd" convention as `LOG_DIR` in
/// `main.rs`, reusing the `logs/` directory rather than adding a new
/// top-level file.
const LIFETIME_STATE_PATH: &str = "logs/lifetime.state";
/// Number of bandwidth samples kept for the Overview network-usage graph
/// (one point per `SAMPLE_INTERVAL`, so 120 * 1s = the last two minutes).
const HISTORY_LEN: usize = 120;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Overview,
    Rooms,
    RoomDetail,
}

/// One point-in-time snapshot of everything the TUI needs to render,
/// pulled from the shared registry under its locks. Rebuilt every redraw
/// tick so the render closure itself never touches the registry (keeps
/// lock hold times tiny and predictable).
struct Snapshot {
    listen_addr: std::net::SocketAddr,
    uptime: Duration,
    room_count: usize,
    total_rx: u64,
    total_tx: u64,
    rooms: Vec<RoomSnapshot>,
}

struct RoomSnapshot {
    room_id: RoomId,
    members: Vec<MemberSnapshot>,
    streams: Vec<StreamSnapshot>,
    log_tail: Vec<String>,
}

struct MemberSnapshot {
    conn_id: u64,
    display_name: String,
    stream_count: u8,
    /// Number of *other* members' streams this member currently subscribes
    /// to as a viewer. Mirrors `stream_count` (sharing) but for watching —
    /// sourced from `ConnState::subscriptions`, which is authoritative for
    /// per-connection subscribe state (see `room.rs`).
    watching_count: u8,
}

struct StreamSnapshot {
    owner: u64,
    stream_id: u8,
    label: String,
    subscriber_count: usize,
    bytes_received: u64,
}

fn take_snapshot(registry: &Registry) -> Snapshot {
    let rooms_map = registry.rooms.blocking_read();
    let mut rooms = Vec::with_capacity(rooms_map.len());
    for (room_id, room) in rooms_map.iter() {
        let guard = room.blocking_lock();
        let members = guard
            .connections
            .values()
            .map(|c| MemberSnapshot {
                conn_id: c.conn_id,
                display_name: c.display_name.clone(),
                stream_count: c.stream_count,
                watching_count: c.subscriptions.len() as u8,
            })
            .collect();
        let streams = guard
            .streams
            .values()
            .map(|s| StreamSnapshot {
                owner: s.owner,
                stream_id: s.stream_id,
                label: s.label.clone(),
                subscriber_count: s.subscribers.len(),
                bytes_received: s.route.bytes_received.load(Ordering::Relaxed),
            })
            .collect();
        let log_tail = guard
            .log
            .iter()
            .rev()
            .take(200)
            .map(|e| e.text.clone())
            .collect::<Vec<_>>();
        rooms.push(RoomSnapshot {
            room_id: *room_id,
            members,
            streams,
            log_tail,
        });
    }

    Snapshot {
        listen_addr: registry.listen_addr,
        uptime: registry.started_at.elapsed(),
        room_count: rooms_map.len(),
        total_rx: registry.bandwidth.rx.load(Ordering::Relaxed),
        total_tx: registry.bandwidth.tx.load(Ordering::Relaxed),
        rooms,
    }
}

struct App {
    tab: Tab,
    rooms_cursor: usize,
    selected_room: Option<RoomId>,
    last_sample_at: Instant,
    last_rx: u64,
    last_tx: u64,
    rx_rate: f64,
    tx_rate: f64,
    per_stream_prev: HashMap<(u64, u8), (Instant, u64)>,
    per_stream_rate: HashMap<(u64, u8), f64>,
    /// Lifetime rx/tx persisted from *previous* runs (loaded once at
    /// startup). The displayed lifetime total is this baseline plus the
    /// current session's own `Snapshot::total_rx`/`total_tx`, which is
    /// how a genuinely restart-surviving counter is built on top of
    /// `ByteCounters` (which itself resets to zero every process start).
    lifetime_rx_base: u64,
    lifetime_tx_base: u64,
    last_lifetime_save_at: Instant,
    /// Rolling bandwidth-rate history for the Overview network-usage
    /// graph, one point pushed per `maybe_sample` tick, capped at
    /// `HISTORY_LEN` so it never grows unbounded.
    rx_history: VecDeque<f64>,
    tx_history: VecDeque<f64>,
}

impl App {
    fn new() -> Self {
        let (lifetime_rx_base, lifetime_tx_base) = load_lifetime_baseline();
        Self {
            tab: Tab::Overview,
            rooms_cursor: 0,
            selected_room: None,
            last_sample_at: Instant::now(),
            last_rx: 0,
            last_tx: 0,
            rx_rate: 0.0,
            tx_rate: 0.0,
            per_stream_prev: HashMap::new(),
            per_stream_rate: HashMap::new(),
            lifetime_rx_base,
            lifetime_tx_base,
            last_lifetime_save_at: Instant::now(),
            rx_history: VecDeque::with_capacity(HISTORY_LEN),
            tx_history: VecDeque::with_capacity(HISTORY_LEN),
        }
    }

    fn maybe_sample(&mut self, snap: &Snapshot) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample_at);
        if elapsed < SAMPLE_INTERVAL {
            return;
        }
        let secs = elapsed.as_secs_f64().max(0.001);
        self.rx_rate = (snap.total_rx.saturating_sub(self.last_rx)) as f64 / secs;
        self.tx_rate = (snap.total_tx.saturating_sub(self.last_tx)) as f64 / secs;
        self.last_rx = snap.total_rx;
        self.last_tx = snap.total_tx;

        push_capped(&mut self.rx_history, self.rx_rate);
        push_capped(&mut self.tx_history, self.tx_rate);

        for room in &snap.rooms {
            for s in &room.streams {
                let key = (s.owner, s.stream_id);
                let (prev_at, prev_bytes) = self
                    .per_stream_prev
                    .get(&key)
                    .copied()
                    .unwrap_or((self.last_sample_at, 0));
                let dt = now.duration_since(prev_at).as_secs_f64().max(0.001);
                let rate = (s.bytes_received.saturating_sub(prev_bytes)) as f64 / dt;
                self.per_stream_rate.insert(key, rate);
                self.per_stream_prev.insert(key, (now, s.bytes_received));
            }
        }
        self.last_sample_at = now;

        if now.duration_since(self.last_lifetime_save_at) >= LIFETIME_SAVE_INTERVAL {
            self.save_lifetime(snap);
            self.last_lifetime_save_at = now;
        }
    }

    fn selected_room_snapshot<'a>(&self, snap: &'a Snapshot) -> Option<&'a RoomSnapshot> {
        let id = self.selected_room?;
        snap.rooms.iter().find(|r| r.room_id == id)
    }

    /// Combined lifetime (rx, tx) = persisted baseline + this session's
    /// own cumulative counters.
    fn lifetime_totals(&self, snap: &Snapshot) -> (u64, u64) {
        (
            self.lifetime_rx_base + snap.total_rx,
            self.lifetime_tx_base + snap.total_tx,
        )
    }

    /// Best-effort write of the combined lifetime total back to disk.
    /// Called periodically from `maybe_sample` and once more on a clean
    /// ('q'/Ctrl-C) exit from `run_app`; an unclean exit only loses
    /// whatever the current session added since the last periodic save.
    fn save_lifetime(&self, snap: &Snapshot) {
        let (rx, tx) = self.lifetime_totals(snap);
        save_lifetime_baseline(rx, tx);
    }
}

fn push_capped(buf: &mut VecDeque<f64>, value: f64) {
    if buf.len() >= HISTORY_LEN {
        buf.pop_front();
    }
    buf.push_back(value);
}

/// Load the persisted lifetime rx/tx baseline written by a previous run.
/// Missing file, unreadable content, or a malformed line all fall back to
/// `(0, 0)` — this is best-effort durability, not something worth failing
/// startup over.
fn load_lifetime_baseline() -> (u64, u64) {
    let Ok(content) = std::fs::read_to_string(LIFETIME_STATE_PATH) else {
        return (0, 0);
    };
    let mut rx = 0u64;
    let mut tx = 0u64;
    for line in content.lines() {
        if let Some(v) = line.strip_prefix("rx=") {
            rx = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("tx=") {
            tx = v.trim().parse().unwrap_or(0);
        }
    }
    (rx, tx)
}

/// Persist the combined lifetime rx/tx total as plain `key=value` lines.
/// Deliberately not a config-file crate (toml/serde_json) — this repo has
/// none wired up and two `u64`s don't need one.
fn save_lifetime_baseline(rx: u64, tx: u64) {
    if let Some(parent) = std::path::Path::new(LIFETIME_STATE_PATH).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(LIFETIME_STATE_PATH, format!("rx={rx}\ntx={tx}\n"));
}

fn fmt_rate(bytes_per_sec: f64) -> String {
    const UNITS: [&str; 4] = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut v = bytes_per_sec;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Scale a raw byte *count* (not a rate — no `/s` suffix) through
/// B/KB/MB/GB/TB/PB. One tier further than `fmt_rate`'s GB ceiling: a
/// long-running relay's lifetime total can plausibly reach TB/PB.
fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    format!("{v:.1} {}", UNITS[unit])
}

/// Right-align `s` within a fixed-width column by adding leading spaces
/// (never truncates if `s` is already >= `width`). Uses char count, not
/// byte length, since values here contain multi-byte arrow glyphs.
fn right_pad_column(s: String, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        return s;
    }
    format!("{}{}", " ".repeat(width - len), s)
}

fn fmt_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

fn hex8(id: &RoomId) -> String {
    id.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn hex_conn(id: u64) -> String {
    format!("{id:016x}")
}

/// Run the TUI event loop on the *calling* thread until the user quits
/// ('q' or Esc-at-top-level) or `should_stop` returns true. Intended to
/// be invoked via `std::thread::spawn` from `main`, not from a tokio
/// task.
pub fn run(registry: Arc<Registry>) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let result = run_app(&mut terminal, &registry);
    ratatui::restore();
    result
}

fn run_app(terminal: &mut ratatui::DefaultTerminal, registry: &Registry) -> anyhow::Result<()> {
    let mut app = App::new();

    loop {
        let mut snap = take_snapshot(registry);
        app.maybe_sample(&snap);
        // Keep cursor in range as rooms come and go.
        if app.rooms_cursor >= snap.rooms.len() && !snap.rooms.is_empty() {
            app.rooms_cursor = snap.rooms.len() - 1;
        }
        snap.rooms.sort_by_key(|r| r.room_id);

        terminal.draw(|frame| draw(frame, &app, &snap))?;

        if event::poll(REDRAW_INTERVAL)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                match key.code {
                    KeyCode::Char('q') | KeyCode::Char('Q') => {
                        app.save_lifetime(&snap);
                        return Ok(());
                    }
                    KeyCode::Char('c')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
                        app.save_lifetime(&snap);
                        return Ok(());
                    }
                    KeyCode::Left => {
                        app.tab = match app.tab {
                            Tab::Overview => Tab::RoomDetail,
                            Tab::Rooms => Tab::Overview,
                            Tab::RoomDetail => Tab::Rooms,
                        };
                    }
                    KeyCode::Right | KeyCode::Tab => {
                        app.tab = match app.tab {
                            Tab::Overview => Tab::Rooms,
                            Tab::Rooms => Tab::RoomDetail,
                            Tab::RoomDetail => Tab::Overview,
                        };
                    }
                    KeyCode::Down => {
                        if app.tab == Tab::Rooms && !snap.rooms.is_empty() {
                            app.rooms_cursor = (app.rooms_cursor + 1).min(snap.rooms.len() - 1);
                        }
                    }
                    KeyCode::Up => {
                        if app.tab == Tab::Rooms {
                            app.rooms_cursor = app.rooms_cursor.saturating_sub(1);
                        }
                    }
                    KeyCode::Enter => {
                        if app.tab == Tab::Rooms {
                            if let Some(room) = snap.rooms.get(app.rooms_cursor) {
                                app.selected_room = Some(room.room_id);
                                app.tab = Tab::RoomDetail;
                            }
                        }
                    }
                    KeyCode::Esc => {
                        if app.tab == Tab::RoomDetail {
                            app.tab = Tab::Rooms;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw(frame: &mut Frame, app: &App, snap: &Snapshot) {
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(0),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Tabs take the bulk of the top bar; a small right-bound slice shows
    // the persisted lifetime rx/tx total (part of "genuine lifetime
    // counter, top bar, right bound" — `Tabs` has no built-in trailing
    // slot, so this splits the row into two adjacent bordered boxes
    // rather than rendering `Tabs` across the full width as before).
    let top_bar = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(20), Constraint::Length(36)])
        .split(outer[0]);

    let selected = match app.tab {
        Tab::Overview => 0,
        Tab::Rooms => 1,
        Tab::RoomDetail => 2,
    };
    // Shared with the lifetime box below so its numeric values render in
    // the exact same style as the active tab.
    let highlight_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let tabs = Tabs::new(TAB_TITLES.to_vec())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" simple-screensharing relay "),
        )
        .select(selected)
        .highlight_style(highlight_style);
    frame.render_widget(tabs, top_bar[0]);

    let (lifetime_rx, lifetime_tx) = app.lifetime_totals(snap);
    let lifetime_label = "Lifetime";
    // Fixed-width columns (arrow + fmt_bytes output, e.g. "↓784.9 MB" is 9
    // chars) so the rx/tx fields stay aligned as the numbers tick over
    // frame to frame, rather than a single flexible gap that would make
    // only the first gap jitter while the second stayed fixed.
    const VALUE_COL_WIDTH: usize = 9;
    let rx_col = right_pad_column(format!("\u{2193}{}", fmt_bytes(lifetime_rx)), VALUE_COL_WIDTH);
    let tx_col = right_pad_column(format!("\u{2191}{}", fmt_bytes(lifetime_tx)), VALUE_COL_WIDTH);
    let lifetime_line = Line::from(vec![
        Span::raw(" "),
        Span::raw(lifetime_label),
        Span::raw("    "), // fixed gap before the rx column
        Span::styled(rx_col, Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        Span::raw("  "), // fixed gap before the tx column
        Span::styled(tx_col, highlight_style),
        Span::raw(" "),
    ]);
    let lifetime = Paragraph::new(lifetime_line).block(Block::default().borders(Borders::ALL));
    frame.render_widget(lifetime, top_bar[1]);

    match app.tab {
        Tab::Overview => draw_overview(frame, outer[1], app, snap),
        Tab::Rooms => draw_rooms(frame, outer[1], app, snap),
        Tab::RoomDetail => draw_room_detail(frame, outer[1], app, snap),
    }

    let help = match app.tab {
        Tab::Overview => "q: quit   \u{2190}/\u{2192}: switch tab",
        Tab::Rooms => {
            "q: quit   \u{2191}/\u{2193}: select room   Enter: drill in   \u{2190}/\u{2192}: switch tab"
        }
        Tab::RoomDetail => "q: quit   Esc/\u{2190}: back to Rooms   \u{2192}: switch tab",
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        outer[2],
    );
}

/// Fixed width for the rate segment of the "Bandwidth in:"/"Bandwidth out:"
/// lines, sized to comfortably fit the longest plausible `fmt_rate` output
/// ("1023.9 GB/s" is 11 chars, since `fmt_rate` only rolls over to the next
/// unit once the value reaches 1024). Keeps the "(total ..." text that
/// follows starting at the same column on both lines regardless of the
/// rate's actual digit count.
const BANDWIDTH_RATE_COL_WIDTH: usize = 11;

/// Fixed width for the total-value segment inside "(total ...)" on the
/// bandwidth lines, sized to comfortably fit the longest plausible
/// `fmt_bytes` output ("1023.9 PB" is 9 chars). Keeps both lines'
/// parenthetical blocks the same shape as totals grow at different rates.
const BANDWIDTH_TOTAL_COL_WIDTH: usize = 9;

fn draw_overview(frame: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(0)])
        .split(area);

    let lines = vec![
        Line::from(vec![
            Span::raw("Listening on:  "),
            Span::styled(
                snap.listen_addr.to_string(),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::raw("Uptime:        "),
            Span::raw(fmt_duration(snap.uptime)),
        ]),
        Line::from(vec![
            Span::raw("Active rooms:  "),
            Span::raw(snap.room_count.to_string()),
        ]),
        Line::from(vec![
            Span::raw("Bandwidth in:  "),
            Span::styled(
                right_pad_column(fmt_rate(app.rx_rate), BANDWIDTH_RATE_COL_WIDTH),
                Style::default().fg(Color::Green),
            ),
            Span::styled(
                format!(
                    "   (total {})",
                    right_pad_column(fmt_bytes(snap.total_rx), BANDWIDTH_TOTAL_COL_WIDTH)
                ),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::from(vec![
            Span::raw("Bandwidth out: "),
            Span::styled(
                right_pad_column(fmt_rate(app.tx_rate), BANDWIDTH_RATE_COL_WIDTH),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                format!(
                    "   (total {})",
                    right_pad_column(fmt_bytes(snap.total_tx), BANDWIDTH_TOTAL_COL_WIDTH)
                ),
                Style::default().fg(Color::Yellow),
            ),
        ]),
    ];
    let block = Block::default().borders(Borders::ALL).title(" Overview ");
    frame.render_widget(Paragraph::new(lines).block(block), rows[0]);

    let lower = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Percentage(65)])
        .split(rows[1]);
    draw_room_tree(frame, lower[0], snap);
    draw_network_graph(frame, lower[1], app);
}

/// Small all-rooms-at-once tree: every active room with its current
/// members indented underneath, glanceable without drilling in (unlike
/// the Rooms tab, which only shows the currently-selected room's
/// participants).
fn draw_room_tree(frame: &mut Frame, area: Rect, snap: &Snapshot) {
    let mut items: Vec<ListItem> = Vec::new();
    if snap.rooms.is_empty() {
        items.push(ListItem::new(Span::styled(
            "(no active rooms)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for room in &snap.rooms {
            items.push(ListItem::new(Line::from(Span::styled(
                format!("\u{25a0} {}", hex8(&room.room_id)),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ))));
            if room.members.is_empty() {
                items.push(ListItem::new(Line::from(Span::styled(
                    "  \u{2514}\u{2500} (empty)",
                    Style::default().fg(Color::DarkGray),
                ))));
                continue;
            }
            let last = room.members.len() - 1;
            for (i, m) in room.members.iter().enumerate() {
                let branch = if i == last { "\u{2514}\u{2500}" } else { "\u{251c}\u{2500}" };
                let mut spans = vec![
                    Span::raw(format!("  {branch} ")),
                    Span::raw(format!("{:<16}", m.display_name)),
                ];
                if m.stream_count == 0 && m.watching_count == 0 {
                    spans.push(Span::styled(
                        "\u{25cb} idle",
                        Style::default().fg(Color::DarkGray),
                    ));
                } else {
                    if m.stream_count > 0 {
                        spans.push(Span::styled(
                            format!("\u{25cf} streaming x{}", m.stream_count),
                            Style::default().fg(Color::Green),
                        ));
                    }
                    if m.watching_count > 0 {
                        if m.stream_count > 0 {
                            spans.push(Span::raw(" \u{00b7} "));
                        }
                        spans.push(Span::styled(
                            format!("\u{25d1} watching x{}", m.watching_count),
                            Style::default().fg(Color::Cyan),
                        ));
                    }
                }
                items.push(ListItem::new(Line::from(spans)));
            }
        }
    }
    let list = List::new(items).block(Block::default().borders(Borders::ALL).title(" Tree "));
    frame.render_widget(list, area);
}

/// Live rx/tx bandwidth-rate history, filling the space to the right of
/// the room tree. Sourced from `App::rx_history`/`tx_history`, pushed
/// once per `maybe_sample` tick and capped at `HISTORY_LEN` points.
fn draw_network_graph(frame: &mut Frame, area: Rect, app: &App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Network Usage ");
    if app.rx_history.is_empty() && app.tx_history.is_empty() {
        frame.render_widget(
            Paragraph::new("(gathering samples\u{2026})")
                .style(Style::default().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    }

    let rx_points: Vec<(f64, f64)> = app
        .rx_history
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v))
        .collect();
    let tx_points: Vec<(f64, f64)> = app
        .tx_history
        .iter()
        .enumerate()
        .map(|(i, v)| (i as f64, *v))
        .collect();

    let max_len = app.rx_history.len().max(app.tx_history.len()).max(2);
    let max_val = app
        .rx_history
        .iter()
        .chain(app.tx_history.iter())
        .cloned()
        .fold(0.0_f64, f64::max);
    // A little headroom above the peak so the line doesn't hug the top
    // border, and a floor so an idle relay (all-zero history) still
    // renders a sane, non-degenerate axis.
    let y_max = (max_val * 1.2).max(1024.0);

    let datasets = vec![
        Dataset::default()
            // Padded with a leading/trailing space so the legend box (which
            // hugs the name tightly by default) doesn't read as cramped.
            .name(" in ")
            .marker(ratatui::symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Green))
            .data(&rx_points),
        Dataset::default()
            .name(" out ")
            .marker(ratatui::symbols::Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::default().fg(Color::Yellow))
            .data(&tx_points),
    ];

    let chart = Chart::new(datasets)
        .block(block)
        .x_axis(Axis::default().bounds([0.0, (max_len - 1) as f64]))
        .y_axis(
            Axis::default()
                .bounds([0.0, y_max])
                .labels([format!("{} ", fmt_rate(0.0)), format!("{} ", fmt_rate(y_max))])
                // Each label renders left-aligned in its own fixed-width
                // gutter by default, so "0.0 B/s" and (e.g.) "12.3 MB/s"
                // land ragged against the chart's left border as their
                // digit-count changes. Right-aligning flushes both to the
                // border consistently — same intent as right_pad_column,
                // but via ratatui's native per-axis alignment knob since
                // Axis labels aren't raw strings we can pre-pad ourselves.
                // A trailing space on each label leaves a one-character
                // margin between the text and the border, matching the
                // buffering already applied to the lifetime bar and the
                // bandwidth in/out lines elsewhere in this TUI.
                .labels_alignment(Alignment::Right),
        );
    frame.render_widget(chart, area);
}

fn draw_rooms(frame: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    let header = Row::new(vec![
        Cell::from("Room"),
        Cell::from("Members"),
        Cell::from("Streams"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let rows: Vec<Row> = snap
        .rooms
        .iter()
        .map(|r| {
            let sharing = r.members.iter().filter(|m| m.stream_count > 0).count();
            Row::new(vec![
                Cell::from(hex8(&r.room_id)),
                Cell::from(format!("{} ({} sharing)", r.members.len(), sharing)),
                Cell::from(r.streams.len().to_string()),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(18),
        Constraint::Length(20),
        Constraint::Length(10),
    ];
    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Rooms (Enter to drill in) "),
        )
        .row_highlight_style(Style::default().fg(Color::Black).bg(Color::Yellow));
    let mut state = ratatui::widgets::TableState::default();
    if !snap.rooms.is_empty() {
        state.select(Some(app.rooms_cursor));
    }
    frame.render_stateful_widget(&table, cols[0], &mut state);

    let items: Vec<ListItem> = match snap.rooms.get(app.rooms_cursor) {
        Some(room) => room
            .members
            .iter()
            .map(|m| {
                let mut spans = vec![Span::raw(format!("{:<20}", m.display_name))];
                if m.stream_count == 0 && m.watching_count == 0 {
                    spans.push(Span::styled(
                        "\u{25cb} idle",
                        Style::default().fg(Color::DarkGray),
                    ));
                } else {
                    if m.stream_count > 0 {
                        spans.push(Span::styled(
                            format!("\u{25cf} streaming x{}", m.stream_count),
                            Style::default().fg(Color::Green),
                        ));
                    }
                    if m.watching_count > 0 {
                        if m.stream_count > 0 {
                            spans.push(Span::raw(" \u{00b7} "));
                        }
                        spans.push(Span::styled(
                            format!("\u{25d1} watching x{}", m.watching_count),
                            Style::default().fg(Color::Cyan),
                        ));
                    }
                }
                ListItem::new(Line::from(spans))
            })
            .collect(),
        None => vec![ListItem::new("(no rooms)")],
    };
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Participants "),
    );
    frame.render_widget(list, cols[1]);
}

fn draw_room_detail(frame: &mut Frame, area: Rect, app: &App, snap: &Snapshot) {
    let Some(room) = app.selected_room_snapshot(snap) else {
        frame.render_widget(
            Paragraph::new("No room selected. Go to the Rooms tab and press Enter on one.").block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Room Detail "),
            ),
            area,
        );
        return;
    };

    let rows_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Percentage(45),
            Constraint::Min(0),
        ])
        .split(area);

    frame.render_widget(
        Paragraph::new(format!(
            "Room {}  ({} members)",
            hex8(&room.room_id),
            room.members.len()
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Room Detail "),
        ),
        rows_layout[0],
    );

    let header = Row::new(vec![
        Cell::from("Owner"),
        Cell::from("Stream"),
        Cell::from("Label"),
        Cell::from("Subs"),
        Cell::from("Rate"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));
    let stream_rows: Vec<Row> = room
        .streams
        .iter()
        .map(|s| {
            let rate = app
                .per_stream_rate
                .get(&(s.owner, s.stream_id))
                .copied()
                .unwrap_or(0.0);
            Row::new(vec![
                Cell::from(hex_conn(s.owner)),
                Cell::from(s.stream_id.to_string()),
                Cell::from(s.label.clone()),
                Cell::from(s.subscriber_count.to_string()),
                // Right-align within the fixed-width "Rate" column so the
                // unit suffix lines up across rows of differing magnitude
                // (e.g. "4.9 MB/s" vs "12.3 MB/s") — same intent as
                // right_pad_column, expressed via Text::alignment since a
                // Table column already reserves fixed cell width.
                Cell::from(Line::from(fmt_rate(rate)).alignment(Alignment::Right)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(18),
        Constraint::Length(8),
        Constraint::Length(16),
        Constraint::Length(6),
        Constraint::Length(12),
    ];
    let table = Table::new(stream_rows, widths)
        .header(header)
        .block(Block::default().borders(Borders::ALL).title(" Streams "));
    frame.render_widget(table, rows_layout[1]);

    let log_items: Vec<ListItem> = if room.log_tail.is_empty() {
        vec![ListItem::new("(no events yet)")]
    } else {
        room.log_tail
            .iter()
            .map(|s| ListItem::new(s.as_str()))
            .collect()
    };
    let log_list = List::new(log_items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Event log (most recent first) "),
    );
    frame.render_widget(log_list, rows_layout[2]);
}
