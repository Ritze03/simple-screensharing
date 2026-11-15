//! `simple-screensharing --ctl <command>`: a tiny command line that controls every running client at once, so
//! users can bind **Hotkeys** on their desktop (stop the webcam, stop sharing, hang up, kill).
//!
//! Two halves in one module:
//!
//! * **The app side** ([`start_listener`]): each running client opens a Unix socket
//!   `$XDG_RUNTIME_DIR/simple-screensharing/<pid>.sock` and answers one line per connection. The listener runs on
//!   its own thread; it queues the command for the UI thread ([`take_pending`], drained at the top of
//!   `ui::App::update`), because `AppState` lives there, and waits up to [`APPLY_WAIT`] for the answer.
//! * **The CLI side** ([`run`]): parse, send the command to every socket (parallel, short timeouts, dead sockets
//!   unlinked), print one line per instance, pick the exit code. `kill` is the exception: it signals the PID.
//!
//! Wire protocol: the CLI sends one line, `<command>\n` with the command words joined by one space
//! (`webcam toggle`); the app answers one line of plain text and closes. Linux only (D1): on other platforms
//! [`run`] says so and exits 1, and the listener does not exist.
//!
//! `Why a socket per instance and not one shared service:` several clients can run at once, each owns its own
//! `AppState`; "every running instance" is "every socket in the folder", and a crashed one is detected by a
//! refused connect.

use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use crate::session::RoomHandle;

/// What the CLI can ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    WebcamStart,
    WebcamStop,
    WebcamToggle,
    ScreenshareStop,
    AudioStop,
    Hangup,
    /// Quit every instance (SIGTERM, then SIGKILL). Handled by the CLI itself, never sent over the socket.
    Kill,
    Help,
}

impl Command {
    /// The wire form (`webcam toggle`); the same words the user types.
    pub fn wire(self) -> &'static str {
        match self {
            Command::WebcamStart => "webcam start",
            Command::WebcamStop => "webcam stop",
            Command::WebcamToggle => "webcam toggle",
            Command::ScreenshareStop => "screenshare stop",
            Command::AudioStop => "audio stop",
            Command::Hangup => "hangup",
            Command::Kill => "kill",
            Command::Help => "help",
        }
    }

    /// Parse the words after `--ctl`. Whitespace between words is not significant.
    pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Command, String> {
        let words: Vec<&str> = args.iter().map(|s| s.as_ref()).collect();
        match words.as_slice() {
            [] => Err("missing command".into()),
            ["help" | "--help" | "-h", ..] => Ok(Command::Help),
            ["webcam", "start"] => Ok(Command::WebcamStart),
            ["webcam", "stop"] => Ok(Command::WebcamStop),
            ["webcam", "toggle"] => Ok(Command::WebcamToggle),
            ["webcam"] => Err("webcam needs start, stop or toggle".into()),
            ["screenshare", "stop"] => Ok(Command::ScreenshareStop),
            ["screenshare"] => Err("screenshare needs stop".into()),
            ["audio", "stop"] => Ok(Command::AudioStop),
            ["audio"] => Err("audio needs stop".into()),
            ["hangup"] => Ok(Command::Hangup),
            ["kill"] => Ok(Command::Kill),
            [first, ..] if matches!(*first, "webcam" | "screenshare" | "audio" | "hangup" | "kill") => {
                Err(format!("unexpected argument {:?} for {first}", words[1..].join(" ")))
            }
            _ => Err(format!("unknown command {:?}", words.join(" "))),
        }
    }

    /// Parse the line the CLI sent (what [`Self::wire`] produced). `kill` and `help` are not wire commands.
    #[cfg(any(target_os = "linux", test))]
    fn parse_wire(line: &str) -> Option<Command> {
        let words: Vec<&str> = line.split_whitespace().collect();
        match Command::parse(&words) {
            Ok(Command::Kill | Command::Help) | Err(_) => None,
            Ok(c) => Some(c),
        }
    }
}

/// Exit code: 0 when at least one instance was reached, 1 when none is running.
pub fn exit_code(reached: usize) -> i32 {
    if reached > 0 { 0 } else { 1 }
}

/// Exit code of a usage error (unknown command): a hint, not a run.
pub const EXIT_USAGE: i32 = 2;

