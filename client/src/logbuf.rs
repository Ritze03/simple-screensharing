//! The app's own recent log, kept in memory so Settings can copy or save it ("Copy logs to
//! clipboard" / "Save logs to file…", task #107).
//!
//! A second `fmt` layer on the tracing subscriber (installed by `main.rs` `init_logging`)
//! writes every event it lets through into a bounded ring. It filters on its own, independent
//! of what goes to stderr (which keeps honouring `RUST_LOG`): INFO and above from our crates,
//! WARN and above from everything else. The lines carry the same timestamps (UTC) and layout as
//! stderr.
//!
//! Why: friends hitting stalls or stutter used to be asked to restart the app from a terminal
//! with `RUST_LOG=info ... 2> sss.log`, and the evidence of the first occurrence was gone by
//! then. User request, 2026-11-03: "The GUI should have an option in the settings to dump logs
//! to the clipboard or a file, so investigation is easier."
//!
//! No secrets: no log line prints a room key or a join link today (checked 2026-11-03), and
//! [`redact`] still strips the key and what follows from anything shaped like one
//! (`server|room|key…`) before a line enters the ring.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;
use std::sync::Mutex;

use tracing::Level;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{self, MakeWriter};
use tracing_subscriber::registry::LookupSpan;

/// Most events kept.
const MAX_LINES: usize = 5000;
/// Most bytes kept (the oldest lines go first). Whichever bound is hit first applies.
const MAX_BYTES: usize = 2 << 20;

/// A line-and-byte bounded FIFO of log lines.
struct Ring {
    lines: VecDeque<String>,
    bytes: usize,
    max_lines: usize,
    max_bytes: usize,
    /// Sequence number of `lines[0]`: every line ever pushed gets the next number, so an
    /// uploader can remember "everything below N is sent" across evictions (task #110).
    first_seq: u64,
    /// Sequence number of the newest WARN/ERROR line (the uploader sends early after one).
    last_warn_seq: Option<u64>,
}

/// What of the ring a reader that has seen everything below some cursor still has to send.
#[derive(Debug, PartialEq, Eq)]
struct Pending {
    /// Index into the ring's lines of the first line to send.
    start: usize,
    /// Wanted lines that are gone (evicted from the ring, or dropped to fit the byte budget).
    skipped: u64,
    /// The cursor to continue from once these lines are sent.
    next: u64,
}

impl Ring {
    const fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self { lines: VecDeque::new(), bytes: 0, max_lines, max_bytes, first_seq: 0, last_warn_seq: None }
    }

    fn next_seq(&self) -> u64 {
        self.first_seq + self.lines.len() as u64
    }

    fn push(&mut self, line: String) {
        if is_warn_or_error(&line) {
            self.last_warn_seq = Some(self.next_seq());
        }
        self.bytes += line.len();
        self.lines.push_back(line);
        // Always keeps the newest line, even one larger than `max_bytes`.
        while self.lines.len() > self.max_lines || (self.bytes > self.max_bytes && self.lines.len() > 1) {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len();
                self.first_seq += 1;
            }
        }
    }

    /// The lines at or after `cursor` that fit `budget` bytes (each counted with its newline),
    /// newest preferred: when they do not all fit the oldest are skipped. The newest line is
    /// always kept.
    fn pending(&self, cursor: u64, budget: usize) -> Pending {
        let next = self.next_seq();
        let from = cursor.clamp(self.first_seq, next);
        let mut start = (from - self.first_seq) as usize;
        let mut size: usize = self.lines.iter().skip(start).map(|l| l.len() + 1).sum();
        while size > budget && start + 1 < self.lines.len() {
            size -= self.lines[start].len() + 1;
            start += 1;
        }
        let kept_from = self.first_seq + start as u64;
        Pending { start, skipped: kept_from.saturating_sub(cursor.min(next)), next }
    }
}

/// Whether a formatted line is at WARN or ERROR level (`<time> WARN target: message`).
fn is_warn_or_error(line: &str) -> bool {
    matches!(line.split_whitespace().nth(1), Some("WARN" | "ERROR"))
}

