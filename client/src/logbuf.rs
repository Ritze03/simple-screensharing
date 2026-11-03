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
}

impl Ring {
    const fn new(max_lines: usize, max_bytes: usize) -> Self {
        Self { lines: VecDeque::new(), bytes: 0, max_lines, max_bytes }
    }

    fn push(&mut self, line: String) {
        self.bytes += line.len();
        self.lines.push_back(line);
        // Always keeps the newest line, even one larger than `max_bytes`.
        while self.lines.len() > self.max_lines || (self.bytes > self.max_bytes && self.lines.len() > 1) {
            if let Some(old) = self.lines.pop_front() {
                self.bytes -= old.len();
            }
        }
    }
}

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

/// What a dump starts with.
fn header(lines: usize) -> String {
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
    h.push_str(&format!(
        "dumped: {}\nlines: {lines} (the most recent; INFO+ from the app, WARN+ from libraries; times are UTC)\n\
         ----------------------------------------\n",
        now_utc()
    ));
    h
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