/// The `help` text.
pub fn help_text() -> String {
    "\
simple-screensharing --ctl: control every running Simple Screensharing window at once.

It exists so you can bind Hotkeys on your desktop: press a key and the command goes to
every running instance, whether its window is in front, minimized or on another workspace.
Nothing asks for confirmation (a hotkey pressed by accident does what it says), and the
app shows a short notice ending in \"(CTL)\" when a command acted.

Usage: simple-screensharing --ctl <command>

Commands:
  webcam start       Turn the webcam on in every room you are in the call in.
  webcam stop        Turn the webcam off everywhere and forget which rooms it was on in.
  webcam toggle      Off if on (remembering the rooms); if off, on again in those rooms
                     (like \"webcam start\" after a stop, or when none of them is in the
                     call any more).
  screenshare stop   Stop every screen or window share, also one still in its set-up
                     dialog, and the audio that was started together with it.
  audio stop         Stop every audio share (Desktop audio or one app) in every room.
  hangup             Leave the call in every room you are in the call in (rooms stay
                     connected; rooms you are only connected to are left alone).
  kill               Quit every running instance (they leave their rooms first); one that
                     has not quit after a few seconds is killed.
  help, --help, -h   Show this text.

Output and exit code: one line per instance (\"pid 1234: webcam stopped in 2 rooms\").
Exit 0 when at least one instance answered, 1 when none is running, 2 for a usage error.

Binding a command to a key (use the same command line; bind a different key per command):
  GNOME   Settings > Keyboard > View and Customize Shortcuts > Custom Shortcuts, command:
            simple-screensharing --ctl webcam toggle
  KDE     System Settings > Keyboard > Shortcuts > Add New > Command or Script, command:
            simple-screensharing --ctl webcam toggle
  sway    bindsym $mod+Shift+w exec simple-screensharing --ctl webcam toggle
  Hyprland
          bind = SUPER SHIFT, W, exec, simple-screensharing --ctl webcam toggle

AppImage: use the file instead of the installed name:
  /path/to/<name>.AppImage --ctl webcam toggle

Only the normal install and the AppImage on Linux are supported (not Flatpak, not Windows).
"
    .to_string()
}

/// One-line hint printed after a usage error.
pub fn usage_hint() -> &'static str {
    "try 'simple-screensharing --ctl help'"
}

// ---------------------------------------------------------------------------
// Webcam toggle memory (pure; the decision logic of D7)
// ---------------------------------------------------------------------------

/// What `webcam toggle` does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebcamPlan {
    /// Turn it off everywhere (the rooms were remembered).
    Stop(Vec<RoomHandle>),
    /// Turn it on in exactly these rooms.
    Start(Vec<RoomHandle>),
    /// Off, and no room is in the call.
    NoRoomInCall,
}

/// The rooms the webcam was on in when `webcam toggle` last turned it off. Cleared by `webcam stop`, by the app's
/// own webcam buttons and by a toggle-on, so a toggle after any of those behaves like `start`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WebcamMemory {
    rooms: Vec<RoomHandle>,
}

impl WebcamMemory {
    pub fn forget(&mut self) {
        self.rooms.clear();
    }

    pub fn remembered(&self) -> &[RoomHandle] {
        &self.rooms
    }

    /// Decide `webcam toggle`: `on` = rooms the webcam is on in now, `in_call` = rooms we are in the call in.
    pub fn toggle(&mut self, on: &[RoomHandle], in_call: &[RoomHandle]) -> WebcamPlan {
        if !on.is_empty() {
            self.rooms = on.to_vec();
            return WebcamPlan::Stop(on.to_vec());
        }
        if self.rooms.is_empty() {
            return if in_call.is_empty() { WebcamPlan::NoRoomInCall } else { WebcamPlan::Start(in_call.to_vec()) };
        }
        let restore: Vec<RoomHandle> = self.rooms.iter().copied().filter(|r| in_call.contains(r)).collect();
        self.rooms.clear();
        if restore.is_empty() {
            // None of the remembered rooms is in the call any more: a hotkey must not get stuck, so this is `start`.
            return if in_call.is_empty() { WebcamPlan::NoRoomInCall } else { WebcamPlan::Start(in_call.to_vec()) };
        }
        WebcamPlan::Start(restore)
    }
}