/// Tests that touch the process-wide ring or the upload switch and then reason about them take
/// this, so they do not see each other's changes (the `diag_upload` tests here, and a settings
/// test in the binary crate, hence `pub`).
#[doc(hidden)]
pub static TEST_LOCK: Mutex<()> = Mutex::new(());

static RING: Mutex<Ring> = Mutex::new(Ring::new(MAX_LINES, MAX_BYTES));
/// "key: value" facts for the dump header that only some code knows (GPU, VA driver).
static FACTS: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());

/// Record (or replace) a fact for the header of [`dump`].
pub fn fact(key: &'static str, value: impl Into<String>) {
    let value = value.into();
    let Ok(mut facts) = FACTS.lock() else { return };
    match facts.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => facts.push((key, value)),
    }
}

/// Strip the key (and certhash, name) from anything shaped like a join link
/// (`server|room-id|key[|…]`): a whitespace/punctuation-delimited token with two or more `|`.
fn redact(line: &str) -> Cow<'_, str> {
    if line.matches('|').count() < 2 {
        return Cow::Borrowed(line);
    }
    let delim = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']' | ',' | ';');
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while !rest.is_empty() {
        let end = rest.find(delim).unwrap_or(rest.len());
        let (tok, tail) = rest.split_at(end);
        let mut parts = tok.splitn(3, '|');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(server), Some(room), Some(_)) => {
                out.push_str(server);
                out.push('|');
                out.push_str(room);
                out.push_str("|<redacted>");
            }
            _ => out.push_str(tok),
        }
        let sep = tail.chars().next().map_or(0, char::len_utf8);
        out.push_str(&tail[..sep]);
        rest = &tail[sep..];
    }
    Cow::Owned(out)
}

/// One event's text: the fmt layer writes it, `Drop` files it (after the whole event is in).
struct LineWriter(Vec<u8>);

impl io::Write for LineWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for LineWriter {
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.0);
        let line = redact(text.trim_end());
        if line.is_empty() {
            return;
        }
        if let Ok(mut ring) = RING.lock() {
            ring.push(line.into_owned());
        }
    }
}

struct RingWriter;

impl<'a> MakeWriter<'a> for RingWriter {
    type Writer = LineWriter;
    fn make_writer(&'a self) -> LineWriter {
        LineWriter(Vec::new())
    }
}

/// The tracing layer that feeds the ring.
pub fn layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    let filter = Targets::new()
        .with_target("simple_screensharing_client", Level::INFO)
        .with_target("simple_screensharing_protocol", Level::INFO)
        .with_default(Level::WARN);
    fmt::layer().with_ansi(false).with_writer(RingWriter).with_filter(filter)
}

/// Now, as the log lines write it (`2026-11-03T12:34:56.789012Z`, UTC).
pub fn now_utc() -> String {
    let mut s = String::new();
    let _ = SystemTime.format_time(&mut Writer::new(&mut s));
    s
}

/// `YYYY-MM-DD_HH-MM-SS` (UTC), for file names.
pub fn file_stamp() -> String {
    now_utc()
        .chars()
        .take(19)
        .map(|c| match c {
            'T' => '_',
            ':' => '-',
            c => c,
        })
        .collect()
}

fn os_description() -> String {
    #[cfg(target_os = "linux")]
    if let Ok(text) = std::fs::read_to_string("/etc/os-release")
        && let Some(name) = text.lines().find_map(|l| l.strip_prefix("PRETTY_NAME="))
    {
        return format!("{} ({})", name.trim_matches('"'), std::env::consts::ARCH);
    }
    format!("{} ({})", std::env::consts::OS, std::env::consts::ARCH)
}

/// The version / OS / facts block both a dump and an upload start with.
fn header_top() -> String {
    let commit = env!("SSS_GIT_COMMIT");
    let mut h = format!("Simple Screensharing client log\nversion: {}", env!("CARGO_PKG_VERSION"));
    if !commit.is_empty() {
        h.push_str(&format!(" (commit {commit})"));
    }
    h.push_str(&format!("\nos: {}\n", os_description()));
    if let Ok(facts) = FACTS.lock() {
        for (k, v) in facts.iter() {
            h.push_str(&format!("{k}: {v}\n"));
        }
    }
    h
}

