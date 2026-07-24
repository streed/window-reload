//! Data types: the subset of Hyprland's IPC JSON we consume, plus the schema we persist.

use serde::{Deserialize, Serialize};

/// A workspace reference as embedded in a client/monitor object.
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceRef {
    pub id: i64,
    #[serde(default)]
    pub name: String,
}

/// A window ("client") as reported by `hyprctl clients -j`.
#[derive(Debug, Clone, Deserialize)]
pub struct Client {
    pub address: String,
    pub at: [i64; 2],
    pub size: [i64; 2],
    pub workspace: WorkspaceRef,
    pub floating: bool,
    #[serde(default)]
    pub monitor: i64,
    pub class: String,
    #[serde(rename = "initialClass", default)]
    pub initial_class: String,
    #[serde(default)]
    pub title: String,
    #[serde(rename = "initialTitle", default)]
    pub initial_title: String,
    pub pid: i32,
    #[serde(default)]
    pub xwayland: bool,
    #[serde(default)]
    pub fullscreen: i64,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub mapped: bool,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub grouped: Vec<String>,
}

/// A workspace as reported by `hyprctl workspaces -j`.
#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub monitor: String,
    #[serde(rename = "monitorID", default)]
    pub monitor_id: i64,
    #[serde(rename = "ispersistent", default)]
    pub persistent: bool,
    #[serde(rename = "tiledLayout", default)]
    pub tiled_layout: String,
}

/// A monitor as reported by `hyprctl monitors -j`.
#[derive(Debug, Clone, Deserialize)]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub width: i64,
    pub height: i64,
    #[serde(default)]
    pub x: i64,
    #[serde(default)]
    pub y: i64,
    #[serde(rename = "activeWorkspace")]
    pub active_workspace: WorkspaceRef,
}

// ---------------------------------------------------------------------------
// Persisted schema
// ---------------------------------------------------------------------------

/// A complete snapshot of the desktop layout, persisted to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub captured_at_unix: u64,
    pub monitors: Vec<SavedMonitor>,
    pub workspaces: Vec<SavedWorkspace>,
    pub windows: Vec<SavedWindow>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedMonitor {
    pub id: i64,
    pub name: String,
    pub width: i64,
    pub height: i64,
    pub x: i64,
    pub y: i64,
    pub active_workspace_id: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub id: i64,
    pub name: String,
    pub monitor: String,
    pub monitor_id: i64,
    pub persistent: bool,
    pub layout: String,
}

/// How a window should be recreated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowKind {
    /// A terminal emulator; relaunched in its saved working directory.
    Terminal,
    /// A Chromium/Chrome PWA "app" window (class `chrome-<appid>-<profile>`).
    ChromeApp,
    /// A main Chromium/Chrome browser window.
    Chrome,
    /// Anything else; relaunched from its recorded argv.
    Generic,
}

/// A single window's saved state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedWindow {
    /// Hyprland window address at capture time. Stable while the window lives (so
    /// it identifies a window across snapshots within one session); changes when
    /// the window is destroyed and recreated. Used to merge snapshots.
    #[serde(default)]
    pub address: String,
    /// Unix time this window was last seen open. Equals the snapshot time for live
    /// windows; older for a remembered (recently-closed) one.
    #[serde(default)]
    pub last_seen_unix: u64,

    pub kind: WindowKind,
    pub class: String,
    pub initial_class: String,
    pub title: String,
    pub initial_title: String,
    pub xwayland: bool,

    // Placement.
    pub workspace_id: i64,
    pub workspace_name: String,
    pub monitor: i64,
    pub at: [i64; 2],
    pub size: [i64; 2],
    pub floating: bool,
    pub fullscreen: i64,
    pub pinned: bool,

    // Grouping (tabbed windows): members of the same group share `group_key`,
    // ordered by `group_index`.
    #[serde(default)]
    pub group_key: Option<String>,
    #[serde(default)]
    pub group_index: usize,

    // Relaunch data.
    pub pid: i32,
    pub cmdline: Vec<String>,
    #[serde(default)]
    pub exe: Option<String>,

    // Terminal specifics.
    pub is_terminal: bool,
    #[serde(default)]
    pub term_cwd: Option<String>,

    // Chrome specifics. `profile` is the real on-disk profile directory
    // (e.g. "Default", "Profile 1") for both main and PWA windows, so restore can
    // relaunch the right profile; Chrome's own session restore reopens the tabs.
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub profile: Option<String>,
}

/// An axis-aligned rectangle, used by the layout reconstruction.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
}

impl SavedWindow {
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.at[0],
            y: self.at[1],
            w: self.size[0],
            h: self.size[1],
        }
    }

    /// A signature used to detect whether an equivalent window is already open,
    /// so restore can be idempotent.
    pub fn signature(&self) -> String {
        signature_for(
            self.kind,
            &self.class,
            self.term_cwd.as_deref(),
            self.app_id.as_deref(),
            self.profile.as_deref(),
            &self.cmdline,
        )
    }
}

/// Build the idempotency signature from raw fields. Used both when reading a saved
/// window and when inspecting a live one, so the two can never drift apart.
///
/// A main Chrome window's signature includes its profile: every such window shares
/// the class `google-chrome`, so without the profile two windows from different
/// profiles would collide, and a single open Chrome window would suppress restore
/// of all the others.
pub fn signature_for(
    kind: WindowKind,
    class: &str,
    term_cwd: Option<&str>,
    app_id: Option<&str>,
    profile: Option<&str>,
    cmdline: &[String],
) -> String {
    match kind {
        WindowKind::Terminal => format!("term:{}:{}", class, term_cwd.unwrap_or("")),
        WindowKind::ChromeApp => {
            format!("chromeapp:{}:{}", app_id.unwrap_or(class), profile.unwrap_or(""))
        }
        WindowKind::Chrome => format!("chrome:{}:{}", class, profile.unwrap_or("")),
        WindowKind::Generic => format!("generic:{}:{}", class, cmdline.join(" ")),
    }
}
