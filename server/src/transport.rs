//! QUIC transport tuning of the relay (congestion controller, datagram send
//! buffer, idle timeout, keep-alive). One place, one struct, so production and
//! the lab (`transport_lab.rs`) build the exact same quinn config. The
//! measurements and the reasoning behind every default are in
//! `superdoc/features/relay-transport.md`; the native client mirrors the
//! defaults in `client/src/net.rs` (`build_client_endpoint`) and must be kept
//! in step.

use std::sync::Arc;
use std::time::Duration;

/// Which quinn congestion controller governs a connection's send side.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CongestionControl {
    /// quinn's default (loss-based).
    Cubic,
    NewReno,
    /// quinn's `BbrConfig` (labelled experimental upstream).
    Bbr,
}

impl CongestionControl {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cubic" => Some(Self::Cubic),
            "newreno" | "reno" => Some(Self::NewReno),
            "bbr" => Some(Self::Bbr),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TransportTuning {
    pub cc: CongestionControl,
    /// quinn's `datagram_send_buffer_size`: when it is full, the OLDEST queued
    /// datagrams are dropped silently to make room for the new one.
    pub datagram_send_buffer: usize,
    pub max_idle: Duration,
    pub keep_alive: Duration,
}

impl Default for TransportTuning {
    /// The production settings (2026-11-02, relay-transport.md): BBR, quinn's
    /// 1 MiB datagram buffer, 20 s idle timeout, 5 s keep-alive.
    fn default() -> Self {
        Self {
            cc: CongestionControl::Bbr,
            datagram_send_buffer: 1024 * 1024,
            max_idle: Duration::from_secs(20),
            keep_alive: Duration::from_secs(5),
        }
    }
}

impl TransportTuning {
    /// What the relay and client ran until 2026-11-02 (quinn defaults, 60 s idle
    /// timeout, 15 s keep-alive): the "old build" the lab compares against and
    /// that the user's deployed relay still is.
    #[cfg(test)]
    pub fn legacy() -> Self {
        Self {
            cc: CongestionControl::Cubic,
            datagram_send_buffer: 1024 * 1024,
            max_idle: Duration::from_secs(60),
            keep_alive: Duration::from_secs(15),
        }
    }

    /// The defaults with the operator overrides applied:
    /// `SSSHARE_CC=cubic|newreno|bbr` (go back to the old controller without a
    /// rebuild) and `SSSHARE_DGRAM_BUF_KB=N`. A value that does not parse is
    /// ignored with a note on stderr.
    pub fn from_env() -> Self {
        let mut t = Self::default();
        if let Ok(v) = std::env::var("SSSHARE_CC") {
            match CongestionControl::parse(&v) {
                Some(cc) => t.cc = cc,
                None => eprintln!("ignoring SSSHARE_CC={v:?} (want cubic, newreno or bbr)"),
            }
        }
        if let Ok(v) = std::env::var("SSSHARE_DGRAM_BUF_KB") {
            match v.trim().parse::<usize>() {
                Ok(kb) if kb > 0 => t.datagram_send_buffer = kb * 1024,
                _ => eprintln!("ignoring SSSHARE_DGRAM_BUF_KB={v:?} (want a positive number of KiB)"),
            }
        }
        t
    }

    /// Build the quinn transport config for these settings.
    pub fn build(&self) -> quinn::TransportConfig {
        let mut t = quinn::TransportConfig::default();
        t.max_idle_timeout(Some(
            quinn::IdleTimeout::try_from(self.max_idle).expect("idle timeout fits in a QUIC varint"),
        ))
        .keep_alive_interval(Some(self.keep_alive))
        .datagram_send_buffer_size(self.datagram_send_buffer);
        match self.cc {
            CongestionControl::Cubic => {}
            CongestionControl::NewReno => {
                t.congestion_controller_factory(Arc::new(quinn::congestion::NewRenoConfig::default()));
            }
            CongestionControl::Bbr => {
                t.congestion_controller_factory(Arc::new(quinn::congestion::BbrConfig::default()));
            }
        }
        t
    }
}