// ---------------------------------------------------------------------------
// Instances: socket folder, stale cleanup
// ---------------------------------------------------------------------------

/// A running (or crashed) client found through its socket.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub pid: u32,
    pub path: std::path::PathBuf,
}

/// `<runtime dir>/simple-screensharing`. The runtime dir is `$XDG_RUNTIME_DIR`, else `/run/user/<uid>` when that
/// exists (a hotkey daemon started outside a login session may lack the variable); `None` when neither exists.
#[cfg(target_os = "linux")]
pub fn socket_dir() -> Option<std::path::PathBuf> {
    let base = match std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        Some(v) => std::path::PathBuf::from(v),
        None => {
            // SAFETY: getuid has no preconditions.
            let p = std::path::PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
            if !p.is_dir() {
                return None;
            }
            p
        }
    };
    Some(base.join("simple-screensharing"))
}

/// The PID in a socket file name (`<pid>.sock`).
#[cfg(target_os = "linux")]
fn pid_of(path: &std::path::Path) -> Option<u32> {
    path.file_name()?.to_str()?.strip_suffix(".sock")?.parse().ok()
}

/// Every socket file in `dir`, whether or not anything listens.
#[cfg(target_os = "linux")]
fn socket_files(dir: &std::path::Path) -> Vec<Instance> {
    let mut out: Vec<Instance> =
        std::fs::read_dir(dir).into_iter().flatten().flatten().filter_map(|e| pid_of(&e.path()).map(|pid| Instance { pid, path: e.path() })).collect();
    out.sort_by_key(|i| i.pid);
    out
}

/// Whether something listens on `path`: connecting works (also for a frozen process, whose kernel backlog
/// still accepts). `ECONNREFUSED` means the socket file outlived its process: unlinked, `false`.
#[cfg(target_os = "linux")]
fn connect_or_sweep(path: &std::path::Path) -> Option<std::os::unix::net::UnixStream> {
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(s) => Some(s),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound) => {
            let _ = std::fs::remove_file(path);
            None
        }
        Err(_) => None,
    }
}

/// The PID of the process listening behind `stream` (`SO_PEERCRED`), `None` when the kernel cannot say.
#[cfg(target_os = "linux")]
fn peer_pid(stream: &std::os::unix::net::UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `cred` and `len` are valid for the call and `len` is its size.
    let rc = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&mut cred as *mut libc::ucred).cast(), &mut len) };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

/// Whether `path` is owned by the current user.
#[cfg(target_os = "linux")]
fn owned_by_me(path: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: getuid has no preconditions.
    std::fs::metadata(path).is_ok_and(|m| m.uid() == unsafe { libc::getuid() })
}

/// Remove the sockets in `dir` nobody listens on; the live instances with their connection.
#[cfg(target_os = "linux")]
fn live_instances(dir: &std::path::Path) -> Vec<(Instance, std::os::unix::net::UnixStream)> {
    socket_files(dir).into_iter().filter_map(|i| connect_or_sweep(&i.path).map(|s| (i, s))).collect()
}

// ---------------------------------------------------------------------------
// CLI side
// ---------------------------------------------------------------------------

/// How long the CLI waits for an instance's answer. Above [`APPLY_WAIT`], so an instance that applies in time
/// is never reported as silent; a frozen one (SIGSTOP) never answers and must not hang the CLI.
pub const ANSWER_TIMEOUT: Duration = Duration::from_millis(3500);
/// How long `kill` waits for the clean quit before SIGKILL (a little above the app's own 4 s hard exit).
pub const KILL_GRACE: Duration = Duration::from_millis(5000);

/// `simple-screensharing --ctl <args>`. Returns the process exit code. Never opens a window or touches GStreamer.
pub fn run<S: AsRef<str>>(args: &[S]) -> i32 {
    let command = match Command::parse(args) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("simple-screensharing --ctl: {e}\n{}", usage_hint());
            return EXIT_USAGE;
        }
    };
    #[cfg(target_os = "linux")]
    {
        run_linux(command)
    }
    #[cfg(not(target_os = "linux"))]
    {
        if command == Command::Help {
            print!("{}", help_text());
            return 0;
        }
        eprintln!("simple-screensharing --ctl is Linux-only (normal install and AppImage).");
        1
    }
}

