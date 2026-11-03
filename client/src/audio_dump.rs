//! Debug dumps of shared audio, off unless the environment variable `SSS_AUDIO_DUMP` names a
//! directory (task #128). Two kinds of file per stream, named `<kind>-<stream>-<unix ms>.*`:
//!
//! - `.pkt`: every Opus packet, binary records `[u64 LE arrival_us][u64 LE pts_us][u32 LE len][payload]`
//!   (`arrival_us` counts from when the file was opened). The sender writes what it sends
//!   (`sent-*`), the native viewer what it receives (`recv-*`). `client/scripts/vm-audio/pktdump.py`
//!   decodes one to a WAV (sequentially, the way a perfect receiver would) and prints the PTS steps,
//!   which separates "the sender sent broken audio" from "the viewer played it badly".
//! - `.log`: text lines of what the viewer's jitter buffer / timeline did (`push`, `gap`, re-bases).
//!
//! Cost when unset: one `std::env::var_os` per stream start.

use std::fmt;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// The environment variable that turns the dumps on.
pub const ENV: &str = "SSS_AUDIO_DUMP";

/// One open dump file. Write errors are ignored (a debug aid must never break the stream).
pub struct Dump {
    out: BufWriter<File>,
    opened: Instant,
}

impl Dump {
    /// `$SSS_AUDIO_DUMP/<kind>-<stream>-<unix ms>.<ext>`, or `None` when the variable is unset or the
    /// file cannot be created.
    pub fn open(kind: &str, stream: impl fmt::Display, ext: &str) -> Option<Self> {
        let dir = PathBuf::from(std::env::var_os(ENV)?);
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let path = dir.join(format!("{kind}-{stream}-{ms}.{ext}"));
        match File::create(&path) {
            Ok(f) => {
                tracing::info!("audio dump: writing {}", path.display());
                Some(Self { out: BufWriter::new(f), opened: Instant::now() })
            }
            Err(e) => {
                tracing::warn!("audio dump: cannot create {}: {e}", path.display());
                None
            }
        }
    }

    fn now_us(&self) -> u64 {
        self.opened.elapsed().as_micros() as u64
    }

    /// One packet record (see the module docs for the layout).
    pub fn packet(&mut self, pts_us: u64, payload: &[u8]) {
        let t = self.now_us();
        let _ = self.out.write_all(&t.to_le_bytes());
        let _ = self.out.write_all(&pts_us.to_le_bytes());
        let _ = self.out.write_all(&(payload.len() as u32).to_le_bytes());
        let _ = self.out.write_all(payload);
    }

    /// One text line, prefixed with the time since the file was opened (microseconds).
    pub fn line(&mut self, args: fmt::Arguments<'_>) {
        let t = self.now_us();
        let _ = writeln!(self.out, "{t} {args}");
    }
}

impl Drop for Dump {
    fn drop(&mut self) {
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packets_are_written_in_the_documented_layout() {
        let dir = std::env::temp_dir().join(format!("sss-dump-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.pkt");
        {
            let mut d = Dump { out: BufWriter::new(File::create(&path).unwrap()), opened: Instant::now() };
            d.packet(1_234_567, &[1, 2, 3]);
            d.packet(1_254_567, &[]);
        }
        let b = std::fs::read(&path).unwrap();
        assert_eq!(b.len(), 2 * 20 + 3);
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 1_234_567);
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), 3);
        assert_eq!(&b[20..23], &[1, 2, 3]);
        assert_eq!(u64::from_le_bytes(b[31..39].try_into().unwrap()), 1_254_567);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
