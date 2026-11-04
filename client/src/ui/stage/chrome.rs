//! Fullscreen auto-hide of the stage's chrome (people strip, control bar, tile overlays), the pure rule.
//!
//! In fullscreen the chrome lies over the stage and is shown only while the pointer is over the stage and
//! moved in the last [`IDLE`] seconds. An open menu (or a drag) holds it up wherever the pointer is and
//! stops the timer; when the hold ends the chrome gets a **full** [`IDLE`] again, so closing a menu never
//! hides the bar at once. Entering fullscreen starts the same grace. Outside fullscreen the chrome always
//! shows and this is not consulted. The replica's rule (`chromeSync` / `chromePointer`), the user's request
//! (#136); the pop-out windows' own overlay (`popout::overlay_shown`) is the older twin for one stream.

/// Seconds without activity before the chrome goes (same as `popout::OVERLAY_IDLE`).
pub const IDLE: f64 = 3.0;

/// What [`step`] remembers between frames.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chrome {
    /// Time of the last activity (or of the hold's end / the start of fullscreen).
    last_active: f64,
    /// The timer was (re)started by a menu closing or by entering fullscreen, not by the pointer: the chrome
    /// stays for the whole [`IDLE`] even with the pointer off the stage, until the pointer is moved on it.
    grace: bool,
    held: bool,
}

impl Chrome {
    /// The state on entering fullscreen at `now`.
    pub fn start(now: f64) -> Self {
        Self { last_active: now, grace: true, held: false }
    }

    /// Seconds until the chrome would go, for scheduling a repaint (`None` when it is held or already gone).
    pub fn remaining(&self, now: f64) -> Option<f64> {
        (!self.held && now - self.last_active < IDLE).then(|| IDLE - (now - self.last_active))
    }
}

/// One frame. `inside`: the pointer is over the stage; `active`: the pointer moved or pressed, the wheel
/// turned or a key was pressed since the last frame; `held`: a menu, modal or drag is open. Returns the new
/// state and whether the chrome shows.
pub fn step(mut s: Chrome, now: f64, inside: bool, active: bool, held: bool) -> (Chrome, bool) {
    if held {
        return (Chrome { last_active: now, grace: true, held: true }, true);
    }
    if s.held {
        // The hold just ended: a full IDLE from now, wherever the pointer is.
        s = Chrome { last_active: now, grace: true, held: false };
    } else if active && inside {
        s = Chrome { last_active: now, grace: false, held: false };
    } else if active {
        // A key press with the pointer off the stage brings the bar back for one IDLE too.
        s.last_active = now;
        s.grace = true;
    }
    let shown = now - s.last_active < IDLE && (inside || s.grace);
    (s, shown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(mut s: Chrome, frames: &[(f64, bool, bool, bool)]) -> (Chrome, Vec<bool>) {
        let mut out = Vec::new();
        for &(t, inside, active, held) in frames {
            let (n, shown) = step(s, t, inside, active, held);
            s = n;
            out.push(shown);
        }
        (s, out)
    }

    #[test]
    fn hides_after_three_seconds_without_movement_and_movement_brings_it_back() {
        let (s, shown) = run(Chrome::start(0.0), &[(0.1, true, true, false), (2.9, true, false, false), (3.2, true, false, false)]);
        assert_eq!(shown, [true, true, false]);
        let (_, shown) = run(s, &[(10.0, true, true, false), (12.9, true, false, false), (13.2, true, false, false)]);
        assert_eq!(shown, [true, true, false], "movement restarts the three seconds");
    }

    #[test]
    fn leaving_the_stage_hides_it_at_once_but_a_grace_period_survives_it() {
        // Pointer on the stage, moved, then leaves: gone at once.
        let (_, shown) = run(Chrome::start(0.0), &[(1.0, true, true, false), (1.1, false, false, false)]);
        assert_eq!(shown, [true, false]);
        // Entering fullscreen with the pointer elsewhere: shown for the first three seconds.
        let (_, shown) = run(Chrome::start(0.0), &[(0.1, false, false, false), (2.9, false, false, false), (3.1, false, false, false)]);
        assert_eq!(shown, [true, true, false]);
    }

    #[test]
    fn a_menu_stops_the_timer_and_after_it_closes_the_bar_gets_a_full_three_seconds() {
        let (s, shown) = run(Chrome::start(0.0), &[(0.5, true, true, false), (30.0, false, false, true), (60.0, false, false, true)]);
        assert_eq!(shown, [true, true, true], "held up for as long as the menu is open, off the stage too");
        assert_eq!(s.remaining(60.0), None, "no timer runs while held");
        let (_, shown) = run(s, &[(60.1, false, false, false), (62.9, false, false, false), (63.2, false, false, false)]);
        assert_eq!(shown, [true, true, false], "a full 3 s after the close, not hidden at once");
    }

    #[test]
    fn the_pointer_off_the_stage_cancels_the_grace_it_started_with_a_move() {
        let (_, shown) = run(Chrome::start(0.0), &[(5.0, true, true, false), (5.5, false, false, false)]);
        assert_eq!(shown, [true, false], "a pointer move on the stage ended the grace, so leaving hides at once");
    }
}