const HEADER_RULE: &str = "----------------------------------------\n";

/// What a dump starts with.
fn header(lines: usize) -> String {
    let mut h = header_top();
    h.push_str(&format!(
        "dumped: {}\nlines: {lines} (the most recent; INFO+ from the app, WARN+ from libraries; times are UTC)\n{HEADER_RULE}",
        now_utc()
    ));
    h
}

// -------------------------------------------------------------------------------------------
// Upload to the relay (task #110, `diag_upload.rs`)
// -------------------------------------------------------------------------------------------

/// Where the next line to be logged will be numbered: the cursor an uploader starts from to
/// send only what is logged from now on.
pub fn next_seq() -> u64 {
    RING.lock().map(|r| r.next_seq()).unwrap_or(0)
}

/// Whether a WARN/ERROR line at or after `cursor` is waiting.
pub fn warn_since(cursor: u64) -> bool {
    RING.lock().is_ok_and(|r| r.last_warn_seq.is_some_and(|s| s >= cursor))
}

/// The header of an upload session: like a dump's, minus the line count that does not apply.
pub fn upload_header() -> String {
    let mut h = header_top();
    h.push_str(&format!(
        "session started: {}\ntimes are UTC; INFO+ from the app, WARN+ from libraries\n{HEADER_RULE}",
        now_utc()
    ));
    h
}

/// Result of [`lines_since`].
#[derive(Debug, Default)]
pub struct LineBatch {
    /// The lines, each newline-terminated.
    pub text: String,
    pub lines: usize,
    /// Wanted lines that are missing (evicted from the ring, or over the byte budget).
    pub skipped: u64,
    /// The cursor to continue from after sending this batch.
    pub next: u64,
}

/// The log lines with sequence number >= `cursor`, at most `budget` bytes of them (the newest
/// win).
pub fn lines_since(cursor: u64, budget: usize) -> LineBatch {
    let Ok(ring) = RING.lock() else { return LineBatch::default() };
    let p = ring.pending(cursor, budget);
    let mut text = String::new();
    let mut lines = 0;
    for l in ring.lines.iter().skip(p.start) {
        text.push_str(l);
        text.push('\n');
        lines += 1;
    }
    LineBatch { text, lines, skipped: p.skipped, next: p.next }
}

/// The header and the kept log lines, oldest first.
pub fn dump() -> String {
    let lines: Vec<String> = RING.lock().map(|r| r.lines.iter().cloned().collect()).unwrap_or_default();
    let mut out = header(lines.len());
    for l in &lines {
        out.push_str(l);
        out.push('\n');
    }
    out
}

/// Write [`dump`] to `simple-screensharing-<UTC timestamp>.log` in the user's Downloads folder
/// (the home folder where there is none) and return the path.
///
/// Why no file dialog: no dialog crate is a dependency (a native one would pull in a portal
/// client or GTK for one button), and the Downloads folder is where people look for it.
pub fn save_to_downloads() -> io::Result<std::path::PathBuf> {
    let dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no Downloads or home folder"))?;
    save_to(&dir)
}

