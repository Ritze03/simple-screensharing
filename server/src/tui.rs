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

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, Paragraph, Row, Table, Tabs};

use crate::protocol::RoomId;
use crate::room::Registry;

const TAB_TITLES: [&str; 3] = ["Overview", "Rooms", "Room Detail"];
const REDRAW_INTERVAL: Duration = Duration::from_millis(200);
const SAMPLE_INTERVAL: Duration = Duration::from_secs(1);

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
                bytes_received: s.bytes_received.load(Ordering::Relaxed),
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
}

impl App {
    fn new() -> Self {
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
    }

    fn selected_room_snapshot<'a>(&self, snap: &'a Snapshot) -> Option<&'a RoomSnapshot> {
        let id = self.selected_room?;
        snap.rooms.iter().find(|r| r.room_id == id)
    }
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
                    KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(()),
                    KeyCode::Char('c')
                        if key
                            .modifiers
                            .contains(crossterm::event::KeyModifiers::CONTROL) =>
                    {
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

    let selected = match app.tab {
        Tab::Overview => 0,
        Tab::Rooms => 1,
        Tab::RoomDetail => 2,
    };
    let tabs = Tabs::new(TAB_TITLES.to_vec())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" simple-screensharing relay "),
        )
        .select(selected)
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    frame.render_widget(tabs, outer[0]);

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
            Span::styled(fmt_rate(app.rx_rate), Style::default().fg(Color::Green)),
            Span::raw(format!("   (total {} bytes)", snap.total_rx)),
        ]),
        Line::from(vec![
            Span::raw("Bandwidth out: "),
            Span::styled(fmt_rate(app.tx_rate), Style::default().fg(Color::Green)),
            Span::raw(format!("   (total {} bytes)", snap.total_tx)),
        ]),
    ];
    let block = Block::default().borders(Borders::ALL).title(" Overview ");
    frame.render_widget(Paragraph::new(lines).block(block), rows[0]);
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
                let indicator = if m.stream_count > 0 {
                    format!("\u{25cf} streaming x{}", m.stream_count)
                } else {
                    "\u{25cb} idle".to_string()
                };
                let color = if m.stream_count > 0 {
                    Color::Green
                } else {
                    Color::DarkGray
                };
                ListItem::new(Line::from(vec![
                    Span::raw(format!("{:<20}", m.display_name)),
                    Span::styled(indicator, Style::default().fg(color)),
                ]))
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
                Cell::from(fmt_rate(rate)),
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
