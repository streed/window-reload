//! Reading Chromium/Chrome on-disk state to recover the one thing the window
//! manager and `/proc` cannot provide: which **profile** a browser window belongs
//! to.
//!
//! A single Chrome browser process serves *every* window of a Chrome instance
//! across *all* profiles, and it rewrites its own argv, so neither the window pid
//! nor its command line reveals the profile. The compositor only ever sees the
//! class `google-chrome`. We recover the profile by searching each profile's
//! current session file for the window's title — a window's tabs live only in its
//! own profile's session, so a title match uniquely identifies the owner.
//!
//! Tabs themselves are *not* captured: restore just relaunches each profile and
//! lets Chrome's built-in "continue where you left off" restore its own windows
//! and tabs.
//!
//! Everything here is best-effort: any read failure yields `None`, so capture
//! degrades to a blank, default-profile window rather than failing.

use std::fs;
use std::path::{Path, PathBuf};

/// Map a browser window class to its on-disk config directory, if known.
pub fn config_dir_for_class(class: &str) -> Option<PathBuf> {
    let sub = match class.to_ascii_lowercase().as_str() {
        "google-chrome" | "google-chrome-stable" => "google-chrome",
        "google-chrome-beta" => "google-chrome-beta",
        "google-chrome-unstable" => "google-chrome-unstable",
        "chromium" | "chromium-browser" => "chromium",
        "brave-browser" | "brave" => "BraveSoftware/Brave-Browser",
        _ => return None,
    };
    config_subdir(sub)
}

/// Map an executable path (e.g. `/opt/google/chrome/chrome`) to its config dir.
/// Used for PWA windows, whose class (`chrome-<appid>-<profile>`) does not name the
/// browser family.
pub fn config_dir_for_exe(exe: &str) -> Option<PathBuf> {
    let base = exe.rsplit('/').next().unwrap_or(exe).to_ascii_lowercase();
    let class = if base.contains("chromium") {
        "chromium"
    } else if base.contains("brave") {
        "brave-browser"
    } else if base.contains("chrome") {
        "google-chrome"
    } else {
        return None;
    };
    config_dir_for_class(class)
}

fn config_subdir(sub: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var("HOME").ok()?).join(".config").join(sub);
    dir.is_dir().then_some(dir)
}

/// Profiles present in a config dir, as `(folder, display_name)` pairs read from
/// `Local State` → `profile.info_cache`. Falls back to scanning for `Default` /
/// `Profile *` directories if `Local State` is unreadable.
pub fn profiles(config_dir: &Path) -> Vec<(String, String)> {
    if let Ok(bytes) = fs::read(config_dir.join("Local State")) {
        if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            if let Some(cache) = v.pointer("/profile/info_cache").and_then(|c| c.as_object()) {
                let mut out: Vec<(String, String)> = cache
                    .iter()
                    .map(|(folder, info)| {
                        let name = info
                            .get("name")
                            .and_then(|n| n.as_str())
                            .unwrap_or(folder)
                            .to_string();
                        (folder.clone(), name)
                    })
                    .collect();
                out.sort();
                if !out.is_empty() {
                    return out;
                }
            }
        }
    }
    // Fallback: directory scan.
    let mut out = Vec::new();
    if let Ok(rd) = fs::read_dir(config_dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if (name == "Default" || name.starts_with("Profile "))
                && e.path().join("Preferences").exists()
            {
                out.push((name.clone(), name));
            }
        }
    }
    out.sort();
    out
}

/// Reverse Chrome's WM_CLASS sanitization (`[^A-Za-z0-9_-]` → `_`) of a profile
/// segment back to the real on-disk directory name, e.g. `Profile_1` → `Profile 1`.
/// Returns the input unchanged if no directory matches.
pub fn desanitize_profile(config_dir: &Path, class_segment: &str) -> String {
    for (folder, _) in profiles(config_dir) {
        if sanitize(&folder) == class_segment {
            return folder;
        }
    }
    class_segment.to_string()
}

/// Chrome's class sanitization: any byte outside `[A-Za-z0-9_-]` becomes `_`.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
        .collect()
}

/// The newest `Sessions/Session_*` file for a profile (its current session).
fn newest_session_file(config_dir: &Path, profile: &str) -> Option<PathBuf> {
    let dir = config_dir.join(profile).join("Sessions");
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    for e in fs::read_dir(&dir).ok()?.flatten() {
        let p = e.path();
        if !p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("Session_"))
        {
            continue;
        }
        let Ok(mtime) = e.metadata().and_then(|m| m.modified()) else { continue };
        if best.as_ref().is_none_or(|(t, _)| mtime >= *t) {
            best = Some((mtime, p));
        }
    }
    best.map(|(_, p)| p)
}

/// Determine which profile owns a browser window, given its title.
///
/// A window's tab titles are stored (as UTF-16) in its own profile's session file
/// and nowhere else, so the window title appears in exactly one profile's session.
/// We byte-search each profile's current session for the title; a unique match
/// names the owner. Returns `None` when the title is too generic, matches no
/// profile, or matches more than one (ambiguous → let restore use the default).
pub fn find_profile(config_dir: &Path, title: &str) -> Option<String> {
    let needle = normalize_title(title);
    if needle.len() < 3 {
        return None; // too generic (e.g. "New Tab") to attribute
    }
    let needle_u16: Vec<u8> = needle.encode_utf16().flat_map(u16::to_le_bytes).collect();

    let mut found: Option<String> = None;
    for (folder, _) in profiles(config_dir) {
        let Some(path) = newest_session_file(config_dir, &folder) else { continue };
        let Ok(bytes) = fs::read(&path) else { continue };
        if contains(&bytes, &needle_u16) || contains(&bytes, needle.as_bytes()) {
            if found.is_some() {
                return None; // ambiguous
            }
            found = Some(folder);
        }
    }
    found
}

/// Strip a trailing browser suffix (" - Google Chrome", " — Chromium", …) so the
/// window title reduces to the active tab's page title (as stored in the session).
fn normalize_title(title: &str) -> String {
    const SUFFIXES: &[&str] = &[
        " - Google Chrome",
        " — Google Chrome",
        " - Chromium",
        " — Chromium",
        " - Brave",
        " — Brave",
    ];
    let mut t = title.trim();
    for suf in SUFFIXES {
        if let Some(stripped) = t.strip_suffix(suf) {
            t = stripped.trim_end();
            break;
        }
    }
    t.to_string()
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_matches_chrome() {
        assert_eq!(sanitize("Profile 1"), "Profile_1");
        assert_eq!(sanitize("Default"), "Default");
        assert_eq!(sanitize("Work/Home"), "Work_Home");
    }

    #[test]
    fn normalize_strips_suffix() {
        assert_eq!(normalize_title("Foo - Google Chrome"), "Foo");
        assert_eq!(normalize_title("Bar"), "Bar");
    }

    #[test]
    fn contains_finds_utf16_needle() {
        let hay: Vec<u8> = "xxBeamlinkxx".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let needle: Vec<u8> = "Beamlink".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(contains(&hay, &needle));
        let miss: Vec<u8> = "Nope".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert!(!contains(&hay, &miss));
    }
}
