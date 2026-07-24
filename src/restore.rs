//! Recreate a saved layout: spawn windows onto their workspaces and place them.

use crate::layout::{self, Node, Orient};
use crate::model::*;
use crate::{hypr, launch, state, R};
use std::collections::{HashMap, HashSet};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Options controlling a restore run.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Print the plan without spawning or moving anything.
    pub dry_run: bool,
    /// Spawn windows even if an equivalent one already appears to be open.
    pub force: bool,
    /// Restrict restore to a single workspace id.
    pub only_workspace: Option<i64>,
}


/// How long to wait for a freshly spawned window to appear.
const SPAWN_TIMEOUT: Duration = Duration::from_secs(8);
const SPAWN_TIMEOUT_CHROME: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(100);

/// A window paired with its index in `Snapshot::windows` (a stable identity that,
/// unlike a signature, is unique even for two terminals in the same directory).
type Indexed<'a> = (usize, &'a SavedWindow);

/// While this guard is alive, a sentinel file tells the recorder daemon to hold
/// off snapshotting, so a half-spawned layout is never written over the saved one.
///
/// Acquire it *before* reading the saved snapshot (see `window-reload restore`) so
/// the daemon cannot overwrite `state.json` between load and restore.
pub struct RestoreLock;

impl RestoreLock {
    pub fn acquire() -> Self {
        if let Ok(p) = state::restore_lock_path() {
            let _ = std::fs::write(&p, std::process::id().to_string());
        }
        RestoreLock
    }
}

