//! Opt-in upload of the client's own log to the relay (spec §6.7, task #110).
//!
//! While Settings > Diagnostics > "Send diagnostics to the relay" is ticked, every relay
//! connection runs [`upload_loop`]: it says `DiagHello` (a 1-byte datagram that every older relay
//! drops as undersized), waits for the relay's `DiagAck`, and then sends the log lines the
//! in-memory ring (`logbuf`) gained since the last upload, one reliable uni stream per batch
//! (type byte `0x02`, version, UTF-8 text). The relay stores them per person and session.
//!
//! Why: the owner and friends are testing heavily and the owner used to have to ask each one
//! to "start it like this, then send me the file". User request, 2026-11-03: "It would be way
//! better if he or all of my friends could just tick the checkbox while we are still in heavy
//! development, and then I can just see it. Everyone knows this data is being collected."
//!
//! Never in the way of media: it runs in its own task, its only shared state is the ring's
//! mutex (held for a copy of the new lines), a batch is at most [`MAX_DIAG_BATCH_BYTES`], and
//! a relay that does not accept it within [`UPLOAD_TIMEOUT`] is simply skipped (the lines stay
//! in the bounded ring and go with the next batch). Nothing is queued beyond the ring.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use quinn::VarInt;

use crate::logbuf;
use crate::protocol::{DIAG_TAG_HELLO, DIAG_UPLOAD_VERSION, MAX_DIAG_BATCH_BYTES, STREAM_TYPE_DIAG};

/// Batches go out at most this often, unless a WARN/ERROR line is waiting.
pub const BATCH_INTERVAL: Duration = Duration::from_secs(30);
/// Earliest a batch triggered by a WARN/ERROR line follows the previous one (a burst of
/// warnings is one batch, not one per line).
pub const WARN_INTERVAL: Duration = Duration::from_secs(3);
/// Give up on one batch (open, write, delivered) after this long.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// How often the loop looks at the ring.
const TICK: Duration = Duration::from_secs(1);
/// `DiagHello` is sent once per tick, at most this many times per ticking of the box.
const MAX_HELLOS: u32 = 10;
/// Room kept in a batch for the type/version bytes and the "N lines skipped" note.
const BATCH_SLACK: usize = 256;

static ENABLED: AtomicBool = AtomicBool::new(false);
/// Lines numbered below this were logged before the box was ticked and are never uploaded.
static FLOOR: AtomicU64 = AtomicU64::new(0);

