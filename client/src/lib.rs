//! Backend library target: networking, capture, decode, encryption, and
//! bandwidth-allocation logic, reused as-is by both the existing egui
//! front-end (`main.rs`/`ui.rs`, bin-only) and the new Tauri front-end
//! (`src-tauri/`).
//!
//! `ui.rs` and `theme.rs` are deliberately NOT re-exported here: they're
//! egui-specific and are being extracted/replaced separately. Everything
//! below is front-end-agnostic.
//!
//! `preview` is included even though it wasn't in the original module list
//! handed down for this task: `capture.rs` does `use crate::preview::...`,
//! so leaving it out of the lib would make `capture` (which the task does
//! require) fail to compile as a lib member.
//!
//! `end_to_end_test` is included test-only (mirroring `main.rs`'s own
//! `#[cfg(test)] mod end_to_end_test;`): `audio.rs`'s
//! `stop_returns_promptly_and_reaps_pw_loopback` test calls
//! `crate::end_to_end_test::server_repo_candidates()`, and since this same
//! `audio.rs` source file is now also compiled as part of *this* lib crate,
//! that path needs to resolve here too or `cargo test --workspace` fails to
//! compile the lib's test target.

pub mod audio;
pub mod bandwidth;
pub mod capture;
pub mod decode;
#[cfg(test)]
mod end_to_end_test;
pub mod fragment;
pub mod net;
pub mod preview;
pub mod protocol;
pub mod rooms;
