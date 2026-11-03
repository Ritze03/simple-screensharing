//! Storage for the diagnostics log lines clients upload when their user ticked "Send diagnostics
//! to the relay" (spec §6.7, task #110).
//!
//! Layout, relative to the relay's working directory:
//! `logs/clients/<YYYY-MM-DD>/<display-name>-<conn_id>.log`, one file per connection, appended to
//! as batches arrive, `# relay:` lines marking start and end. The day is the UTC day the
//! connection's first batch arrived (a session that crosses midnight stays in one file).
//!
//! Why on the relay and per person: the owner investigates friends' stalls and used to ask each
//! one to "launch it this way and send me the file"; the relay is the one place that is always
//! reachable. Stored with the display name (not anonymised) because the upload is opt-in and
//! everyone knows it; never room keys (the client strips join-link-shaped text before a line is
//! even buffered).
//!
//! Bounds, because the relay box is a small LXC and anyone in a room can open the stream:
//! [`MAX_FILE_BYTES`] per file, [`MAX_DAY_BYTES`] per day directory, directories older than
//! [`RETENTION_DAYS`] deleted (at start and then hourly). A batch that does not fit is dropped
//! whole (a `# relay:` note says so once per file); the client is not told, it just keeps
//! sending, which costs one rejected stream per 30 s.
//!
//! All file work runs under `spawn_blocking` and one `std::sync::Mutex` that is held only
//! around the (small, <= 256 KiB) append itself, never across an `.await`.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::{Date, Month, OffsetDateTime};

use crate::protocol::ConnId;

/// Where uploads land, relative to the working directory (next to the relay's own `logs/`).
pub const CLIENT_LOG_DIR: &str = "logs/clients";
/// One connection's file stops growing here.
pub const MAX_FILE_BYTES: u64 = 20 * 1024 * 1024;
/// One day directory stops growing here.
pub const MAX_DAY_BYTES: u64 = 500 * 1024 * 1024;
/// Day directories older than this many days are deleted.
pub const RETENTION_DAYS: i64 = 14;
/// How often old directories are looked for.
pub const PRUNE_EVERY: Duration = Duration::from_secs(3600);
/// Longest display-name part of a file name, in characters.
const MAX_NAME_CHARS: usize = 32;

/// The caps; a struct so tests can use tiny ones.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub file_bytes: u64,
    pub day_bytes: u64,
    pub retention_days: i64,
}

impl Default for Limits {
    fn default() -> Self {
        Self { file_bytes: MAX_FILE_BYTES, day_bytes: MAX_DAY_BYTES, retention_days: RETENTION_DAYS }
    }
}

/// Why a batch was not stored.
#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    FileFull,
    DayFull,
}

/// One connection's file.
struct Session {
    path: PathBuf,
    day: String,
    written: u64,
    /// The "limit reached" note has been written.
    noted_full: bool,
}

#[derive(Default)]
struct State {
    sessions: HashMap<ConnId, Session>,
    /// Bytes stored per day directory; summed from disk the first time a day is touched.
    days: HashMap<String, u64>,
}

pub struct ClientLogs {
    root: PathBuf,
    /// `SSSHARE_DIAG_UPLOAD=0` turns the whole feature off: no `DiagAck`, nothing stored.
    pub enabled: bool,
    limits: Limits,
    state: Arc<Mutex<State>>,
}

impl ClientLogs {
    pub fn new(root: impl Into<PathBuf>, enabled: bool, limits: Limits) -> Self {
        Self { root: root.into(), enabled, limits, state: Arc::new(Mutex::new(State::default())) }
    }

    /// The relay's real store: `logs/clients`, on unless `SSSHARE_DIAG_UPLOAD=0`.
    pub fn from_env() -> Self {
        let enabled = std::env::var("SSSHARE_DIAG_UPLOAD").map(|v| v != "0").unwrap_or(true);
        Self::new(CLIENT_LOG_DIR, enabled, Limits::default())
    }

