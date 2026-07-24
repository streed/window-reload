//! The user-facing CLI: restore, save, status, path, install.

use window_reload::restore::Options;
use window_reload::{daemon, restore, state, R};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("help");

    let result = match cmd {
        "restore" => cmd_restore(&args[1..]),
        "save" => cmd_save(),
        "status" => cmd_status(),
        "path" => cmd_path(),
        "install" => cmd_install(&args[1..]),
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        other => {
            eprintln!("window-reload: unknown command '{other}'\n");
            print_help();
            std::process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("window-reload: {e}");
        std::process::exit(1);
    }
}

fn cmd_restore(args: &[String]) -> R<()> {
    let mut opts = Options::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-n" | "--dry-run" => opts.dry_run = true,
            "-f" | "--force" => opts.force = true,
            "--only" => {
                opts.only_workspace = it.next().and_then(|v| v.parse::<i64>().ok());
                if opts.only_workspace.is_none() {
                    return Err("--only requires a numeric workspace id".into());
                }
            }
            other => return Err(format!("unknown restore option '{other}'").into()),
        }
    }

    // Hold off the recorder *before* reading state, so its snapshots can't
    // overwrite state.json between load and restore (they race at login).
    let _lock = if opts.dry_run {
        None
    } else {
        Some(restore::RestoreLock::acquire())
    };

    let snap = state::load()?;
    let when = format_age(snap.captured_at_unix);
    eprintln!(
        "Restoring {} window(s) across {} workspace(s) (snapshot {}){}",
        snap.windows.len(),
        distinct_workspaces(&snap),
        when,
        if opts.dry_run { " [dry-run]" } else { "" }
    );
    restore::restore(&snap, &opts)?;
    Ok(())
}

fn cmd_save() -> R<()> {
    daemon::snapshot_once()?;
    eprintln!("Saved snapshot to {}", state::state_path()?.display());
    Ok(())
}

fn cmd_status() -> R<()> {
    match state::load() {
        Ok(snap) => {
            println!("state file : {}", state::state_path()?.display());
            println!("captured   : {} ago", format_age(snap.captured_at_unix).trim());
            println!("windows    : {}", snap.windows.len());
            println!("workspaces : {}", distinct_workspaces(&snap));
            println!();
            let mut ws: Vec<_> = snap.workspaces.iter().collect();
            ws.sort_by_key(|w| w.id);
            for w in ws {
                let wins: Vec<&window_reload::model::SavedWindow> =
                    snap.windows.iter().filter(|x| x.workspace_id == w.id).collect();
                if wins.is_empty() {
                    continue;
                }
                println!("  workspace {} ({}):", w.id, w.name);
                for win in wins {
                    let extra = win
                        .term_cwd
                        .as_deref()
                        .map(|c| format!("  {c}"))
                        .unwrap_or_default();
                    let float = if win.floating { " (float)" } else { "" };
                    // A window last seen before this snapshot's time is remembered
                    // (recently closed) rather than currently open.
                    let remembered = if win.last_seen_unix < snap.captured_at_unix {
                        " (closed, remembered)"
                    } else {
                        ""
                    };
                    println!("    - {}{}{}{}", win.class, float, extra, remembered);
                }
            }
        }
        Err(e) => {
            println!("No snapshot yet ({e}).");
        }
    }
    Ok(())
}

fn cmd_path() -> R<()> {
    println!("{}", state::state_path()?.display());
    Ok(())
}

fn cmd_install(args: &[String]) -> R<()> {
    let enable = args.iter().any(|a| a == "--enable");

    let home = std::env::var("HOME").map_err(|_| "HOME not set")?;
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("window-reloadd")))
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "window-reloadd".to_string());

    let unit_dir = std::path::Path::new(&home).join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    let unit_path = unit_dir.join("window-reloadd.service");
    // Restart=always because a clean socket-close (Hyprland restart) exits 0; the
    // daemon re-resolves the instance on restart, so restarting it is always right.
    let unit = format!(
        "[Unit]\n\
         Description=Record Hyprland window layout for window-reload\n\
         PartOf=graphical-session.target\n\
         After=graphical-session.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={bin}\n\
         Restart=always\n\
         RestartSec=2\n\
         \n\
         [Install]\n\
         WantedBy=graphical-session.target\n"
    );
    std::fs::write(&unit_path, unit)?;
    eprintln!("Wrote {}", unit_path.display());

    if enable {
        let reloaded = std::process::Command::new("systemctl")
            .args(["--user", "daemon-reload"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !reloaded {
            return Err("systemctl --user daemon-reload failed".into());
        }
        let enabled = std::process::Command::new("systemctl")
            .args(["--user", "enable", "--now", "window-reloadd.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !enabled {
            return Err("systemctl --user enable --now window-reloadd.service failed".into());
        }
        eprintln!("Enabled and started window-reloadd.service");
    } else {
        eprintln!("\nTo enable the recorder:");
        eprintln!("  systemctl --user daemon-reload");
        eprintln!("  systemctl --user enable --now window-reloadd.service");
    }

    let restore_bin = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "window-reload".to_string());
    eprintln!("\nAdd to your hyprland.conf:");
    eprintln!("  # start the recorder even if you don't launch Hyprland via uwsm:");
    eprintln!("  exec-once = systemctl --user start window-reloadd.service");
    eprintln!("  # restore the saved layout on login:");
    eprintln!("  exec-once = {restore_bin} restore");
    eprintln!(
        "\nThe recorder resolves the running Hyprland instance itself, so it works\n\
         whether or not it inherits the compositor's environment."
    );
    Ok(())
}

fn distinct_workspaces(snap: &window_reload::model::Snapshot) -> usize {
    let mut ids: Vec<i64> = snap.windows.iter().map(|w| w.workspace_id).collect();
    ids.sort();
    ids.dedup();
    ids.len()
}

/// Human-friendly age of a unix timestamp.
fn format_age(captured: u64) -> String {
    let now = window_reload::now_unix();
    if captured == 0 || now < captured {
        return "just now".into();
    }
    let secs = now - captured;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn print_help() {
    println!(
        "window-reload — restore Hyprland windows to their saved workspaces\n\
         \n\
         USAGE:\n\
         \x20 window-reload <command>\n\
         \n\
         COMMANDS:\n\
         \x20 restore [-n|--dry-run] [-f|--force] [--only WS]\n\
         \x20                 Recreate the saved layout. --dry-run prints the plan;\n\
         \x20                 --force spawns even windows that look already-open;\n\
         \x20                 --only restores a single workspace id.\n\
         \x20 save            Take a snapshot now (the daemon does this automatically).\n\
         \x20 status          Show the current snapshot summary.\n\
         \x20 path            Print the snapshot file path.\n\
         \x20 install [--enable]\n\
         \x20                 Write the systemd user unit for the recorder.\n\
         \x20 help            Show this help.\n"
    );
}
