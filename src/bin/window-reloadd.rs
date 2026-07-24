//! The recorder daemon binary.

use std::time::Duration;
use window_reload::daemon::{self, Config};

fn main() {
    let mut cfg = Config::default();
    let mut once = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--once" => once = true,
            "--debounce" => {
                if let Some(ms) = args.next().and_then(|v| v.parse::<u64>().ok()) {
                    cfg.debounce = Duration::from_millis(ms);
                }
            }
            "--interval" => {
                if let Some(s) = args.next().and_then(|v| v.parse::<u64>().ok()) {
                    cfg.periodic = Duration::from_secs(s);
                }
            }
            "--keep-closed" => {
                if let Some(s) = args.next().and_then(|v| v.parse::<u64>().ok()) {
                    cfg.keep_closed = Duration::from_secs(s);
                }
            }
            "-h" | "--help" => {
                print_help();
                return;
            }
            other => {
                eprintln!("window-reloadd: unknown argument '{other}'");
                print_help();
                std::process::exit(2);
            }
        }
    }

    let result = if once {
        daemon::snapshot_once()
    } else {
        daemon::run(cfg)
    };

    if let Err(e) = result {
        eprintln!("window-reloadd: {e}");
        std::process::exit(1);
    }
}

fn print_help() {
    println!(
        "window-reloadd — record Hyprland window layout on change\n\
         \n\
         USAGE:\n\
         \x20 window-reloadd [--debounce MS] [--interval SECONDS]\n\
         \x20 window-reloadd --once      Take a single snapshot and exit\n\
         \n\
         OPTIONS:\n\
         \x20 --debounce MS       Quiet period after an event before snapshotting (default 800)\n\
         \x20 --interval SECS     Periodic safety snapshot interval (default 30)\n\
         \x20 --keep-closed SECS  Remember closed windows this long so they stay\n\
         \x20                     restorable (default 600; 0 = mirror live state)\n"
    );
}