    /// Append one uploaded batch to `conn_id`'s file (created on first use). Runs on the
    /// blocking pool.
    pub async fn append(&self, conn_id: ConnId, name: &str, text: Vec<u8>) -> io::Result<Result<PathBuf, Rejected>> {
        let (root, limits, state, name) = (self.root.clone(), self.limits, self.state.clone(), name.to_owned());
        tokio::task::spawn_blocking(move || append_at(&root, &limits, &state, today(), conn_id, &name, &text))
            .await
            .map_err(io::Error::other)?
    }

    /// The connection ended: note it in its file (if it has one) and forget its state.
    pub async fn end(&self, conn_id: ConnId) {
        let (root, state) = (self.root.clone(), self.state.clone());
        let _ = tokio::task::spawn_blocking(move || end_at(&root, &state, conn_id)).await;
    }

    /// Delete day directories past retention.
    pub async fn prune(&self) {
        let (root, limits, state) = (self.root.clone(), self.limits, self.state.clone());
        let _ = tokio::task::spawn_blocking(move || prune_at(&root, &limits, &state, today())).await;
    }
}

/// Prune now and then, for the life of the relay.
pub async fn maintain(registry: Arc<crate::room::Registry>) {
    loop {
        registry.client_logs.prune().await;
        tokio::time::sleep(PRUNE_EVERY).await;
    }
}

fn today() -> Date {
    OffsetDateTime::now_utc().date()
}

fn day_name(d: Date) -> String {
    format!("{:04}-{:02}-{:02}", d.year(), d.month() as u8, d.day())
}

fn parse_day(s: &str) -> Option<Date> {
    let mut it = s.split('-');
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || y.len() != 4 || m.len() != 2 || d.len() != 2 {
        return None;
    }
    Date::from_calendar_date(y.parse().ok()?, Month::try_from(m.parse::<u8>().ok()?).ok()?, d.parse().ok()?).ok()
}

/// A display name as a file-name part: letters, digits, `-` and `_` stay, everything else
/// (path separators, dots, spaces, control characters) becomes `_`; at most 32 characters;
/// never empty. Not unique (two people may be called "mo"): the connection id follows it.
pub fn sanitize_name(name: &str) -> String {
    let s: String = name
        .trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if s.chars().all(|c| c == '_') { "anon".to_owned() } else { s }
}

/// Uploaded text as it is stored: invalid UTF-8 replaced, control characters other than newline
/// and tab replaced by `?` (the owner reads these files in a terminal; an escape sequence from
/// a client must not drive it), always ending in a newline.
pub fn clean_text(bytes: &[u8]) -> String {
    let mut s: String = String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| if c.is_control() && c != '\n' && c != '\t' { '?' } else { c })
        .collect();
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

fn now_stamp() -> String {
    let t = OffsetDateTime::now_utc();
    format!(
        "{} {:02}:{:02}:{:02}Z",
        day_name(t.date()),
        t.hour(),
        t.minute(),
        t.second()
    )
}