#[cfg(target_os = "linux")]
fn run_linux(command: Command) -> i32 {
    use std::io::Write;
    if command == Command::Help {
        print!("{}", help_text());
        return 0;
    }
    let Some(dir) = socket_dir() else {
        eprintln!("no running instance (no runtime directory: XDG_RUNTIME_DIR is not set)");
        return 1;
    };
    let live = live_instances(&dir);
    if live.is_empty() {
        eprintln!("no running instance");
        return exit_code(0);
    }
    let lines: Vec<String> = if command == Command::Kill {
        kill_all(live)
    } else {
        let handles: Vec<_> = live.into_iter().map(|(i, s)| std::thread::spawn(move || ask(i, s, command))).collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    };
    let mut out = std::io::stdout().lock();
    for l in &lines {
        let _ = writeln!(out, "{l}");
    }
    exit_code(lines.len())
}

/// Send `command` to one instance, return its output line.
#[cfg(target_os = "linux")]
fn ask(instance: Instance, mut stream: std::os::unix::net::UnixStream, command: Command) -> String {
    use std::io::{BufRead, Write};
    let _ = stream.set_write_timeout(Some(ANSWER_TIMEOUT));
    let _ = stream.set_read_timeout(Some(ANSWER_TIMEOUT));
    let pid = instance.pid;
    if stream.write_all(format!("{}\n", command.wire()).as_bytes()).is_err() {
        return format!("pid {pid}: could not send the command");
    }
    let mut line = String::new();
    match std::io::BufReader::new(stream).read_line(&mut line) {
        Ok(n) if n > 0 => format!("pid {pid}: {}", line.trim_end()),
        _ => format!("pid {pid}: no answer (not responding)"),
    }
}

