//! Classifying windows and turning a [`SavedWindow`] back into a command line.

use crate::model::{SavedWindow, WindowKind};

/// Known terminal emulators, matched case-insensitively against the window class,
/// paired with the flag they use to set the initial working directory.
const TERMINALS: &[(&str, &str)] = &[
    ("alacritty", "--working-directory"),
    ("kitty", "--directory"),
    ("foot", "--working-directory"),
    ("footclient", "--working-directory"),
    ("wezterm", "--cwd"),
    ("org.wezfurlong.wezterm", "--cwd"),
    ("ghostty", "--working-directory"),
    ("com.mitchellh.ghostty", "--working-directory"),
    ("konsole", "--workdir"),
    ("st", "-d"),
    ("xterm", ""),
    ("urxvt", "-cd"),
];

/// Chromium-family main-browser classes.
const CHROME_MAIN: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "google-chrome-beta",
    "chromium",
    "chromium-browser",
    "brave-browser",
];

fn terminal_entry(class: &str) -> Option<(&'static str, &'static str)> {
    let lc = class.to_ascii_lowercase();
    TERMINALS
        .iter()
        .find(|(name, _)| lc == *name)
        .map(|(n, f)| (*n, *f))
}

/// Is this window class a terminal emulator?
pub fn is_terminal(class: &str, initial_class: &str) -> bool {
    terminal_entry(class).is_some() || terminal_entry(initial_class).is_some()
}

/// Parse a Chromium PWA app-window class of the form `chrome-<appid>-<profile>`.
/// Returns `(app_id, profile)`.
pub fn parse_chrome_app(class: &str) -> Option<(String, String)> {
    let rest = class.strip_prefix("chrome-")?;
    // profile is the final `-`-delimited segment; the rest is the app id.
    let idx = rest.rfind('-')?;
    let (app_id, profile) = rest.split_at(idx);
    let profile = &profile[1..];
    if app_id.is_empty() || profile.is_empty() {
        return None;
    }
    Some((app_id.to_string(), profile.to_string()))
}

/// Classify a window into a [`WindowKind`].
pub fn classify(class: &str, initial_class: &str) -> WindowKind {
    if is_terminal(class, initial_class) {
        return WindowKind::Terminal;
    }
    if parse_chrome_app(class).is_some() {
        return WindowKind::ChromeApp;
    }
    let lc = class.to_ascii_lowercase();
    if CHROME_MAIN.contains(&lc.as_str()) {
        return WindowKind::Chrome;
    }
    WindowKind::Generic
}

