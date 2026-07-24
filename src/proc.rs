//! Reading `/proc` to recover a window's launch command line and, for terminals,
//! the working directory of the shell (or foreground program) running inside it.

use std::collections::VecDeque;
use std::fs;

/// Shells whose working directory best represents "where the terminal is".
const SHELLS: &[&str] = &["zsh", "bash", "fish", "sh", "dash", "nu", "tcsh", "ksh", "elvish", "xonsh"];

/// Read `/proc/<pid>/cmdline` split into argv. Returns empty on failure.
pub fn cmdline(pid: i32) -> Vec<String> {
    match fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(bytes) => bytes
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Read `/proc/<pid>/comm` (the process's command name).
pub fn comm(pid: i32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Resolve `/proc/<pid>/exe` to the executable path.
pub fn exe(pid: i32) -> Option<String> {
    fs::read_link(format!("/proc/{pid}/exe"))
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Resolve `/proc/<pid>/cwd` to an existing directory. The kernel appends
/// " (deleted)" when the directory has been removed; such a path (and any that no
/// longer resolves to a directory) yields `None` so callers can fall back.
pub fn cwd(pid: i32) -> Option<String> {
    let p = fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let mut s = p.to_string_lossy().into_owned();
    if let Some(stripped) = s.strip_suffix(" (deleted)") {
        s = stripped.to_string();
    }
    if s.is_empty() || !std::path::Path::new(&s).is_dir() {
        return None;
    }
    Some(s)
}

/// Direct children of `pid` via `/proc/<pid>/task/<pid>/children`.
pub fn children(pid: i32) -> Vec<i32> {
    fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .ok()
        .map(|s| s.split_whitespace().filter_map(|t| t.parse().ok()).collect())
        .unwrap_or_default()
}

/// All descendants of `pid` (breadth-first), paired with their depth (children = 1).
pub fn descendants(pid: i32) -> Vec<(i32, usize)> {
    let mut out = Vec::new();
    let mut q: VecDeque<(i32, usize)> = children(pid).into_iter().map(|c| (c, 1)).collect();
    // Guard against pathological trees.
    let mut budget = 4096;
    while let Some((c, depth)) = q.pop_front() {
        if budget == 0 {
            break;
        }
        budget -= 1;
        out.push((c, depth));
        for gc in children(c) {
            q.push_back((gc, depth + 1));
        }
    }
    out
}

/// Best-effort "directory this terminal is in".
///
/// Walks the process subtree below the terminal's window pid and prefers the
/// working directory of the deepest shell process; failing that, the deepest
/// descendant with a readable cwd; failing that, the terminal pid's own cwd.
pub fn resolve_terminal_cwd(window_pid: i32) -> Option<String> {
    let mut best_shell: Option<(usize, String)> = None;
    let mut best_any: Option<(usize, String)> = None;

    for (pid, depth) in descendants(window_pid) {
        let Some(dir) = cwd(pid) else { continue };
        let is_shell = comm(pid)
            .map(|c| SHELLS.contains(&c.as_str()))
            .unwrap_or(false);
        if is_shell && best_shell.as_ref().is_none_or(|(d, _)| depth >= *d) {
            best_shell = Some((depth, dir.clone()));
        }
        if best_any.as_ref().is_none_or(|(d, _)| depth >= *d) {
            best_any = Some((depth, dir));
        }
    }

    best_shell
        .map(|(_, d)| d)
        .or(best_any.map(|(_, d)| d))
        .or_else(|| cwd(window_pid))
}
