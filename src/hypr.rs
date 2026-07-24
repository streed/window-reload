//! Thin wrapper over Hyprland IPC.
//!
//! Queries and dispatches go through the `hyprctl` binary (guaranteed present and
//! correct for the running compositor); the recorder reads the event socket
//! (`.socket2.sock`) directly.

use crate::model::{Client, Monitor, Workspace};
use crate::R;
use std::path::PathBuf;
use std::process::Command;

fn hypr_runtime_dir() -> R<PathBuf> {
    let rt = std::env::var("XDG_RUNTIME_DIR")
        .map_err(|_| "XDG_RUNTIME_DIR is not set (are you inside a graphical session?)")?;
    Ok(PathBuf::from(rt).join("hypr"))
}

/// The signature of the currently-running Hyprland instance.
///
/// Prefers `HYPRLAND_INSTANCE_SIGNATURE` when its socket is live; otherwise scans
/// `$XDG_RUNTIME_DIR/hypr/*` for the most recently-active instance that still has a
/// command socket. This lets the recorder run under systemd (where the env var may
/// be absent or stale) and reconnect after Hyprland restarts.
pub fn active_signature() -> R<String> {
    let base = hypr_runtime_dir()?;

    if let Ok(sig) = std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        if !sig.is_empty() && base.join(&sig).join(".socket.sock").exists() {
            return Ok(sig);
        }
    }

    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(&base)? {
        let entry = entry?;
        let path = entry.path();
        if !path.join(".socket.sock").exists() {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let name = entry.file_name().to_string_lossy().into_owned();
        if best.as_ref().is_none_or(|(bm, _)| mtime >= *bm) {
            best = Some((mtime, name));
        }
    }
    best.map(|(_, n)| n)
        .ok_or_else(|| "no running Hyprland instance found".into())
}

/// Resolve the active instance and export it as `HYPRLAND_INSTANCE_SIGNATURE` so
/// child `hyprctl` invocations target the right compositor. Returns the signature.
pub fn ensure_instance_env() -> R<String> {
    let sig = active_signature()?;
    std::env::set_var("HYPRLAND_INSTANCE_SIGNATURE", &sig);
    Ok(sig)
}

/// Path to the Hyprland event socket (`.socket2.sock`) of the active instance.
pub fn event_socket_path() -> R<PathBuf> {
    let sig = active_signature()?;
    Ok(hypr_runtime_dir()?.join(sig).join(".socket2.sock"))
}

fn json<T: serde::de::DeserializeOwned>(query: &[&str]) -> R<T> {
    let out = Command::new("hyprctl").arg("-j").args(query).output()?;
    if !out.status.success() {
        return Err(format!(
            "hyprctl {:?} failed: {}",
            query,
            String::from_utf8_lossy(&out.stderr).trim()
        )
        .into());
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

pub fn clients() -> R<Vec<Client>> {
    json(&["clients"])
}

pub fn workspaces() -> R<Vec<Workspace>> {
    json(&["workspaces"])
}

pub fn monitors() -> R<Vec<Monitor>> {
    json(&["monitors"])
}

/// Read an integer option (e.g. `dwindle:force_split`).
pub fn get_option_int(key: &str) -> R<i64> {
    #[derive(serde::Deserialize)]
    struct O {
        int: i64,
    }
    let o: O = json(&["getoption", key])?;
    Ok(o.int)
}

/// Run `hyprctl dispatch <dispatcher> <arg>` and return trimmed stdout.
///
/// `arg` is passed as a single argument (no shell involved), matching how
/// `hyprctl` concatenates dispatcher arguments.
pub fn dispatch(dispatcher: &str, arg: &str) -> R<String> {
    let mut c = Command::new("hyprctl");
    c.arg("dispatch").arg(dispatcher);
    if !arg.is_empty() {
        c.arg(arg);
    }
    let out = c.output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Run `hyprctl keyword <key> <value>`.
pub fn keyword(key: &str, value: &str) -> R<String> {
    let out = Command::new("hyprctl")
        .arg("keyword")
        .arg(key)
        .arg(value)
        .output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Convenience: focus a window by address.
pub fn focus(addr: &str) -> R<String> {
    dispatch("focuswindow", &format!("address:{addr}"))
}