impl Drop for RestoreLock {
    fn drop(&mut self) {
        if let Ok(p) = state::restore_lock_path() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Restore the given snapshot into the running compositor.
pub fn restore(snap: &Snapshot, opts: &Options) -> R<()> {
    // Windows already open, keyed by signature, so restore is idempotent.
    // A dry run shows the full plan (as if on a fresh session), so it ignores
    // what is currently open.
    let existing: HashSet<String> = if opts.force || opts.dry_run {
        HashSet::new()
    } else {
        present_signatures()?
    };

    // Remember the currently focused workspace so we can return to it.
    let original_ws = hypr::monitors()
        .ok()
        .and_then(|ms| ms.first().map(|m| m.active_workspace.id));

    // Save and pin dwindle:force_split so spawn side is predictable, restore after.
    let saved_force_split = hypr::get_option_int("dwindle:force_split").unwrap_or(0);
    if !opts.dry_run {
        let _ = hypr::keyword("dwindle:force_split", "2"); // new window takes the second slot
    }

    // Group windows by workspace, keeping each window's snapshot index.
    let mut by_ws: HashMap<i64, Vec<Indexed>> = HashMap::new();
    for (i, w) in snap.windows.iter().enumerate() {
        if let Some(only) = opts.only_workspace {
            if w.workspace_id != only {
                continue;
            }
        }
        by_ws.entry(w.workspace_id).or_default().push((i, w));
    }

    // Deterministic workspace order.
    let mut ws_ids: Vec<i64> = by_ws.keys().copied().collect();
    ws_ids.sort();

    // (snapshot index, address) for every window we place.
    let mut placed: Vec<(usize, String)> = Vec::new();

    // A failing workspace must not abort the rest or skip the cleanup below.
    for ws_id in ws_ids {
        let windows = &by_ws[&ws_id];
        if let Err(e) = restore_workspace(ws_id, windows, &existing, opts, &mut placed) {
            eprintln!("window-reload: workspace {ws_id} restore error: {e}");
        }
    }

    if !opts.dry_run {
        // Restore groups (tabbed windows).
        if let Err(e) = restore_groups(snap, &placed) {
            eprintln!("window-reload: group restore error: {e}");
        }

        // Reset the split preference and return focus to where we started.
        let _ = hypr::keyword("dwindle:force_split", &saved_force_split.to_string());
        if let Some(ws) = original_ws {
            let _ = hypr::dispatch("workspace", &ws.to_string());
        }
    }

    Ok(())
}

/// Restore all windows for one workspace: tiled ones first (replaying the tree),
/// then floating ones.
fn restore_workspace(
    ws_id: i64,
    windows: &[Indexed],
    existing: &HashSet<String>,
    opts: &Options,
    placed: &mut Vec<(usize, String)>,
) -> R<()> {
    // Partition into tiled and floating, skipping ones already open (unless forced).
    let mut tiled: Vec<Indexed> = Vec::new();
    let mut floating: Vec<Indexed> = Vec::new();
    for &(idx, w) in windows {
        if existing.contains(&w.signature()) {
            log(opts, &format!("skip (already open): {}", describe(w)));
            continue;
        }
        if w.floating {
            floating.push((idx, w));
        } else {
            tiled.push((idx, w));
        }
    }

    // Reconstruct the tiling tree from saved geometry.
    let items: Vec<(usize, Rect)> = tiled.iter().enumerate().map(|(i, (_, w))| (i, w.rect())).collect();

    // addr_of[i] gets filled in as we spawn tiled window i.
    let mut addr_of: Vec<Option<String>> = vec![None; tiled.len()];

    if !tiled.is_empty() {
        let tree = layout::reconstruct(&items);
        if opts.dry_run {
            log(opts, &format!("workspace {ws_id}: tiled tree {}", render_tree(&tree, &tiled)));
        } else {
            // Spawn the whole subtree, replaying splits.
            let anchor = spawn_tiled(ws_id, tree.first_leaf(), &tiled, opts, placed)?;
            if let Some(anchor) = anchor {
                addr_of[tree.first_leaf()] = Some(anchor.clone());
                build_tree(&tree, &anchor, ws_id, &tiled, &mut addr_of, opts, placed)?;
            }
            // Converge sizes: apply saved pixel sizes leaf-by-leaf, a couple of passes.
            // Targets are mutually consistent, so this settles the dividers without fighting.
            // Fullscreen windows are excluded — their saved size is the whole monitor,
            // which would thrash their siblings; they get sized by apply_fullscreen.
            for _ in 0..2 {
                for (i, (_, w)) in tiled.iter().enumerate() {
                    if w.fullscreen != 0 {
                        continue;
                    }
                    if let Some(addr) = &addr_of[i] {
                        let _ = hypr::dispatch(
                            "resizewindowpixel",
                            &format!("exact {} {},address:{}", w.size[0], w.size[1], addr),
                        );
                    }
                }
            }
            // Fullscreen any tiled window that wants it.
            for (i, (_, w)) in tiled.iter().enumerate() {
                if w.fullscreen != 0 {
                    if let Some(addr) = &addr_of[i] {
                        apply_fullscreen(addr, w.fullscreen);
                    }
                }
            }
        }
    }

    // Floating windows: spawn, ensure floating, then exact size + position.
    for &(idx, w) in &floating {
        if opts.dry_run {
            log(opts, &format!(
                "workspace {ws_id}: float {} at {:?} size {:?}",
                describe(w), w.at, w.size
            ));
            continue;
        }
        if let Some(addr) = spawn_window(ws_id, idx, w, opts, placed)? {
            // A freshly spawned window is tiled unless a rule floats it; force floating.
            if !is_floating(&addr).unwrap_or(true) {
                let _ = hypr::dispatch("togglefloating", &format!("address:{addr}"));
            }
            let _ = hypr::dispatch(
                "resizewindowpixel",
                &format!("exact {} {},address:{}", w.size[0], w.size[1], addr),
            );
            let _ = hypr::dispatch(
                "movewindowpixel",
                &format!("exact {} {},address:{}", w.at[0], w.at[1], addr),
            );
            if w.fullscreen != 0 {
                apply_fullscreen(&addr, w.fullscreen);
            }
        }
    }

    Ok(())
}

/// Recursively realize a tiling subtree whose region is currently occupied by the
/// single window `anchor_addr`. For each split we spawn one window (which dwindle
/// places into the second slot), correct its orientation, then recurse.
fn build_tree(
    node: &Node,
    anchor_addr: &str,
    ws_id: i64,
    tiled: &[Indexed],
    addr_of: &mut Vec<Option<String>>,
    opts: &Options,
    placed: &mut Vec<(usize, String)>,
) -> R<()> {
    match node {
        Node::Leaf(i) => {
            addr_of[*i] = Some(anchor_addr.to_string());
            Ok(())
        }
        Node::Split { orient, first, second } => {
            // The first child inherits the anchor; the second child gets a new window.
            let second_leaf = second.first_leaf();
            hypr::focus(anchor_addr)?;
            let Some(second_addr) = spawn_tiled(ws_id, second_leaf, tiled, opts, placed)? else {
                // Spawn failed; still record the first side so sizing can proceed.
                return build_tree(first, anchor_addr, ws_id, tiled, addr_of, opts, placed);
            };
            addr_of[second_leaf] = Some(second_addr.clone());
            ensure_orientation(anchor_addr, &second_addr, *orient);

            build_tree(first, anchor_addr, ws_id, tiled, addr_of, opts, placed)?;
            build_tree(second, &second_addr, ws_id, tiled, addr_of, opts, placed)?;
            Ok(())
        }
    }
}

/// Spawn the window for tiled leaf `i` and return its address.
fn spawn_tiled(
    ws_id: i64,
    i: usize,
    tiled: &[Indexed],
    opts: &Options,
    placed: &mut Vec<(usize, String)>,
) -> R<Option<String>> {
    let (idx, w) = tiled[i];
    spawn_window(ws_id, idx, w, opts, placed)
}

/// Spawn one window onto `ws_id` (silently) and wait for it to appear.
fn spawn_window(
    ws_id: i64,
    idx: usize,
    w: &SavedWindow,
    opts: &Options,
    placed: &mut Vec<(usize, String)>,
) -> R<Option<String>> {
    let cmd = launch::command_string(w);
    let exec = format!("[workspace {ws_id} silent] {cmd}");
    log(opts, &format!("spawn ws{ws_id}: {}  ->  {cmd}", describe(w)));

    let before = address_set()?;
    // Windows we already grabbed this run must never be re-claimed (e.g. by a
    // later spawn of the same class after an earlier one timed out).
    let claimed: HashSet<String> = placed.iter().map(|(_, a)| a.clone()).collect();
    hypr::dispatch("exec", &exec)?;

    let timeout = match w.kind {
        WindowKind::Chrome | WindowKind::ChromeApp => SPAWN_TIMEOUT_CHROME,
        _ => SPAWN_TIMEOUT,
    };
    let addr = wait_for_new_window(&before, &claimed, ws_id, w, timeout);
    match &addr {
        Some(a) => {
            // Chrome forwards a new-window request to its already-running browser
            // process, so the window can be created by that pre-existing pid and
            // escape the `[workspace N silent]` spawn rule — landing on the current
            // workspace instead. Place Chrome/PWA windows explicitly.
            if matches!(w.kind, WindowKind::Chrome | WindowKind::ChromeApp) {
                let _ = hypr::dispatch("movetoworkspacesilent", &format!("{ws_id},address:{a}"));
            }
            placed.push((idx, a.clone()));
        }
        None => log(opts, &format!("  (timed out waiting for window: {})", describe(w))),
    }
    Ok(addr)
}

/// Poll for a newly appeared window that matches `w`, ignoring any address we have
/// already claimed this run and preferring a match on the target workspace.
fn wait_for_new_window(
    before: &HashSet<String>,
    claimed: &HashSet<String>,
    ws_id: i64,
    w: &SavedWindow,
    timeout: Duration,
) -> Option<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(clients) = hypr::clients() {
            let newcomers: Vec<&Client> = clients
                .iter()
                .filter(|c| c.mapped && !before.contains(&c.address) && !claimed.contains(&c.address))
                .collect();
            // Best: a class match already sitting on the target workspace.
            if let Some(c) = newcomers
                .iter()
                .find(|c| class_matches(c, w) && c.workspace.id == ws_id)
            {
                return Some(c.address.clone());
            }
            // Next: any class match.
            if let Some(c) = newcomers.iter().find(|c| class_matches(c, w)) {
                return Some(c.address.clone());
            }
            // Fallback: exactly one unclaimed newcomer.
            if newcomers.len() == 1 {
                return Some(newcomers[0].address.clone());
            }
        }
        if Instant::now() >= deadline {
            return None;
        }
        sleep(POLL);
    }
}