/// Turn the upload on or off (the Settings checkbox, and the stored value at startup).
///
/// `now == true` marks a tick while the app runs: only lines logged from now on may go, not the
/// ones that were already in the ring (the user agreed to share what happens from here on).
/// At startup (`now == false`) the stored setting is already the user's earlier agreement, so
/// the whole ring, startup lines included, is eligible.
pub fn set_enabled(on: bool, now: bool) {
    if on && now && !ENABLED.load(Ordering::Relaxed) {
        FLOOR.store(logbuf::next_seq(), Ordering::Relaxed);
    }
    ENABLED.store(on, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// One batch ready to be sent: the stream's bytes and the ring cursor to continue from once the
/// relay has them.
#[derive(Debug, PartialEq, Eq)]
pub struct Batch {
    pub bytes: Vec<u8>,
    pub next: u64,
    pub lines: usize,
}

/// The "which lines are already sent" bookkeeping of one connection.
#[derive(Debug)]
pub struct UploadState {
    cursor: u64,
    header_sent: bool,
    last_sent: Option<Instant>,
}

impl UploadState {
    pub fn new() -> Self {
        Self { cursor: 0, header_sent: false, last_sent: None }
    }

    /// The batch due at `now`, if any. The first one of a connection is due at once (it
    /// carries the header: version, OS, GPU), then one per [`BATCH_INTERVAL`] while there are
    /// new lines, or after [`WARN_INTERVAL`] when one of them is a WARN/ERROR.
    pub fn due(&self, now: Instant, floor: u64) -> Option<Batch> {
        let cursor = self.cursor.max(floor);
        let since = self.last_sent.map(|t| now.saturating_duration_since(t));
        let wanted = match since {
            None => true,
            Some(s) if s >= BATCH_INTERVAL => logbuf::next_seq() > cursor,
            Some(s) if s >= WARN_INTERVAL => logbuf::warn_since(cursor),
            Some(_) => false,
        };
        if !wanted {
            return None;
        }
        let header = (!self.header_sent).then(logbuf::upload_header);
        let budget = MAX_DIAG_BATCH_BYTES - BATCH_SLACK - header.as_ref().map_or(0, String::len);
        let lines = logbuf::lines_since(cursor, budget);
        if header.is_none() && lines.lines == 0 {
            return None;
        }
        Some(Batch { bytes: encode(header.as_deref(), &lines), next: lines.next, lines: lines.lines })
    }

    /// The relay has the batch.
    pub fn sent(&mut self, batch: &Batch, now: Instant) {
        self.cursor = batch.next;
        self.header_sent = true;
        self.last_sent = Some(now);
    }
}

impl Default for UploadState {
    fn default() -> Self {
        Self::new()
    }
}

/// The bytes of one diagnostics stream (spec §6.7): `STREAM_TYPE_DIAG | version | text`.
fn encode(header: Option<&str>, lines: &logbuf::LineBatch) -> Vec<u8> {
    let mut out = vec![STREAM_TYPE_DIAG, DIAG_UPLOAD_VERSION];
    if let Some(h) = header {
        out.extend_from_slice(h.as_bytes());
    }
    if lines.skipped > 0 {
        out.extend_from_slice(format!("[{} log lines not uploaded (ring wrapped or batch limit)]\n", lines.skipped).as_bytes());
    }
    out.extend_from_slice(lines.text.as_bytes());
    debug_assert!(out.len() <= MAX_DIAG_BATCH_BYTES);
    out
}

/// Send one batch and wait until the relay has read it all. On any failure or timeout the
/// stream is reset, so the relay discards a partial batch and the lines can go again.
async fn send_batch(connection: &quinn::Connection, bytes: &[u8]) -> bool {
    let work = async {
        let mut send = connection.open_uni().await.ok()?;
        let ok = async {
            send.write_all(bytes).await.ok()?;
            send.finish().ok()?;
            send.stopped().await.ok()?;
            Some(())
        };
        match tokio::time::timeout(UPLOAD_TIMEOUT, ok).await {
            Ok(Some(())) => Some(()),
            _ => {
                let _ = send.reset(VarInt::from_u32(1));
                None
            }
        }
    };
    matches!(tokio::time::timeout(UPLOAD_TIMEOUT * 2, work).await, Ok(Some(())))
}

/// Per-connection task, spawned by `ScreenshareClient::connect`; ends with the connection.
/// `acked` is set by the datagram loop when the relay answers `DiagHello`.
pub async fn upload_loop(connection: quinn::Connection, acked: Arc<AtomicBool>) {
    let mut state = UploadState::new();
    let mut hellos = 0;
    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        if connection.close_reason().is_some() {
            return;
        }
        if !enabled() {
            hellos = 0;
            continue;
        }
        if !acked.load(Ordering::Relaxed) {
            if hellos < MAX_HELLOS {
                hellos += 1;
                let _ = connection.send_datagram(Bytes::from_static(&[DIAG_TAG_HELLO]));
            }
            continue;
        }
        let Some(batch) = state.due(Instant::now(), FLOOR.load(Ordering::Relaxed)) else { continue };
        if send_batch(&connection, &batch.bytes).await {
            tracing::debug!(lines = batch.lines, bytes = batch.bytes.len(), "diagnostics batch uploaded");
            state.sent(&batch, Instant::now());
        } else {
            tracing::debug!(lines = batch.lines, "diagnostics batch not accepted by the relay in time, will retry");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `logbuf` is one process-wide ring, so these tests only add their own marker lines and
    // look for them; they never assume what else is in it.

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        logbuf::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn log(marker: &str) {
        use tracing_subscriber::prelude::*;
        let sub = tracing_subscriber::registry().with(logbuf::layer());
        tracing::subscriber::with_default(sub, || {
            tracing::info!(target: "simple_screensharing_client::probe", "{marker}");
        });
    }

    fn text(b: &Batch) -> String {
        assert_eq!(&b.bytes[..2], &[STREAM_TYPE_DIAG, DIAG_UPLOAD_VERSION]);
        String::from_utf8(b.bytes[2..].to_vec()).unwrap()
    }

    #[test]
    fn first_batch_has_the_header_then_only_new_lines() {
        let _guard = lock();
        let t0 = Instant::now();
        let mut state = UploadState::new();
        let floor = logbuf::next_seq();
        log("upl-one");
        let b = state.due(t0, floor).expect("the first batch is due at once");
        let first = text(&b);
        assert!(first.starts_with("Simple Screensharing client log\nversion: "), "{first}");
        assert!(first.contains("upl-one"));
        state.sent(&b, t0);

        // Nothing new: nothing due, however late.
        assert_eq!(state.due(t0 + Duration::from_secs(90), floor), None);
        // New lines wait for the interval ...
        log("upl-two");
        assert_eq!(state.due(t0 + Duration::from_secs(10), floor), None);
        let b = state.due(t0 + BATCH_INTERVAL, floor).expect("due after the interval");
        let second = text(&b);
        // ... and go without the header and without what was sent before.
        assert!(second.contains("upl-two") && !second.contains("upl-one"), "{second}");
        assert!(!second.contains("Simple Screensharing client log"));
        state.sent(&b, t0 + BATCH_INTERVAL);
        assert_eq!(state.due(t0 + BATCH_INTERVAL * 3, floor), None);
    }

    #[test]
    fn a_failed_batch_is_resent_and_a_warning_sends_early() {
        let _guard = lock();
        let t0 = Instant::now();
        let mut state = UploadState::new();
        let floor = logbuf::next_seq();
        log("retry-one");
        let b = state.due(t0, floor).unwrap();
        state.sent(&b, t0);
        log("retry-two");
        let lost = state.due(t0 + BATCH_INTERVAL, floor).unwrap();
        assert!(text(&lost).contains("retry-two"));
        // Not marked sent (the upload failed): the same lines are due again, a tick later.
        let again = state.due(t0 + BATCH_INTERVAL + TICK, floor).unwrap();
        assert!(text(&again).contains("retry-two") && !text(&again).contains("retry-one"));

        // A WARN does not wait for the full interval, but does wait WARN_INTERVAL.
        state.sent(&again, t0 + BATCH_INTERVAL);
        {
            use tracing_subscriber::prelude::*;
            let sub = tracing_subscriber::registry().with(logbuf::layer());
            tracing::subscriber::with_default(sub, || {
                tracing::warn!(target: "simple_screensharing_client::probe", "retry-warn");
            });
        }
        let last = t0 + BATCH_INTERVAL;
        assert_eq!(state.due(last + Duration::from_secs(1), floor), None);
        let w = state.due(last + WARN_INTERVAL, floor).expect("a warning sends early");
        assert!(text(&w).contains("retry-warn"));
    }

    #[test]
    fn lines_before_the_floor_are_never_sent() {
        let _guard = lock();
        let t0 = Instant::now();
        log("before-the-tick");
        let floor = logbuf::next_seq();
        log("after-the-tick");
        let b = UploadState::new().due(t0, floor).unwrap();
        let t = text(&b);
        assert!(t.contains("after-the-tick") && !t.contains("before-the-tick"), "{t}");
    }

    #[test]
    fn a_batch_never_exceeds_the_stream_limit() {
        let _guard = lock();
        let t0 = Instant::now();
        let floor = logbuf::next_seq();
        let long = "x".repeat(1000);
        for i in 0..400 {
            log(&format!("big-{i} {long}"));
        }
        let b = UploadState::new().due(t0, floor).unwrap();
        assert!(b.bytes.len() <= MAX_DIAG_BATCH_BYTES, "{}", b.bytes.len());
        let t = text(&b);
        // The newest lines win, and the missing ones are counted.
        assert!(t.contains("big-399 "));
        assert!(t.contains("log lines not uploaded"), "{}", &t[t.len().min(2000)..].len());
    }

    #[test]
    fn ticking_the_box_mid_session_skips_what_is_already_in_the_ring() {
        // One test owns the two statics, so it cannot race the others.
        let _guard = lock();
        set_enabled(false, true);
        log("pre-tick-marker");
        set_enabled(true, true);
        let floor = FLOOR.load(Ordering::Relaxed);
        assert_eq!(floor, logbuf::next_seq());
        // A startup load keeps whatever floor there is.
        set_enabled(true, false);
        assert_eq!(FLOOR.load(Ordering::Relaxed), floor);
        set_enabled(false, true);
        assert!(!enabled());
    }
}
