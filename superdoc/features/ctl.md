# Hotkeys (`ctl`)

`simple-screensharing ctl <command>` controls every running native client from a shell command, so a desktop
can bind it to a keyboard shortcut. User-facing name everywhere: **Hotkeys** (D17: it is a general shortcut set,
not only an emergency stop, so it is not named after one).
Linux only: normal install and AppImage. Not Flatpak (its sandbox has its own runtime dir, so the socket is
invisible to the host), not Windows (prints "Linux only", exit 1), no macOS.

## Forms

- `simple-screensharing ctl <cmd>` (the `install.sh` symlink), `simple-screensharing-client ctl <cmd>`.
- AppImage: `./<name>.AppImage ctl <cmd>`. `client/packaging/AppRun` execs the client with the arguments unchanged
  and, for `$1 = ctl`, skips the host-requirement check (its warnings are noise and time per key press).

Why a subcommand and not a second binary: the owner wanted `simple-screensharing ctl`, one form for every
install; no packaging change. (D11)

## Commands

| Command | Behaviour |
| --- | --- |
| `webcam start` | On in every room you are **in the call** in, never in a connected-only room; also clears the toggle memory. Fan-out: [webcam capture](stream/webcam-capture.md). |
| `webcam stop` | Off everywhere; forgets the toggle memory. |
| `webcam toggle` | On -> off, remembering the rooms. Off -> on again in the remembered rooms that are still in the call; if none of them is in the call any more, the memory is forgotten and it acts like `start` (every in-call room). After a stop, the app's own webcam button, or with nothing remembered it also acts like `start`. |
| `screenshare stop` | Every screen/window share, including one still in its set-up dialog, plus the audio started with it (`LocalCapture::audio_for_capture`, `stop_share` cascades). Webcam and separately started audio keep going. |
| `audio stop` | Every outgoing audio share. |
| `hangup` | Leaves the call in every in-call room; rooms stay connected. |
| `kill` | Quits every instance cleanly (the SIGTERM path, which leaves rooms), SIGKILL for any still alive after 5 s (`KILL_GRACE`). |
| `help`, `--help`, `-h` | `ctl::help_text()`; never goes over the socket. |

Acts on **every running instance** at once, with no confirmation (D2: it is mainly used for hotkeys, so it must
always work; pressed by accident is the user's fault). Works with the window hidden, minimized or on another
workspace (D14: a hotkey is pressed exactly when the app is not in front).

Output: one line per instance, `pid <pid>: <what happened>`. Exit codes: 0 at least one instance answered, 1 none
running (or not Linux), 2 usage error (`try 'simple-screensharing ctl help'`). Why (D6): a hotkey ignores output,
but a person testing in a terminal should see what happened.

The app shows a toast ending in "(CTL)": Webcam started/stopped, Screen sharing stopped, Audio stopped, Left the
call / in N rooms. Nothing done, no toast. Why (D10): a share vanishing with no trace looks like a bug.

## Design and protocol (`client/src/ctl.rs`)

- One Unix socket per instance: `$XDG_RUNTIME_DIR/simple-screensharing/<pid>.sock` (fallback `/run/user/<uid>`;
  neither: ctl is off and that is logged). Directory 0700, socket 0600, removed on clean exit; a stale one is
  unlinked by the next ctl run (a refused connect). Why (D3): no control channel existed, and a socket carries the
  command and the answer; the rig's private runtime dir isolates tests from the user's real instances.
- Wire: one request line (`webcam toggle\n`), one answer line, close. `kill` and `help` never use it: kill =
  connect check, then SIGTERM by the PID in the socket name, SIGKILL after `KILL_GRACE`. Before signalling, the CLI checks that the socket dir and file are owned by the user and that the peer pid (`SO_PEERCRED`) matches the file name, so a planted or stale socket cannot make it kill an unrelated process. Why (D4): a hotkey must
  always work, so a hung instance is still ended.
- Listener: one thread plus one per connection, pushing into a queue and calling `ctx.request_repaint()` every
  100 ms until the UI answers (the window may be hidden and `update` not running); after `APPLY_WAIT` (2.5 s) it
  answers "queued...". The CLI waits `ANSWER_TIMEOUT` (3.5 s) per instance, instances in parallel; a frozen one
  prints "no answer (not responding)".
- UI side: `client/src/ui/ctl_apply.rs` (`App::drain_ctl`, `apply_ctl`), drained at the top of `update`.
- Toggle memory: `ui::Shell::webcam_memory` (`ctl::WebcamMemory`), cleared by `webcam start`, `webcam stop`, the app's webcam
  buttons, a toggle-on and a toggle that found no remembered room in the call. Why (D7, owner): restore only after a toggle; after a stop it is "like a fresh instance
  basically"; start means all rooms where you are in the call; connected and in-call are different things.
- `ctl` runs in `main.rs` before tokio, logging, GStreamer and GPU setup (about 0.01 s).

## Tests and verification

- Unit tests, `client/src/ctl.rs` `mod tests`: `parses_every_command`, `help_has_three_forms`,
  `unknown_and_incomplete_commands_are_usage_errors`, `wire_form_round_trips_for_the_socket_commands`,
  `exit_codes`, `help_text_covers_commands_and_bindings`, the four `toggle_*` tests,
  `stale_sockets_are_removed_and_live_ones_kept`, `pid_comes_from_the_socket_name`,
  `a_frozen_listener_does_not_hang_the_cli`.
- Rig recipe: `client/scripts/hs-rig.sh run -- simple-screensharing-client ctl <cmd>` (the rig's runtime dir keeps
  it away from real instances; see [rig reference](../testing/rig-reference.md)). Verified with the window on an
  invisible sway workspace: `request_repaint` did run `update`.
- AppImage pass-through: `AppRun ctl ...` checked with a stub client that prints its arguments.
- **Not verified**: a really minimized window on GNOME/KDE; real audio shares (no-audio test rule).
