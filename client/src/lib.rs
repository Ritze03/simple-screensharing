//! Backend library target: networking, capture, decode, encryption, session
//! state and bandwidth-allocation logic. The egui UI (`ui/`, `theme.rs`,
//! `widgets/`) lives in the bin crate (`main.rs`) on top of this; nothing
//! here depends on it.
//!
//! `preview` is a lib module because `capture.rs` does `use crate::preview::...`.
//!
//! `end_to_end_test` is test-only: it spawns
//! the real relay binary and drives real `AppState`s against it, and
//! `audio.rs`'s `stop_returns_promptly_and_reaps_pw_loopback` test uses its
//! `spawn_relay_server`.
//!
//! `#[macro_use] extern crate rust_i18n` + `i18n!` mirror `main.rs`'s own
//! setup: `session.rs` uses the `t!` translation macro for user-facing
//! strings/errors, and that macro is only in scope where `rust_i18n` has been
//! registered like this. Harmless to register in both the bin and lib targets
//! — they're separate compiled artifacts, so there's no runtime
//! double-registration.

#[macro_use]
extern crate rust_i18n;

i18n!("locales", fallback = "en");

pub mod audio;
pub mod bandwidth;
pub mod capture;
pub mod decode;
#[cfg(test)]
mod end_to_end_test;
pub mod fragment;
pub mod net;
pub mod presets;
pub mod preview;
pub mod protocol;
pub mod rooms;
pub mod session;
