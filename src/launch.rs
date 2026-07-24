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

/// The chrome/chromium binary to relaunch with, inferred from the recorded argv.
fn chrome_binary(w: &SavedWindow) -> String {
    if let Some(first) = w.cmdline.first() {
        let base = first.rsplit('/').next().unwrap_or(first);
        // Renderer/helper processes are never the window's launcher, but the window
        // pid is always the browser process, so cmdline[0] is the browser binary.
        if base.contains("chrome") || base.contains("chromium") || base.contains("brave") {
            return first.clone();
        }
    }
    "google-chrome-stable".to_string()
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
            let bin = chrome_binary(w);
            let profile = w.profile.as_deref().unwrap_or("Default");
            let mut argv = vec![bin, format!("--profile-directory={profile}")];
            if let Some(app_id) = &w.app_id {
                argv.push(format!("--app-id={app_id}"));
            }
            sh_join(&argv)
        }
        WindowKind::Chrome | WindowKind::Generic => {
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