fn dir_size(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()?.metadata().ok()).filter(|m| m.is_file()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

fn append_at(
    root: &Path,
    limits: &Limits,
    state: &Mutex<State>,
    today: Date,
    conn_id: ConnId,
    name: &str,
    text: &[u8],
) -> io::Result<Result<PathBuf, Rejected>> {
    let mut text = clean_text(text);
    let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
    let st = &mut *st;
    let is_new = !st.sessions.contains_key(&conn_id);
    let session = st.sessions.entry(conn_id).or_insert_with(|| {
        let day = day_name(today);
        let path = root.join(&day).join(format!("{}-{conn_id:016x}.log", sanitize_name(name)));
        Session { path, day, written: 0, noted_full: false }
    });
    if is_new {
        let mut head = String::new();
        let _ = writeln!(head, "# relay: client diagnostics from \"{}\" (connection {conn_id:016x}) started {}", clean_text(name.as_bytes()).trim_end(), now_stamp());
        text.insert_str(0, &head);
    }
    let day_total = *st.days.entry(session.day.clone()).or_insert_with(|| dir_size(&root.join(&session.day)));
    let len = text.len() as u64;
    let verdict = if session.written + len > limits.file_bytes {
        Some(Rejected::FileFull)
    } else if day_total + len > limits.day_bytes {
        Some(Rejected::DayFull)
    } else {
        None
    };
    if let Some(why) = verdict {
        if !std::mem::replace(&mut session.noted_full, true) {
            let note = format!("# relay: {why:?}: later uploads of this connection are dropped ({})\n", now_stamp());
            // The note is outside the caps (one short line per file).
            write_to(&session.path, note.as_bytes())?;
        }
        if is_new && why == Rejected::DayFull {
            // Nothing was written: do not keep a session that has no file.
            st.sessions.remove(&conn_id);
        }
        return Ok(Err(why));
    }
    write_to(&session.path, text.as_bytes())?;
    session.written += len;
    *st.days.entry(session.day.clone()).or_default() += len;
    Ok(Ok(session.path.clone()))
}

fn write_to(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    OpenOptions::new().create(true).append(true).open(path)?.write_all(bytes)
}

fn end_at(_root: &Path, state: &Mutex<State>, conn_id: ConnId) {
    let session = state.lock().unwrap_or_else(|p| p.into_inner()).sessions.remove(&conn_id);
    if let Some(s) = session {
        let _ = write_to(&s.path, format!("# relay: connection closed {}\n", now_stamp()).as_bytes());
    }
}

fn prune_at(root: &Path, limits: &Limits, state: &Mutex<State>, today: Date) {
    let Ok(rd) = std::fs::read_dir(root) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(day) = parse_day(&name) else { continue };
        if (today - day).whole_days() > limits.retention_days
            && entry.path().is_dir()
            && std::fs::remove_dir_all(entry.path()).is_ok()
        {
            tracing::info!("client diagnostics: deleted {name} (older than {} days)", limits.retention_days);
            state.lock().unwrap_or_else(|p| p.into_inner()).days.remove(&name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sss-clientlogs-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn date(y: i32, m: u8, d: u8) -> Date {
        Date::from_calendar_date(y, Month::try_from(m).unwrap(), d).unwrap()
    }

    fn put(root: &Path, limits: &Limits, state: &Mutex<State>, day: Date, id: ConnId, name: &str, text: &str) -> Result<PathBuf, Rejected> {
        append_at(root, limits, state, day, id, name, text.as_bytes()).unwrap()
    }

    #[test]
    fn names_become_safe_file_name_parts() {
        assert_eq!(sanitize_name("mo"), "mo");
        assert_eq!(sanitize_name("Mo Müller-2"), "Mo_Müller-2");
        assert_eq!(sanitize_name("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize_name("a/b\\c.d\ne\0f"), "a_b_c_d_e_f");
        assert_eq!(sanitize_name(""), "anon");
        assert_eq!(sanitize_name("   "), "anon");
        assert_eq!(sanitize_name("..."), "anon");
        assert_eq!(sanitize_name(&"x".repeat(100)).chars().count(), MAX_NAME_CHARS);
        assert!(!sanitize_name("../x").contains(['/', '.']));
    }

    #[test]
    fn uploaded_text_has_no_control_characters_and_ends_in_a_newline() {
        assert_eq!(clean_text(b"a\nb\tc"), "a\nb\tc\n");
        assert_eq!(clean_text(b"\x1b[31mred\x1b[0m\r\n"), "?[31mred?[0m?\n");
        assert_eq!(clean_text(&[b'o', b'k', 0xff, b'\n']), "ok\u{fffd}\n");
        assert_eq!(clean_text(b""), "");
    }

    #[test]
    fn batches_append_to_one_file_per_connection_under_the_day() {
        let root = tmp("append");
        let state = Mutex::new(State::default());
        let l = Limits::default();
        let day = date(2026, 11, 3);
        let p1 = put(&root, &l, &state, day, 0xAB, "Mo Müller", "line 1\nline 2\n").unwrap();
        // A later batch on another day still goes to the same file.
        let p2 = put(&root, &l, &state, date(2026, 11, 4), 0xAB, "Mo Müller", "line 3\n").unwrap();
        assert_eq!(p1, p2);
        assert_eq!(p1, root.join("2026-11-03").join("Mo_Müller-00000000000000ab.log"));
        // Another connection of the same person has its own file.
        let p3 = put(&root, &l, &state, day, 0xCD, "Mo Müller", "other\n").unwrap();
        assert_ne!(p1, p3);
        end_at(&root, &state, 0xAB);
        let text = std::fs::read_to_string(&p1).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines[0].starts_with("# relay: client diagnostics from \"Mo Müller\" (connection 00000000000000ab) started 20"), "{text}");
        assert_eq!(&lines[1..4], ["line 1", "line 2", "line 3"]);
        assert!(lines[4].starts_with("# relay: connection closed "), "{text}");
        assert!(state.lock().unwrap().sessions.contains_key(&0xCD) && !state.lock().unwrap().sessions.contains_key(&0xAB));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_stops_at_its_cap_and_says_so_once() {
        let root = tmp("filecap");
        let state = Mutex::new(State::default());
        let l = Limits { file_bytes: 450, ..Limits::default() };
        let day = date(2026, 11, 3);
        let big = "x".repeat(150) + "\n";
        assert!(put(&root, &l, &state, day, 1, "a", &big).is_ok());
        assert!(put(&root, &l, &state, day, 1, "a", &big).is_ok());
        assert_eq!(put(&root, &l, &state, day, 1, "a", &big), Err(Rejected::FileFull));
        assert_eq!(put(&root, &l, &state, day, 1, "a", &big), Err(Rejected::FileFull));
        // A small batch that still fits is accepted.
        assert!(put(&root, &l, &state, day, 1, "a", "ok\n").is_ok());
        let path = root.join("2026-11-03").join("a-0000000000000001.log");
        let text = std::fs::read_to_string(path).unwrap();
        assert_eq!(text.matches("FileFull").count(), 1, "{text}");
        assert!(text.ends_with("ok\n"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_day_stops_at_its_cap_across_connections_and_counts_what_is_on_disk() {
        let root = tmp("daycap");
        let day = date(2026, 11, 3);
        let l = Limits { day_bytes: 700, ..Limits::default() };
        let body = "y".repeat(250) + "\n";
        {
            let state = Mutex::new(State::default());
            assert!(put(&root, &l, &state, day, 1, "a", &body).is_ok());
            assert!(put(&root, &l, &state, day, 2, "b", &body).is_ok());
            assert_eq!(put(&root, &l, &state, day, 3, "c", &body), Err(Rejected::DayFull));
            // The refused connection has no file but the note, and no lingering session.
            assert!(!state.lock().unwrap().sessions.contains_key(&3));
            // Another day has its own budget.
            assert!(put(&root, &l, &state, date(2026, 11, 4), 4, "d", &body).is_ok());
        }
        // A "restarted relay" (fresh state) counts the day's files already on disk.
        let state = Mutex::new(State::default());
        assert_eq!(put(&root, &l, &state, day, 5, "e", &body), Err(Rejected::DayFull));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn old_day_directories_are_deleted_and_nothing_else() {
        let root = tmp("prune");
        let state = Mutex::new(State::default());
        let l = Limits { retention_days: 14, ..Limits::default() };
        let today = date(2026, 11, 20);
        for d in ["2026-11-05", "2026-11-06", "2026-11-19", "2026-11-20", "2026-12-01"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
            std::fs::write(root.join(d).join("x.log"), "x").unwrap();
        }
        std::fs::create_dir_all(root.join("not-a-date")).unwrap();
        std::fs::write(root.join("2026-11-01"), "a file named like a day").unwrap();
        prune_at(&root, &l, &state, today);
        let mut left: Vec<String> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        // 11-05 is 15 days old: gone. 11-06 is 14 days: kept. The plain file and odd name stay.
        assert_eq!(left, ["2026-11-01", "2026-11-06", "2026-11-19", "2026-11-20", "2026-12-01", "not-a-date"]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn day_names_round_trip() {
        assert_eq!(day_name(date(2026, 1, 9)), "2026-01-09");
        assert_eq!(parse_day("2026-01-09"), Some(date(2026, 1, 9)));
        assert_eq!(parse_day("2026-13-09"), None);
        assert_eq!(parse_day("2026-1-9"), None);
        assert_eq!(parse_day("relay.log"), None);
    }
}