/// Quote a single argument for a POSIX shell (`/bin/sh -c`), which is how
/// Hyprland's `exec` dispatcher runs the command.
pub fn sh_quote(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_./:=@%+,-".contains(&b))
    {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// Join an argv into a single shell-safe command string.
pub fn sh_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| sh_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// A clean chrome/chromium/brave binary to relaunch with.
///
/// The recorded argv is unusable for Chrome: the browser rewrites its own argv into
/// a single space-joined blob (shared by every window and profile, and sometimes
/// carrying an unrelated window's `--app-id`), so replaying it either runs a bogus
/// command or reopens the wrong thing. We resolve the binary from `/proc/<pid>/exe`
/// (captured as `exe`) instead, and only accept a real, space-free path; otherwise
/// we fall back to a stable launcher name.
fn resolve_chrome_bin(w: &SavedWindow) -> String {
    if let Some(exe) = &w.exe {
        if !exe.contains(' ') && std::path::Path::new(exe).exists() {
            let base = exe.rsplit('/').next().unwrap_or(exe).to_ascii_lowercase();
            if base.contains("chrome") || base.contains("chromium") || base.contains("brave") {
                return exe.clone();
            }
        }
    }
    // Infer a stable launcher from the class / exe family.
    let hay = format!("{} {}", w.class, w.exe.as_deref().unwrap_or("")).to_ascii_lowercase();
    if hay.contains("chromium") {
        "chromium".to_string()
    } else if hay.contains("brave") {
        "brave-browser".to_string()
    } else {
        "google-chrome-stable".to_string()
    }
}

/// The command that should recreate this window, as a shell command string
/// (to be placed after a `[workspace N silent]` rule).
///
/// `respect_terminal_command` is currently always false (the user opted for
/// "reopen at directory only"); it is threaded through for future configurability.
pub fn command_string(w: &SavedWindow) -> String {
    match w.kind {
        WindowKind::Terminal => {
            let entry = terminal_entry(&w.class).or_else(|| terminal_entry(&w.initial_class));
            let (bin, flag) = entry.unwrap_or(("alacritty", "--working-directory"));
            let mut argv = vec![bin.to_string()];
            // Only pass the directory if it still exists — otherwise some terminals
            // refuse to start, which would stall restore. Fall back to $HOME.
            if let (Some(dir), false) = (w.term_cwd.as_ref(), flag.is_empty()) {
                if std::path::Path::new(dir).is_dir() {
                    argv.push(flag.to_string());
                    argv.push(dir.clone());
                } else {
                    eprintln!("window-reload: saved terminal dir '{dir}' is gone; opening at home");
                }
            }
            sh_join(&argv)
        }
        WindowKind::ChromeApp => {
            // `profile` is the real on-disk directory, desanitized at capture time.
            let profile = w.profile.as_deref().unwrap_or("Default");
            let mut argv = vec![resolve_chrome_bin(w), format!("--profile-directory={profile}")];
            if let Some(app_id) = &w.app_id {
                argv.push(format!("--app-id={app_id}"));
            }
            sh_join(&argv)
        }
        WindowKind::Chrome => {
            // Don't replay argv (see resolve_chrome_bin). Just launch the right
            // profile; Chrome's own "continue where you left off" restores that
            // profile's windows and tabs. A bare per-profile launch (no
            // `--new-window`) is what triggers that session restore.
            let mut argv = vec![resolve_chrome_bin(w)];
            if let Some(profile) = &w.profile {
                argv.push(format!("--profile-directory={profile}"));
            }
            sh_join(&argv)
        }
        WindowKind::Generic => {
            if w.cmdline.is_empty() {
                return w.exe.clone().unwrap_or_else(|| w.class.to_ascii_lowercase());
            }
            let argv = w.cmdline.clone();
            // If argv[0] is an absolute path that no longer exists (a per-launch
            // temp path such as an AppImage `/tmp/.mount_*` or a since-updated
            // snap/electron path), fall back to a stable launcher and drop the
            // now-meaningless args.
            let arg0 = &argv[0];
            if arg0.starts_with('/') && !std::path::Path::new(arg0).exists() {
                let stable = arg0
                    .rsplit('/')
                    .next()
                    .filter(|b| !b.is_empty())
                    .map(str::to_string)
                    .or_else(|| w.exe.as_ref().and_then(|e| e.rsplit('/').next().map(str::to_string)))
                    .unwrap_or_else(|| w.class.to_ascii_lowercase());
                return sh_quote(&stable);
            }
            sh_join(&argv)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WindowKind;

    /// A blank SavedWindow to tweak per test. `exe` is left None so
    /// `resolve_chrome_bin` deterministically falls back to a stable launcher.
    fn win(kind: WindowKind, class: &str) -> SavedWindow {
        SavedWindow {
            address: String::new(),
            last_seen_unix: 0,
            kind,
            class: class.into(),
            initial_class: class.into(),
            title: String::new(),
            initial_title: String::new(),
            xwayland: false,
            workspace_id: 1,
            workspace_name: "1".into(),
            monitor: 0,
            at: [0, 0],
            size: [100, 100],
            floating: false,
            fullscreen: 0,
            pinned: false,
            group_key: None,
            group_index: 0,
            pid: 0,
            cmdline: Vec::new(),
            exe: None,
            is_terminal: false,
            term_cwd: None,
            app_id: None,
            profile: None,
        }
    }

    #[test]
    fn chrome_main_window_launches_its_profile() {
        // Restore just relaunches the profile; Chrome restores its own tabs.
        let mut w = win(WindowKind::Chrome, "google-chrome");
        w.profile = Some("Profile 1".into());
        assert_eq!(
            command_string(&w),
            "google-chrome-stable '--profile-directory=Profile 1'"
        );
    }

    #[test]
    fn chrome_never_replays_rewritten_argv_blob() {
        // The poisoned single-element blob Chrome writes to /proc/<pid>/cmdline.
        let mut w = win(WindowKind::Chrome, "google-chrome");
        w.cmdline = vec!["/opt/google/chrome/chrome --app-id=pehnimlghmeel".into()];
        w.profile = Some("Default".into());
        let cmd = command_string(&w);
        assert!(!cmd.contains("--app-id"), "must not leak the rewritten blob: {cmd}");
        assert!(!cmd.contains("chrome --app-id"), "blob must not become a bogus binary: {cmd}");
        assert_eq!(cmd, "google-chrome-stable --profile-directory=Default");
    }

    #[test]
    fn chrome_without_profile_falls_back_to_bare_launch() {
        let w = win(WindowKind::Chrome, "google-chrome");
        assert_eq!(command_string(&w), "google-chrome-stable");
    }

    #[test]
    fn pwa_uses_desanitized_profile_and_app_id() {
        let mut w = win(WindowKind::ChromeApp, "chrome-pehnim-Profile_1");
        w.app_id = Some("pehnim".into());
        w.profile = Some("Profile 1".into()); // desanitized at capture
        assert_eq!(
            command_string(&w),
            "google-chrome-stable '--profile-directory=Profile 1' --app-id=pehnim"
        );
    }
}
