//! Build a [`Snapshot`] from the live Hyprland state.

use crate::model::*;
use crate::{hypr, launch, proc, R, SNAPSHOT_VERSION};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Query Hyprland and assemble a snapshot of the windows currently open.
pub fn live_snapshot() -> R<Snapshot> {
    let clients = hypr::clients()?;
    let workspaces = hypr::workspaces()?;
    let monitors = hypr::monitors()?;

    let saved_monitors = monitors
        .iter()
        .map(|m| SavedMonitor {
            id: m.id,
            name: m.name.clone(),
            width: m.width,
            height: m.height,
            x: m.x,
            y: m.y,
            active_workspace_id: m.active_workspace.id,
        })
        .collect();

    let saved_workspaces = workspaces
        .iter()
        // Special (scratchpad) workspaces have negative ids; skip them.
        .filter(|w| w.id > 0)
        .map(|w| SavedWorkspace {
            id: w.id,
            name: w.name.clone(),
            monitor: w.monitor.clone(),
            monitor_id: w.monitor_id,
            persistent: w.persistent,
            layout: w.tiled_layout.clone(),
        })
        .collect();

    // Only real, mapped windows on normal (positive-id) workspaces.
    let live: Vec<&Client> = clients
        .iter()
        .filter(|c| c.mapped && !c.hidden && c.workspace.id > 0)
        .collect();

    // Map window address -> group key, so members of a group can be reunited on restore.
    let mut group_key_of: HashMap<String, String> = HashMap::new();
    for c in &live {
        if c.grouped.len() > 1 {
            // Deterministic key: the lexicographically smallest member address.
            if let Some(key) = c.grouped.iter().min() {
                group_key_of.insert(c.address.clone(), key.clone());
            }
        }
    }

    let now = crate::now_unix();
    let windows = live
        .iter()
        .map(|c| build_window(c, &group_key_of, now))
        .collect();

    Ok(Snapshot {
        version: SNAPSHOT_VERSION,
        captured_at_unix: now,
        monitors: saved_monitors,
        workspaces: saved_workspaces,
        windows,
    })
}

/// Merge a fresh live snapshot with the previous one, carrying forward windows
/// that closed within `keep_closed` so a restore can bring them back.
///
/// A previous window is dropped (not carried) when it is already represented in
/// the live snapshot — either by a live window sharing its address (the same
/// window still open) or by any live window sharing its signature (an equivalent
/// window is still open, whether it kept its address or was recreated with a new
/// one after a compositor restart). Only a window that is *truly gone* — no live
/// window shares its signature — is remembered, and only until it ages out.
///
/// The signature check is deliberately coverage-based rather than counted:
/// signatures are not unique (two terminals in one directory share one), and they
/// are the only identity a restore has. Carrying a closed window whose signature
/// is still live would therefore spawn a duplicate of the live window on the next
/// restore, which is exactly how ghost windows used to accumulate.
pub fn merge(mut live: Snapshot, prev: &Snapshot, keep_closed: Duration) -> Snapshot {
    let now = live.captured_at_unix;
    let ttl = keep_closed.as_secs();

    let live_addrs: HashSet<&str> = live.windows.iter().map(|w| w.address.as_str()).collect();
    // Signatures currently represented by a live window; a closed window matching
    // one of these is redundant and must not be carried forward.
    let live_sigs: HashSet<String> = live.windows.iter().map(|w| w.signature()).collect();

    let mut carried: Vec<SavedWindow> = Vec::new();
    for pw in &prev.windows {
        if live_addrs.contains(pw.address.as_str()) {
            continue; // same window still open; the live copy is fresher
        }
        if live_sigs.contains(&pw.signature()) {
            continue; // an equivalent window is still open; carrying it would duplicate it
        }
        if now.saturating_sub(pw.last_seen_unix) <= ttl {
            carried.push(pw.clone()); // recently closed and truly gone — remember it
        }
    }

    // Keep workspace metadata for any carried window whose workspace is now empty.
    for cw in &carried {
        if !live.workspaces.iter().any(|ws| ws.id == cw.workspace_id) {
            if let Some(ws) = prev.workspaces.iter().find(|ws| ws.id == cw.workspace_id) {
                live.workspaces.push(ws.clone());
            }
        }
    }

    live.windows.extend(carried);
    live
}