fn class_matches(c: &Client, w: &SavedWindow) -> bool {
    if c.class == w.class || c.initial_class == w.initial_class {
        return true;
    }
    // PWA windows settle into their `chrome-<appid>-<profile>` class once the app id
    // is known. When both classes parse as PWA classes, require the app id AND the
    // profile segment to match, so a same-app window on a *different* profile is not
    // mistakenly claimed.
    if let (Some((wa, wp)), Some((ca, cp))) =
        (launch::parse_chrome_app(&w.class), launch::parse_chrome_app(&c.class))
    {
        return wa == ca && wp == cp;
    }
    // Before the class settles (still a generic `google-chrome`), fall back to an
    // app-id match as a last resort.
    if let Some(app_id) = &w.app_id {
        return c.class.contains(app_id);
    }
    false
}

/// Flip the split between `a` and `b` if its live orientation differs from `want`.
fn ensure_orientation(a: &str, b: &str, want: Orient) {
    let Ok(clients) = hypr::clients() else { return };
    let find = |addr: &str| clients.iter().find(|c| c.address == addr).map(|c| Rect {
        x: c.at[0], y: c.at[1], w: c.size[0], h: c.size[1],
    });
    let (Some(ra), Some(rb)) = (find(a), find(b)) else { return };
    if layout::actual_orientation(ra, rb) != want {
        let _ = hypr::focus(b);
        let _ = hypr::dispatch("layoutmsg", "togglesplit");
    }
}

fn apply_fullscreen(addr: &str, mode: i64) {
    let _ = hypr::focus(addr);
    // Client `fullscreen`: 1 = fullscreen, 2 = maximize -> dispatch arg 0 / 1.
    let arg = if mode == 2 { "1" } else { "0" };
    let _ = hypr::dispatch("fullscreen", arg);
}

