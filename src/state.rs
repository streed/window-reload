//! Persisting the snapshot to `$XDG_STATE_HOME/window-reload/state.json` atomically.

use crate::model::Snapshot;
use crate::R;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// A restore in progress older than this is treated as stale (crashed), so the
/// recorder resumes snapshotting rather than being blocked forever.
const RESTORE_LOCK_STALE: Duration = Duration::from_secs(300);

/// Directory holding our state (`$XDG_STATE_HOME/window-reload`, default
/// `~/.local/state/window-reload`).
pub fn state_dir() -> R<PathBuf> {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var("HOME").map_err(|_| "HOME is not set")?;
            PathBuf::from(home).join(".local").join("state")
        }
    };
    Ok(base.join("window-reload"))
}

/// Path to the current snapshot file.
pub fn state_path() -> R<PathBuf> {
    Ok(state_dir()?.join("state.json"))
}

/// Path to the sentinel that marks a restore in progress. Lives in the runtime
/// dir (tmpfs, cleared each boot) so a leftover file never survives a reboot.
pub fn restore_lock_path() -> R<PathBuf> {
    let dir = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => state_dir()?,
    };
    Ok(dir.join("window-reload.restoring"))
}

/// Is a restore currently running (fresh sentinel present)?
pub fn is_restore_active() -> bool {
    let Ok(path) = restore_lock_path() else {
        return false;
    };
    match fs::metadata(&path).and_then(|m| m.modified()) {
        Ok(modified) => SystemTime::now()
            .duration_since(modified)
            .map(|age| age < RESTORE_LOCK_STALE)
            .unwrap_or(true), // clock skew into the future: assume active
        Err(_) => false,
    }
}

/// Load the most recent snapshot.
pub fn load() -> R<Snapshot> {
    let path = state_path()?;
    let data = fs::read(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    Ok(serde_json::from_slice(&data)?)
}

/// Write a snapshot atomically, keeping the previous file as `state.json.bak`.
pub fn save(snap: &Snapshot) -> R<()> {
    let dir = state_dir()?;
    fs::create_dir_all(&dir)?;
    let path = state_path()?;

    // Roll the current file to .bak so a bad write never loses the last good one.
    if path.exists() {
        let _ = fs::copy(&path, dir.join("state.json.bak"));
    }

    // Write to a temp file in the same directory, then rename over the target.
    let tmp = dir.join(format!("state.json.tmp.{}", std::process::id()));
    let json = serde_json::to_vec_pretty(snap)?;
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;

    // Best-effort durability of the rename itself (harmless if the FS rejects it).
    if let Ok(dirf) = fs::File::open(&dir) {
        let _ = dirf.sync_all();
    }
    Ok(())
}
