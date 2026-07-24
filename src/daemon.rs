//! The recorder: watch Hyprland's event socket and snapshot the layout whenever
//! something structural changes (with debouncing), plus a periodic safety snapshot
//! to catch changes that emit no event (e.g. manual resizes).

use crate::{capture, hypr, state, R};
use std::io::{ErrorKind, Read};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// How long a closed window stays restorable by default.
pub const DEFAULT_KEEP_CLOSED: Duration = Duration::from_secs(600);

/// Daemon tuning.
#[derive(Debug, Clone)]
pub struct Config {
    /// Quiet period after the last relevant event before snapshotting.
    pub debounce: Duration,
    /// Maximum interval between snapshots regardless of events.
    pub periodic: Duration,
    /// How long a closed window is remembered (still restorable) before it ages
    /// out of the snapshot. Zero mirrors live state exactly.
    pub keep_closed: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            debounce: Duration::from_millis(800),
            periodic: Duration::from_secs(30),
            keep_closed: DEFAULT_KEEP_CLOSED,
        }
    }
}

/// Event names that change what we need to persist.
const RELEVANT: &[&str] = &[
    "openwindow",
    "closewindow",
    "movewindow",
    "movewindowv2",
    "changefloatingmode",
    "fullscreen",
    "windowtitle",       // terminal cwd often changes with the title
    "windowtitlev2",
    "openworkspace",
    "createworkspace",
    "createworkspacev2",
    "destroyworkspace",
    "destroyworkspacev2",
    "moveworkspace",
    "pin",
];

/// Take a single snapshot now and persist it, using the default keep-closed
/// window. Used by `--once` and by the CLI's `save`.
///
/// Resolves the running Hyprland instance first so `hyprctl` works even when this
/// process did not inherit `HYPRLAND_INSTANCE_SIGNATURE` (e.g. under systemd).
pub fn snapshot_once() -> R<()> {
    persist(DEFAULT_KEEP_CLOSED)
}

/// Capture the live layout, merge in recently-closed windows from the previous
/// snapshot, and persist.
fn persist(keep_closed: Duration) -> R<()> {
    hypr::ensure_instance_env()?;
    let live = capture::live_snapshot()?;
    let merged = match state::load() {
        Ok(prev) => capture::merge(live, &prev, keep_closed),
        Err(_) => live, // no prior snapshot yet
    };
    state::save(&merged)?;
    Ok(())
}

/// Persist unless a restore is in progress (whose half-built layout must not be
/// written over the saved one).
fn snapshot_if_idle(keep_closed: Duration) -> R<()> {
    if state::is_restore_active() {
        return Ok(());
    }
    persist(keep_closed)
}

/// Supervise the recorder: (re)resolve the running Hyprland instance, watch its
/// event socket, and reconnect when the socket closes (e.g. a compositor restart)
/// instead of exiting — so a single long-lived service survives Hyprland restarts.
pub fn run(cfg: Config) -> R<()> {
    let backoff = Duration::from_secs(2);
    loop {
        match connect_and_watch(&cfg) {
            Ok(()) => eprintln!("window-reloadd: event socket closed; reconnecting…"),
            Err(e) => eprintln!("window-reloadd: {e}; retrying…"),
        }
        std::thread::sleep(backoff);
    }
}

/// Connect to the current instance's event socket and watch it until it closes.
fn connect_and_watch(cfg: &Config) -> R<()> {
    // Resolve the active instance and export it so child hyprctl calls target it.
    hypr::ensure_instance_env()?;
    let path = hypr::event_socket_path()?;
    let mut stream = UnixStream::connect(&path)
        .map_err(|e| format!("cannot connect to {}: {e}", path.display()))?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;

    eprintln!("window-reloadd: connected to {}", path.display());

    // Take an initial snapshot ONLY if we have no saved state yet. At login the
    // daemon and `window-reload restore` start together; snapshotting the (empty)
    // login desktop now would clobber last session's layout before restore has
    // read it. When a snapshot already exists it is the baseline to protect —
    // subsequent events and the periodic timer keep it current once the session
    // (and any restore) has settled.
    let have_state = state::state_path().map(|p| p.exists()).unwrap_or(false);
    if !have_state {
        if let Err(e) = snapshot_if_idle(cfg.keep_closed) {
            eprintln!("window-reloadd: initial snapshot failed: {e}");
        }
    }

    let mut buf = [0u8; 8192];
    let mut line: Vec<u8> = Vec::new();
    let mut dirty_since: Option<Instant> = None;
    let mut last_snapshot = Instant::now();

    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                // Socket closed (usually a Hyprland restart). Flush any pending
                // change, then return so the supervisor reconnects.
                if dirty_since.is_some() {
                    let _ = snapshot_if_idle(cfg.keep_closed);
                }
                return Ok(());
            }
            Ok(n) => {
                for &byte in &buf[..n] {
                    if byte == b'\n' {
                        // Event names are ASCII; lossy decoding keeps titles from
                        // corrupting the parse even mid-multibyte.
                        if is_relevant(&String::from_utf8_lossy(&line)) {
                            dirty_since = Some(Instant::now());
                        }
                        line.clear();
                    } else {
                        line.push(byte);
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                // Timeout tick: fall through to timer checks.
            }
            Err(e) => return Err(Box::new(e)),
        }

        let now = Instant::now();

        // Debounced snapshot once events have settled.
        if let Some(since) = dirty_since {
            if now.duration_since(since) >= cfg.debounce {
                if let Err(e) = snapshot_if_idle(cfg.keep_closed) {
                    eprintln!("window-reloadd: snapshot failed: {e}");
                }
                dirty_since = None;
                last_snapshot = now;
            }
        }

        // Periodic safety snapshot.
        if now.duration_since(last_snapshot) >= cfg.periodic {
            if let Err(e) = snapshot_if_idle(cfg.keep_closed) {
                eprintln!("window-reloadd: periodic snapshot failed: {e}");
            }
            last_snapshot = now;
            dirty_since = None;
        }
    }
}

/// A line looks like `EVENT>>DATA`; is EVENT one we care about?
fn is_relevant(line: &str) -> bool {
    match line.split_once(">>") {
        Some((event, _)) => RELEVANT.contains(&event),
        None => false,
    }
}