/// Best-effort reunification of tabbed groups: focus each group's first member,
/// make it a group, then pull the rest in.
fn restore_groups(snap: &Snapshot, placed: &[(usize, String)]) -> R<()> {
    // Map snapshot index -> address for quick lookup.
    let addr_of: HashMap<usize, &str> =
        placed.iter().map(|(i, a)| (*i, a.as_str())).collect();

    // Collect groups: group_key -> list of (group_index, snapshot index).
    let mut groups: HashMap<&str, Vec<(usize, usize)>> = HashMap::new();
    for (i, w) in snap.windows.iter().enumerate() {
        if let Some(key) = &w.group_key {
            groups.entry(key.as_str()).or_default().push((w.group_index, i));
        }
    }

    for (_key, mut members) in groups {
        if members.len() < 2 {
            continue;
        }
        members.sort_by_key(|(gi, _)| *gi);
        let addrs: Vec<&str> = members
            .iter()
            .filter_map(|(_, i)| addr_of.get(i).copied())
            .collect();
        if addrs.len() < 2 {
            continue;
        }
        // Make the first a group of one, then move the neighbours into it.
        let _ = hypr::focus(addrs[0]);
        let _ = hypr::dispatch("togglegroup", "");
        for a in &addrs[1..] {
            let _ = hypr::focus(a);
            // Try each direction; only the correct adjacency succeeds.
            for dir in ["l", "r", "u", "d"] {
                let _ = hypr::dispatch("moveintogroup", dir);
            }
        }
    }
    Ok(())
}

// --- small helpers -------------------------------------------------------

fn present_signatures() -> R<HashSet<String>> {
    let clients = hypr::clients()?;
    let mut set = HashSet::new();
    for c in &clients {
        if !c.mapped || c.hidden || c.workspace.id <= 0 {
            continue;
        }
        let kind = launch::classify(&c.class, &c.initial_class);
        // Build the same fields capture would, then hash them through the shared
        // signature function so live/saved signatures can never diverge.
        let term_cwd = if kind == WindowKind::Terminal {
            crate::proc::resolve_terminal_cwd(c.pid)
        } else {
            None
        };
        let app_id = if kind == WindowKind::ChromeApp {
            launch::parse_chrome_app(&c.class).map(|(a, _)| a)
        } else {
            None
        };
        // Profile must be recovered the same way capture does, or a live Chrome
        // window's signature would not match its saved counterpart.
        let profile = match kind {
            WindowKind::ChromeApp => launch::parse_chrome_app(&c.class).map(|(_, seg)| {
                crate::proc::exe(c.pid)
                    .as_deref()
                    .and_then(crate::chrome::config_dir_for_exe)
                    .map(|dir| crate::chrome::desanitize_profile(&dir, &seg))
                    .unwrap_or(seg)
            }),
            WindowKind::Chrome => crate::chrome::config_dir_for_class(&c.class)
                .and_then(|dir| crate::chrome::find_profile(&dir, &c.title)),
            _ => None,
        };
        let cmdline = if kind == WindowKind::Generic {
            crate::proc::cmdline(c.pid)
        } else {
            Vec::new()
        };
        set.insert(crate::model::signature_for(
            kind,
            &c.class,
            term_cwd.as_deref(),
            app_id.as_deref(),
            profile.as_deref(),
            &cmdline,
        ));
    }
    Ok(set)
}

fn address_set() -> R<HashSet<String>> {
    Ok(hypr::clients()?.into_iter().map(|c| c.address).collect())
}

fn is_floating(addr: &str) -> R<bool> {
    Ok(hypr::clients()?
        .iter()
        .find(|c| c.address == addr)
        .map(|c| c.floating)
        .unwrap_or(false))
}

fn describe(w: &SavedWindow) -> String {
    match w.kind {
        WindowKind::Terminal => format!(
            "{} [{}]",
            w.class,
            w.term_cwd.as_deref().unwrap_or("~")
        ),
        _ => w.class.clone(),
    }
}

fn render_tree(node: &Node, tiled: &[Indexed]) -> String {
    match node {
        Node::Leaf(i) => describe(tiled[*i].1),
        Node::Split { orient, first, second } => {
            let sep = match orient {
                Orient::Vertical => " | ",
                Orient::Horizontal => " / ",
            };
            format!("({}{}{})", render_tree(first, tiled), sep, render_tree(second, tiled))
        }
    }
}

fn log(opts: &Options, msg: &str) {
    if opts.dry_run {
        println!("[dry-run] {msg}");
    } else {
        println!("{msg}");
    }
}