fn save_to(dir: &std::path::Path) -> io::Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("simple-screensharing-{}.log", file_stamp()));
    std::fs::write(&path, dump())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_keeps_the_newest_lines_within_both_bounds() {
        let mut r = Ring::new(3, 1000);
        for i in 0..10 {
            r.push(format!("line {i}"));
        }
        assert_eq!(r.lines, ["line 7", "line 8", "line 9"]);
        assert_eq!(r.bytes, 18);

        let mut r = Ring::new(1000, 27);
        for i in 0..10 {
            r.push(format!("line number {i}")); // 13 bytes
        }
        assert_eq!(r.lines, ["line number 8", "line number 9"]);
        assert!(r.bytes <= 27);

        // One line bigger than the byte bound still stays (the newest is never dropped).
        let mut r = Ring::new(10, 5);
        r.push("a very long line".into());
        assert_eq!(r.lines.len(), 1);
    }

    #[test]
    fn pending_lines_follow_the_cursor_across_evictions_and_budgets() {
        let mut r = Ring::new(4, 1000);
        for i in 0..3 {
            r.push(format!("line {i}")); // 6 bytes + newline
        }
        assert_eq!(r.next_seq(), 3);
        // Nothing sent yet: everything, nothing skipped.
        assert_eq!(r.pending(0, 1000), Pending { start: 0, skipped: 0, next: 3 });
        // Sent up to 2: only line 2 is new.
        assert_eq!(r.pending(2, 1000), Pending { start: 2, skipped: 0, next: 3 });
        // Caught up: nothing.
        assert_eq!(r.pending(3, 1000), Pending { start: 3, skipped: 0, next: 3 });
        // The ring wraps (4 lines kept: seq 4..=7): lines 0..=3 are gone.
        for i in 3..8 {
            r.push(format!("line {i}"));
        }
        assert_eq!((r.first_seq, r.next_seq()), (4, 8));
        assert_eq!(r.pending(1, 1000), Pending { start: 0, skipped: 3, next: 8 });
        assert_eq!(r.pending(6, 1000), Pending { start: 2, skipped: 0, next: 8 });
        // A cursor ahead of the ring (a stale value) is clamped.
        assert_eq!(r.pending(99, 1000), Pending { start: 4, skipped: 0, next: 8 });
        // Byte budget: 4 lines x 7 bytes = 28; a budget of 15 keeps the newest two.
        assert_eq!(r.pending(4, 15), Pending { start: 2, skipped: 2, next: 8 });
        // Always at least the newest line, however small the budget.
        assert_eq!(r.pending(4, 1), Pending { start: 3, skipped: 3, next: 8 });
    }

    #[test]
    fn warn_and_error_lines_are_noticed() {
        assert!(is_warn_or_error("2026-11-03T12:00:00.000000Z  WARN simple_screensharing_client::net: x"));
        assert!(is_warn_or_error("2026-11-03T12:00:00.000000Z ERROR some_lib: x"));
        assert!(!is_warn_or_error("2026-11-03T12:00:00.000000Z  INFO simple_screensharing_client::net: WARN x"));
        let mut r = Ring::new(10, 1000);
        r.push("t  INFO a: b".into());
        assert_eq!(r.last_warn_seq, None);
        r.push("t  WARN a: b".into());
        r.push("t  INFO a: c".into());
        assert_eq!(r.last_warn_seq, Some(1));
    }

    #[test]
    fn join_links_lose_their_key() {
        assert_eq!(
            redact("joining relay.example:4433|room1|SECRETKEY|certhash|My%20Room now (ok)"),
            "joining relay.example:4433|room1|<redacted> now (ok)"
        );
        assert_eq!(redact("link \"a|b|c\": failed"), "link \"a|b|<redacted>\": failed");
        // Not link-shaped: untouched.
        assert_eq!(redact("a | b and c|d"), "a | b and c|d");
        assert_eq!(redact("no pipes here"), "no pipes here");
    }

    #[test]
    fn save_writes_the_dump_to_a_stamped_file() {
        let dir = std::env::temp_dir().join(format!("sss-logbuf-test-{}", std::process::id()));
        let path = save_to(&dir).unwrap();
        let name = path.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(name.starts_with("simple-screensharing-20") && name.ends_with(".log"), "{name}");
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("Simple Screensharing client log\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_layer_files_whole_events_redacted_and_filtered() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        use tracing_subscriber::prelude::*;
        let sub = tracing_subscriber::registry().with(layer());
        tracing::subscriber::with_default(sub, || {
            tracing::info!(target: "simple_screensharing_client::probe", "ringtest-ours at info, link h|r|KEY123");
            tracing::debug!(target: "simple_screensharing_client::probe", "ringtest-ours at debug");
            tracing::info!(target: "some_lib", "ringtest-lib at info");
            tracing::warn!(target: "some_lib", "ringtest-lib at warn");
        });
        let d = dump();
        assert!(d.contains("ringtest-ours at info, link h|r|<redacted>"));
        assert!(!d.contains("KEY123"));
        assert!(!d.contains("ringtest-ours at debug"));
        assert!(!d.contains("ringtest-lib at info"));
        assert!(d.contains("WARN some_lib: ringtest-lib at warn"));
        assert!(d.starts_with("Simple Screensharing client log\nversion: "));
    }
}
