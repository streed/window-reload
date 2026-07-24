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
/// A previous window is considered still-present if a live window shares its
/// address (same window) or — for windows recreated with a new address, e.g. after
/// a compositor restart — its signature. Everything else is "recently closed" and
/// kept until it ages out.
pub fn merge(mut live: Snapshot, prev: &Snapshot, keep_closed: Duration) -> Snapshot {
    let now = live.captured_at_unix;
    let ttl = keep_closed.as_secs();

    let live_addrs: HashSet<&str> = live.windows.iter().map(|w| w.address.as_str()).collect();
    let prev_addrs: HashSet<&str> = prev.windows.iter().map(|w| w.address.as_str()).collect();

    // Signatures of live windows whose address is new since `prev` — these can
    // absorb a previous entry whose window was recreated under a new address.
    let mut new_live_sig: HashMap<String, usize> = HashMap::new();
    for w in &live.windows {
        if !prev_addrs.contains(w.address.as_str()) {
            *new_live_sig.entry(w.signature()).or_insert(0) += 1;
        }
    }

    let mut carried: Vec<SavedWindow> = Vec::new();
    for pw in &prev.windows {
        if live_addrs.contains(pw.address.as_str()) {
            continue; // same window still open; the live copy is fresher
        }
        if let Some(c) = new_live_sig.get_mut(&pw.signature()) {
            if *c > 0 {
                *c -= 1; // recreated with a new address; represented by a live window
                continue;
            }
        }
        if now.saturating_sub(pw.last_seen_unix) <= ttl {
            carried.push(pw.clone()); // recently closed — remember it
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

    let (app_id, profile) = if kind == WindowKind::ChromeApp {
        match launch::parse_chrome_app(&c.class) {
            Some((a, p)) => (Some(a), Some(p)),
            None => (None, None),
        }
    } else {
        (None, None)
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
        exe: proc::exe(c.pid),
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
    fn cwd_change_same_address_updates_not_duplicates() {
        // User cd'd: same window (same address), different signature. No phantom.
        let prev = snap(100, vec![win("0xA", "Alacritty", 4, Some("/old"), 100)]);
        let live = snap(150, vec![win("0xA", "Alacritty", 4, Some("/new"), 150)]);
        let merged = merge(live, &prev, Duration::from_secs(600));
        assert_eq!(merged.windows.len(), 1, "cwd change must not create a phantom window");
        assert_eq!(merged.windows[0].term_cwd.as_deref(), Some("/new"));
    }
}
