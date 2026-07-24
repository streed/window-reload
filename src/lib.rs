//! window-reload: record and restore Hyprland window layout across restarts.
//!
//! The crate is split into small modules:
//! - [`model`]  – Hyprland IPC JSON structs and our persisted snapshot schema.
//! - [`hypr`]   – talking to Hyprland (hyprctl queries/dispatches + event socket path).
//! - [`proc`]   – reading `/proc` to recover terminal working directories and launch argv.
//! - [`launch`] – classifying a window and deriving the command that recreates it.
//! - [`layout`] – reconstructing a workspace's dwindle BSP tree from window geometry.
//! - [`capture`]– building a [`model::Snapshot`] from the live Hyprland state.
//! - [`state`]  – atomically persisting/loading the snapshot under `$XDG_STATE_HOME`.
//! - [`restore`]– recreating the saved layout by spawning + placing windows.
//! - [`daemon`] – the long-running recorder that snapshots on relevant events.

pub mod capture;
pub mod daemon;
pub mod hypr;
pub mod launch;
pub mod layout;
pub mod model;
pub mod proc;
pub mod restore;
pub mod state;

/// Convenience result type used across the crate.
pub type R<T> = Result<T, Box<dyn std::error::Error>>;

/// The current on-disk snapshot schema version.
pub const SNAPSHOT_VERSION: u32 = 1;

/// Seconds since the Unix epoch (best-effort; 0 on failure).
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