/// `kill`: SIGTERM every live instance (the app's own clean quit: leave every room, then exit), wait up to
/// [`KILL_GRACE`], SIGKILL what is left, remove their sockets. Only PIDs whose socket accepted a connection
/// are signalled: a stale file of a crashed client (whose PID may belong to something else by now) was swept.
#[cfg(target_os = "linux")]
fn kill_all(live: Vec<(Instance, std::os::unix::net::UnixStream)>) -> Vec<String> {
    // Only ours: the socket file and its folder owned by this user, and the listener's PID (SO_PEERCRED) the one
    // in the file name. Anything else could make the CLI signal a stranger's process.
    let dir_ok = live.first().and_then(|(i, _)| i.path.parent().map(owned_by_me)).unwrap_or(false);
    let mut lines = Vec::new();
    let mut instances: Vec<Instance> = Vec::new();
    for (i, s) in live {
        if !dir_ok || !owned_by_me(&i.path) {
            lines.push(format!("pid {}: skipped (its socket or folder is not owned by you)", i.pid));
        } else if peer_pid(&s) != Some(i.pid) {
            lines.push(format!("pid {}: skipped (the socket's owner is not that process)", i.pid));
        } else {
            instances.push(i);
        }
    }
    for i in &instances {
        // SAFETY: plain signal to a PID we just proved is a listening client.
        unsafe { libc::kill(i.pid as libc::pid_t, libc::SIGTERM) };
    }
    let started = std::time::Instant::now();
    let mut left: Vec<&Instance> = instances.iter().collect();
    while !left.is_empty() && started.elapsed() < KILL_GRACE {
        left.retain(|i| process_alive(i.pid));
        if !left.is_empty() {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    for i in &instances {
        let line = if left.iter().any(|l| l.pid == i.pid) {
            // SAFETY: as above.
            unsafe { libc::kill(i.pid as libc::pid_t, libc::SIGKILL) };
            format!("pid {}: did not quit in time, killed", i.pid)
        } else {
            format!("pid {}: quit", i.pid)
        };
        let _ = std::fs::remove_file(&i.path);
        lines.push(line);
    }
    lines
}

/// Whether `pid` is a running process (a zombie awaiting its parent counts as gone).
#[cfg(target_os = "linux")]
fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks existence.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => !s.rsplit(')').next().is_some_and(|rest| rest.trim_start().starts_with('Z')),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// App side
// ---------------------------------------------------------------------------

/// A command waiting for the UI thread, with the channel its answer goes back through.
pub struct Request {
    pub command: Command,
    pub reply: mpsc::Sender<String>,
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
}

const REQ_PENDING: u8 = 0;
const REQ_TAKEN: u8 = 1;
const REQ_EXPIRED: u8 = 2;
/// Requests kept waiting for the UI thread; beyond this the oldest is dropped.
const QUEUE_CAP: usize = 16;

impl Request {
    /// The UI thread claims the request. `false` when the listener already gave up and answered: a command the
    /// caller was told was not applied must not run later.
    pub fn claim(&self) -> bool {
        use std::sync::atomic::Ordering::SeqCst;
        self.state.compare_exchange(REQ_PENDING, REQ_TAKEN, SeqCst, SeqCst).is_ok()
    }
}

static PENDING: Mutex<Vec<Request>> = Mutex::new(Vec::new());

/// Commands received since the last call. Drained by the UI thread at the top of every frame.
pub fn take_pending() -> Vec<Request> {
    std::mem::take(&mut *PENDING.lock().unwrap_or_else(|e| e.into_inner()))
}

/// How long the listener waits for the UI thread's answer before replying "queued".
pub const APPLY_WAIT: Duration = Duration::from_millis(2500);

/// Path of this process's socket, once the listener runs (for [`cleanup`]).
#[cfg(target_os = "linux")]
static MY_SOCKET: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);

/// Open this client's control socket and serve it on a background thread. Failures are logged and leave the
/// app without Hotkey support, never without the app.
#[cfg(target_os = "linux")]
pub fn start_listener(ctx: egui::Context) {
    let Some(dir) = socket_dir() else {
        tracing::info!("no runtime directory: Hotkey control (ctl) is off");
        return;
    };
    if let Err(e) = std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700).create(&dir) {
        tracing::warn!("ctl: cannot create {}: {e}", dir.display());
        return;
    }
    // A folder somebody else made (or one with wider rights) is not ours to put a control socket in.
    let dir_safe = std::fs::metadata(&dir).is_ok_and(|m| {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: getuid has no preconditions.
        m.is_dir() && m.uid() == unsafe { libc::getuid() } && m.mode() & 0o077 == 0
    });
    if !dir_safe {
        tracing::warn!("ctl: {} is not a private folder of ours (owner or mode): Hotkey control (ctl) is off", dir.display());
        return;
    }
    let path = dir.join(format!("{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    // The socket file is created with the process umask: tighten it for the bind so it is never wider than 0600.
    // SAFETY: umask only swaps the process mask.
    let old_mask = unsafe { libc::umask(0o177) };
    let bound = std::os::unix::net::UnixListener::bind(&path);
    // SAFETY: as above.
    unsafe { libc::umask(old_mask) };
    let listener = match bound {
        Ok(l) => l,
        Err(e) => {
            tracing::warn!("ctl: cannot bind {}: {e}", path.display());
            return;
        }
    };
    *MY_SOCKET.lock().unwrap_or_else(|e| e.into_inner()) = Some(path.clone());
    tracing::info!("ctl: listening on {}", path.display());
    let spawned = std::thread::Builder::new().name("ctl-listener".into()).spawn(move || {
        for conn in listener.incoming() {
            let Ok(conn) = conn else {
                // EMFILE and friends: do not spin.
                std::thread::sleep(Duration::from_millis(100));
                continue;
            };
            let ctx = ctx.clone();
            let _ = std::thread::Builder::new().name("ctl-conn".into()).spawn(move || serve_connection(conn, ctx));
        }
    });
    if spawned.is_err() {
        tracing::warn!("ctl: cannot start the listener thread");
    }
}

/// Remove this client's socket file (clean exit).
#[cfg(target_os = "linux")]
pub fn cleanup() {
    if let Some(p) = MY_SOCKET.lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = std::fs::remove_file(p);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn start_listener(_ctx: egui::Context) {}

#[cfg(not(target_os = "linux"))]
pub fn cleanup() {}

/// One CLI connection: read the command, queue it for the UI thread, wake the frame loop until it answered
/// (a window that is not drawn may not run a frame on the first wake-up, so the wake-up repeats), write the
/// answer.
#[cfg(target_os = "linux")]
fn serve_connection(stream: std::os::unix::net::UnixStream, ctx: egui::Context) {
    use std::io::{BufRead, Write};
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let mut line = String::new();
    let Ok(read) = std::io::BufReader::new(std::io::Read::take(&stream, 256)).read_line(&mut line) else { return };
    if read == 0 {
        return; // the CLI only probed (kill) or went away
    }
    let answer = match Command::parse_wire(&line) {
        None => format!("unknown command {:?}", line.trim()),
        Some(command) => {
            let (tx, rx) = mpsc::channel();
            let state = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(REQ_PENDING));
            {
                let mut q = PENDING.lock().unwrap_or_else(|e| e.into_inner());
                q.push(Request { command, reply: tx, state: state.clone() });
                if q.len() > QUEUE_CAP {
                    let old = q.remove(0);
                    old.state.store(REQ_EXPIRED, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let started = std::time::Instant::now();
            loop {
                ctx.request_repaint();
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(a) => break a,
                    Err(_) if started.elapsed() >= APPLY_WAIT => {
                        use std::sync::atomic::Ordering::SeqCst;
                        let late = "not applied: the app did not answer in time".to_string();
                        if state.compare_exchange(REQ_PENDING, REQ_EXPIRED, SeqCst, SeqCst).is_ok() {
                            break late;
                        }
                        // Claimed just now: its answer is on its way.
                        break rx.recv_timeout(Duration::from_secs(1)).unwrap_or(late);
                    }
                    Err(_) => {}
                }
            }
        }
    };
    let _ = (&stream).write_all(format!("{answer}\n").as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(n: u32) -> RoomHandle {
        RoomHandle(n)
    }

    fn parse(s: &str) -> Result<Command, String> {
        let words: Vec<&str> = s.split_whitespace().collect();
        Command::parse(&words)
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(parse("webcam start"), Ok(Command::WebcamStart));
        assert_eq!(parse("webcam stop"), Ok(Command::WebcamStop));
        assert_eq!(parse("webcam toggle"), Ok(Command::WebcamToggle));
        assert_eq!(parse("screenshare stop"), Ok(Command::ScreenshareStop));
        assert_eq!(parse("audio stop"), Ok(Command::AudioStop));
        assert_eq!(parse("hangup"), Ok(Command::Hangup));
        assert_eq!(parse("kill"), Ok(Command::Kill));
    }

    #[test]
    fn help_has_three_forms() {
        for f in ["help", "--help", "-h"] {
            assert_eq!(parse(f), Ok(Command::Help), "{f}");
        }
    }

    #[test]
    fn unknown_and_incomplete_commands_are_usage_errors() {
        for bad in ["", "frobnicate", "webcam", "webcam pause", "screenshare", "audio start", "hangup now", "kill -9", "webcam stop now"] {
            assert!(parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn wire_form_round_trips_for_the_socket_commands() {
        for c in [Command::WebcamStart, Command::WebcamStop, Command::WebcamToggle, Command::ScreenshareStop, Command::AudioStop, Command::Hangup] {
            assert_eq!(Command::parse_wire(&format!("{}\n", c.wire())), Some(c));
        }
        assert_eq!(Command::parse_wire("kill\n"), None, "kill never travels over the socket");
        assert_eq!(Command::parse_wire("help\n"), None);
        assert_eq!(Command::parse_wire("rm -rf\n"), None);
    }

    #[test]
    fn exit_codes() {
        assert_eq!(exit_code(0), 1);
        assert_eq!(exit_code(1), 0);
        assert_eq!(exit_code(5), 0);
        assert_eq!(EXIT_USAGE, 2);
    }

    #[test]
    fn help_text_covers_commands_and_bindings() {
        let t = help_text();
        for needle in [
            "webcam start", "webcam stop", "webcam toggle", "screenshare stop", "audio stop", "hangup", "kill", "GNOME", "KDE", "sway",
            "Hyprland", "AppImage", "bindsym $mod+Shift+w exec simple-screensharing --ctl webcam toggle",
            "bind = SUPER SHIFT, W, exec, simple-screensharing --ctl webcam toggle", "Hotkeys",
        ] {
            assert!(t.contains(needle), "help lacks {needle:?}");
        }
        assert!(!t.to_lowercase().contains("panic"));
    }

    #[test]
    fn toggle_off_remembers_and_on_restores_only_rooms_still_in_the_call() {
        let mut m = WebcamMemory::default();
        assert_eq!(m.toggle(&[h(1), h(2)], &[h(1), h(2), h(3)]), WebcamPlan::Stop(vec![h(1), h(2)]));
        // Room 2 hung up meanwhile, room 3 joined the call: only room 1 comes back.
        assert_eq!(m.toggle(&[], &[h(1), h(3)]), WebcamPlan::Start(vec![h(1)]));
        // The toggle-on cleared the memory: the next toggle-on (after another off by other means) is like start.
        assert_eq!(m.toggle(&[], &[h(1), h(3)]), WebcamPlan::Start(vec![h(1), h(3)]));
    }

    #[test]
    fn toggle_after_stop_or_a_button_behaves_like_start() {
        let mut m = WebcamMemory::default();
        m.toggle(&[h(1)], &[h(1), h(2)]);
        m.forget(); // `webcam stop` or the app's own button
        assert_eq!(m.toggle(&[], &[h(1), h(2)]), WebcamPlan::Start(vec![h(1), h(2)]));
    }

    #[test]
    fn toggle_with_nothing_remembered_and_no_call_does_nothing() {
        let mut m = WebcamMemory::default();
        assert_eq!(m.toggle(&[], &[]), WebcamPlan::NoRoomInCall);
    }

    #[test]
    fn toggle_clears_the_memory_and_acts_like_start_when_no_remembered_room_is_in_the_call() {
        let mut m = WebcamMemory::default();
        m.toggle(&[h(1)], &[h(1)]);
        assert_eq!(m.toggle(&[], &[h(2), h(3)]), WebcamPlan::Start(vec![h(2), h(3)]));
        assert!(m.remembered().is_empty());
        m.toggle(&[h(1)], &[h(1)]);
        assert_eq!(m.toggle(&[], &[]), WebcamPlan::NoRoomInCall);
        assert!(m.remembered().is_empty());
    }

    #[cfg(target_os = "linux")]
    fn scratch(name: &str) -> std::path::PathBuf {
        // Under the target dir, never /tmp (a tmpfs); a path too long for sun_path (108) falls back to $HOME/tmp.
        let mut base = std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../target/ctl-test"));
        if base.to_string_lossy().len() > 70 {
            base = std::path::PathBuf::from(std::env::var("HOME").expect("HOME")).join("tmp/ctl-test");
        }
        let d = base.join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn stale_sockets_are_removed_and_live_ones_kept() {
        use std::os::unix::net::UnixListener;
        let dir = scratch("sweep");
        let live_path = dir.join("111.sock");
        let _live = UnixListener::bind(&live_path).unwrap();
        // A socket file whose listener is gone: bind, then drop the listener (the file stays).
        let stale_path = dir.join("222.sock");
        drop(UnixListener::bind(&stale_path).unwrap());
        assert!(stale_path.exists());
        std::fs::write(dir.join("notasocket.txt"), "x").unwrap();

        let live = live_instances(&dir);
        assert_eq!(live.iter().map(|(i, _)| i.pid).collect::<Vec<_>>(), vec![111]);
        assert!(!stale_path.exists(), "the stale socket must be unlinked");
        assert!(live_path.exists());
        assert!(dir.join("notasocket.txt").exists(), "foreign files are left alone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pid_comes_from_the_socket_name() {
        assert_eq!(pid_of(std::path::Path::new("/x/1234.sock")), Some(1234));
        assert_eq!(pid_of(std::path::Path::new("/x/abc.sock")), None);
        assert_eq!(pid_of(std::path::Path::new("/x/1234")), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_frozen_listener_does_not_hang_the_cli() {
        use std::os::unix::net::UnixListener;
        // Listens (kernel accepts into the backlog) but never answers: like a SIGSTOPped client.
        let dir = scratch("frozen");
        let path = dir.join("333.sock");
        let _l = UnixListener::bind(&path).unwrap();
        let s = connect_or_sweep(&path).expect("connect works");
        let started = std::time::Instant::now();
        let line = ask(Instance { pid: 333, path }, s, Command::AudioStop);
        assert!(line.contains("no answer"), "{line}");
        assert!(started.elapsed() < ANSWER_TIMEOUT + Duration::from_secs(2));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