fn build_window(c: &Client, group_key_of: &HashMap<String, String>, now: u64) -> SavedWindow {
    let kind = launch::classify(&c.class, &c.initial_class);
    let is_terminal = kind == WindowKind::Terminal;

    let term_cwd = if is_terminal {
        proc::resolve_terminal_cwd(c.pid)
    } else {
        None
    };

    // Chrome specifics. All windows of a Chrome instance share one browser process
    // (so pid/argv reveal neither profile) and the compositor only reports the
    // class; we recover the profile from Chrome's session files so restore can
    // relaunch it. See [`crate::chrome`].
    let exe = proc::exe(c.pid);
    let (app_id, profile) = match kind {
        WindowKind::ChromeApp => match launch::parse_chrome_app(&c.class) {
            // The class profile segment is sanitized (spaces → '_'); restore its real
            // on-disk directory name so `--profile-directory` matches.
            Some((a, seg)) => {
                let profile = exe
                    .as_deref()
                    .and_then(crate::chrome::config_dir_for_exe)
                    .map(|dir| crate::chrome::desanitize_profile(&dir, &seg))
                    .unwrap_or(seg);
                (Some(a), Some(profile))
            }
            None => (None, None),
        },
        WindowKind::Chrome => {
            let profile = crate::chrome::config_dir_for_class(&c.class)
                .and_then(|dir| crate::chrome::find_profile(&dir, &c.title));
            (None, profile)
        }
        _ => (None, None),
    };

    let (group_key, group_index) = match group_key_of.get(&c.address) {
        Some(key) => {
            // Index of this window within its group's ordered member list.
            let idx = c
                .grouped
                .iter()
                .position(|a| a == &c.address)
                .unwrap_or(0);
            (Some(key.clone()), idx)
        }
        None => (None, 0),
    };

    SavedWindow {
        address: c.address.clone(),
        last_seen_unix: now,
        kind,
        class: c.class.clone(),
        initial_class: c.initial_class.clone(),
        title: c.title.clone(),
        initial_title: c.initial_title.clone(),
        xwayland: c.xwayland,
        workspace_id: c.workspace.id,
        workspace_name: c.workspace.name.clone(),
        monitor: c.monitor,
        at: c.at,
        size: c.size,
        floating: c.floating,
        fullscreen: c.fullscreen,
        pinned: c.pinned,
        group_key,
        group_index,
        pid: c.pid,
        cmdline: proc::cmdline(c.pid),
        exe,
        is_terminal,
        term_cwd,
        app_id,
        profile,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(addr: &str, class: &str, ws: i64, cwd: Option<&str>, last_seen: u64) -> SavedWindow {
        SavedWindow {
            address: addr.into(),
            last_seen_unix: last_seen,
            kind: if cwd.is_some() { WindowKind::Terminal } else { WindowKind::Generic },
            class: class.into(),
            initial_class: class.into(),
            title: String::new(),
            initial_title: String::new(),
            xwayland: false,
            workspace_id: ws,
            workspace_name: ws.to_string(),
            monitor: 0,
            at: [0, 0],
            size: [100, 100],
            floating: false,
            fullscreen: 0,
            pinned: false,
            group_key: None,
            group_index: 0,
            pid: 0,
            cmdline: vec![class.into()],
            exe: None,
            is_terminal: cwd.is_some(),
            term_cwd: cwd.map(str::to_string),
            app_id: None,
            profile: None,
        }
    }

    fn snap(now: u64, windows: Vec<SavedWindow>) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            captured_at_unix: now,
            monitors: vec![],
            workspaces: vec![],
            windows,
        }
    }

    #[test]
    fn open_window_not_duplicated() {
        // Same window (same address) present in prev and live -> one copy, the live one.
        let prev = snap(100, vec![win("0xA", "Alacritty", 1, Some("/x"), 100)]);
        let live = snap(200, vec![win("0xA", "Alacritty", 1, Some("/x"), 200)]);
        let merged = merge(live, &prev, Duration::from_secs(600));
        assert_eq!(merged.windows.len(), 1);
        assert_eq!(merged.windows[0].last_seen_unix, 200);
    }

    #[test]
    fn recently_closed_window_is_remembered() {
        let prev = snap(100, vec![win("0xA", "Alacritty", 4, Some("/proj"), 100)]);
        let live = snap(150, vec![]); // closed at some point before t=150
        let merged = merge(live, &prev, Duration::from_secs(600));
        assert_eq!(merged.windows.len(), 1, "recently closed window should be kept");
        assert_eq!(merged.windows[0].term_cwd.as_deref(), Some("/proj"));
    }

    #[test]
    fn long_closed_window_is_forgotten() {
        let prev = snap(100, vec![win("0xA", "Alacritty", 4, Some("/proj"), 100)]);
        let live = snap(1000, vec![]); // 900s later, beyond the 600s TTL
        let merged = merge(live, &prev, Duration::from_secs(600));
        assert!(merged.windows.is_empty(), "stale closed window should age out");
    }

    #[test]
    fn recreated_window_new_address_not_duplicated() {
        // After a restart the window comes back with a new address but same signature.
        let prev = snap(100, vec![win("0xOLD", "Alacritty", 4, Some("/proj"), 100)]);
        let live = snap(150, vec![win("0xNEW", "Alacritty", 4, Some("/proj"), 150)]);
        let merged = merge(live, &prev, Duration::from_secs(600));
        assert_eq!(merged.windows.len(), 1, "recreated window must not duplicate");
        assert_eq!(merged.windows[0].address, "0xNEW");
    }

    #[test]
    fn keep_closed_zero_mirrors_live() {
        let prev = snap(100, vec![win("0xA", "Alacritty", 4, Some("/proj"), 100)]);
        let live = snap(150, vec![]);
        let merged = merge(live, &prev, Duration::from_secs(0));
        assert!(merged.windows.is_empty(), "keep_closed=0 must not carry anything");
    }

    #[test]
    fn closed_ghost_not_carried_while_equivalent_window_lives() {
        // Two terminals share a directory (same signature). One is closed and
        // remembered in `prev`; the other kept running with a stable address.
        // The closed ghost must not be carried — it would re-spawn a duplicate of
        // the surviving terminal on the next restore.
        let prev = snap(
            100,
            vec![
                win("0xLIVE", "Alacritty", 7, Some("/proj"), 100),
                win("0xGHOST", "Alacritty", 7, Some("/proj"), 90), // closed a moment ago
            ],
        );
        let live = snap(120, vec![win("0xLIVE", "Alacritty", 7, Some("/proj"), 120)]);
        let merged = merge(live, &prev, Duration::from_secs(600));
        assert_eq!(
            merged.windows.len(),
            1,
            "a recently-closed window must not duplicate a still-live equivalent"
        );
        assert_eq!(merged.windows[0].address, "0xLIVE");
    }

    #[test]
    fn multiple_live_same_signature_all_kept() {
        // Genuinely two terminals open in the same directory: both are live and
        // both must survive the merge (this is not duplication).
        let prev = snap(
            100,
            vec![
                win("0xA", "Alacritty", 7, Some("/proj"), 100),
                win("0xB", "Alacritty", 7, Some("/proj"), 100),
            ],
        );
        let live = snap(
            120,
            vec![
                win("0xA", "Alacritty", 7, Some("/proj"), 120),
                win("0xB", "Alacritty", 7, Some("/proj"), 120),
            ],
        );
        let merged = merge(live, &prev, Duration::from_secs(600));
        assert_eq!(merged.windows.len(), 2, "both live terminals must be kept");
    }

    #[test]
    fn all_same_signature_closed_are_remembered() {
        // When every window of a signature is gone, the closed ones are still
        // remembered (nothing live covers them).
        let prev = snap(
            100,
            vec![
                win("0xA", "Alacritty", 7, Some("/proj"), 100),
                win("0xB", "Alacritty", 7, Some("/proj"), 95),
            ],
        );
        let live = snap(120, vec![]); // whole workspace closed
        let merged = merge(live, &prev, Duration::from_secs(600));
        assert_eq!(
            merged.windows.len(),
            2,
            "with no live equivalent, recently-closed windows are remembered"
        );
    }

    #[test]
    fn cwd_change_same_address_updates_not_duplicates() {
        // User cd'd: same window (same address), different signature. No phantom.
        let prev = snap(100, vec![win("0xA", "Alacritty", 4, Some("/old"), 100)]);
        let live = snap(150, vec![win("0xA", "Alacritty", 4, Some("/new"), 150)]);
        let merged = merge(live, &prev, Duration::from_secs(600));
        assert_eq!(merged.windows.len(), 1, "cwd change must not create a phantom window");
        assert_eq!(merged.windows[0].term_cwd.as_deref(), Some("/new"));
    }
}
