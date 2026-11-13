//! Test hook of the rig's scripts (`client/scripts/hs-drive.sh`, #47, stage part I5): with `SSS_UI_DUMP=<file>`
//! set, every frame writes where the driven controls are and what they hold, one `key value…` per line, whenever
//! that changes. Rects are `rect NAME x0 y0 x1 y1` (logical points, window-relative); the file ends with `ppp` and
//! `screen W H` so a script can map points to pixels. A rect or line of the popped-out stage window has `pw.` before
//! its name / key. `<file>.actions` logs every dispatched [`crate::ui::UiAction`], one per line. Unset (always,
//! outside the scripts): nothing is recorded.
//!
//! The names are listed in `client/scripts/README.md`.
//!
//! `Why` (#47): `drive.sh` clicked fixed pixel positions read off old screenshots; the modal grew (the encoder-limit
//! banner) and `setup` silently kept 30 fps while `golive` missed. Positions from the live layout, and the values
//! read back, make a miss impossible to overlook.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::OnceLock;

use egui::Rect;

fn path() -> Option<&'static PathBuf> {
    static P: OnceLock<Option<PathBuf>> = OnceLock::new();
    P.get_or_init(|| std::env::var_os("SSS_UI_DUMP").map(PathBuf::from)).as_ref()
}

thread_local! {
    static BUF: RefCell<String> = const { RefCell::new(String::new()) };
    static LAST: RefCell<String> = const { RefCell::new(String::new()) };
    static PREFIX: Cell<&'static str> = const { Cell::new("") };
}

pub fn begin() {
    if path().is_some() {
        BUF.with_borrow_mut(String::clear);
    }
}

/// `SSS_UI_FAKE_AUDIO=1`: every watched stream acts as if its sender shared audio, so the rig (which has no audio) can
/// drive the volume row, the ring and the pop-out's pill. Nothing is played; the UI's value stays at what was dragged.
pub fn fake_audio() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var_os("SSS_UI_FAKE_AUDIO").is_some())
}

/// `SSS_UI_AUTO_PICKUP=1`: every room is picked up (`App::pick_up_room`, the green button's path) as soon as it is
/// joined. A TEST AID of the rig only (`client/scripts/launch-lib.sh` sets it by default, `HS_AUTO_PICKUP=0` turns it
/// off): a normal launch always connects "not in the call".
pub fn auto_pickup() -> bool {
    static F: OnceLock<bool> = OnceLock::new();
    *F.get_or_init(|| std::env::var_os("SSS_UI_AUTO_PICKUP").is_some_and(|v| v != "0"))
}

/// What follows belongs to the popped-out stage window (`pw.` names) or, with `false`, to the main one again.
pub fn in_window(on: bool) {
    PREFIX.set(if on { "pw." } else { "" });
}

pub fn line(text: impl FnOnce() -> String) {
    if path().is_some() {
        BUF.with_borrow_mut(|b| {
            for l in text().lines() {
                b.push_str(PREFIX.get());
                b.push_str(l);
                b.push('\n');
            }
        });
    }
}

pub fn rect(name: impl FnOnce() -> String, r: Rect) {
    if path().is_some() {
        let n = name().split_whitespace().collect::<Vec<_>>().join("_");
        BUF.with_borrow_mut(|b| {
            let _ = writeln!(b, "rect {}{n} {:.1} {:.1} {:.1} {:.1}", PREFIX.get(), r.min.x, r.min.y, r.max.x, r.max.y);
        });
    }
}

/// A dispatched action, appended to `<file>.actions`.
pub fn action(a: &crate::ui::UiAction) {
    let Some(p) = path() else { return };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p.with_extension("actions")) {
        let _ = writeln!(f, "{a:?}");
    }
}

/// Write the frame's lines if they differ from the last write (atomic rename). A frame without the stream settings
/// modal says `open 0`.
pub fn flush(ctx: &egui::Context) {
    let Some(p) = path() else { return };
    let screen = ctx.content_rect();
    let mut text = BUF.with_borrow(Clone::clone);
    if !text.lines().any(|l| l.starts_with("open ")) {
        text.push_str("open 0\n");
    }
    let _ = write!(text, "ppp {}\nscreen {:.0} {:.0}\n", ctx.pixels_per_point(), screen.width(), screen.height());
    if LAST.with_borrow(|l| *l == text) {
        return;
    }
    let tmp = p.with_extension("tmp");
    if std::fs::write(&tmp, &text).and_then(|()| std::fs::rename(&tmp, p)).is_ok() {
        LAST.set(text);
    }
}
