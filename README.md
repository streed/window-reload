# window-reload

Automatically bring your Hyprland windows, terminals, and workspace layout back
to where they were after a restart.

Wayland has no built-in session restore, so "restoring" a window really means
**re-launching the application and re-placing it**. `window-reload` continuously
records where every window lives — which workspace, the tiling arrangement, and
(for terminals) the working directory of the shell inside — and can recreate that
whole arrangement on demand or automatically at login.

## What it captures

For every mapped window on every normal workspace:

- **Workspace** and monitor.
- **Tiling arrangement** — the dwindle split tree is reconstructed from window
  geometry, so windows come back in the same order, orientation (side-by-side vs
  stacked), and relative sizes.
- **Floating** windows: exact position and size.
- **Fullscreen / maximized** state.
- **Terminal working directory** — resolved by walking the process tree below the
  terminal to the shell (or foreground program) running inside it, not the
  terminal's own cwd.
- **Launch command** — the process argv from `/proc`, plus refinements for known
  apps (terminals get `--working-directory`, Chromium PWAs get `--app-id`).
- **Tabbed groups** (best effort).

Snapshots are written atomically to
`~/.local/state/window-reload/state.json` (a `.bak` copy is kept).

### Remembering closed windows

The recorder mirrors the live desktop, but a window that closes is **remembered**
(and stays restorable) for a grace period — 10 minutes by default — before it ages
out. This is what makes restore work in practice:

- A restart/logout closes every window at once; because they were "just closed",
  the whole desktop is remembered and comes back on the next login.
- You can test restore by closing a window and running `window-reload restore`.
- A window you deliberately closed and left closed is forgotten after the grace
  period, so it does not resurrect on a later restore.

Tune it with `window-reloadd --keep-closed <seconds>` (`0` mirrors live state
exactly). Remembered windows are shown as `(closed, remembered)` in
`window-reload status`.

## Components

| Binary | Role |
| --- | --- |
| `window-reloadd` | Recorder daemon. Watches Hyprland's event socket and snapshots the layout (debounced) whenever something structural changes, plus a periodic safety snapshot. |
| `window-reload`  | CLI: `restore`, `save`, `status`, `path`, `install`. |

## Build & install

```sh
cargo build --release
install -Dm755 target/release/window-reload  ~/.cargo/bin/window-reload
install -Dm755 target/release/window-reloadd ~/.cargo/bin/window-reloadd

# Write and enable the recorder as a systemd --user service:
window-reload install --enable
```

Then add auto-restore to `~/.config/hypr/hyprland.conf`:

```
exec-once = window-reload restore
```

That's the setup the recorder + login-restore combination the installer recommends.

## Usage

```sh
window-reload status              # what's in the current snapshot
window-reload restore             # recreate the saved layout
window-reload restore --dry-run   # print the plan without spawning anything
window-reload restore --only 8    # restore just workspace 8
window-reload restore --force     # spawn even windows that already look open
window-reload save                # take a snapshot right now (daemon does this for you)
window-reload path                # print the state-file path
```

`restore` is **idempotent**: by default it skips any window that already appears
to be open (matched by class + terminal cwd / app id), so running it on a fresh
login recreates everything, and running it again does nothing.

## How restore works

1. Load the snapshot and group windows by workspace.
2. For each workspace, reconstruct the dwindle tiling tree from saved geometry.
3. Spawn each window **directly onto its workspace** using Hyprland's
   `[workspace N silent]` exec rules, replaying the tree so splits form in the
   right order. After each split, the orientation is corrected with
   `layoutmsg togglesplit` if the aspect-ratio default guessed wrong.
4. Converge tile sizes with `resizewindowpixel exact` (the saved pixel sizes are
   mutually consistent, so the dividers settle without fighting).
5. Place floating windows with exact size + position; apply fullscreen; reunite
   groups.
6. Reset temporary settings and return focus to where you were.

## Design choices

- **Terminals reopen at their directory with a fresh shell.** Programs that were
  running inside (a build, an editor, `claude`, `ssh`) are **not** re-run — only
  the working directory is restored. (This is a deliberate safety choice.)
- **Rust, no runtime deps.** Two small static binaries; the recorder runs as a
  `systemd --user` service so it starts with your graphical session and restarts
  if it crashes.

## Known limitations

- **Browsers with multiple windows.** Chromium/Chrome runs one process for many
  windows; session-restore reopens its own windows on first launch, so multiple
  plain browser windows may not map perfectly to their original workspaces. PWA
  "app" windows (`--app-id`) are handled individually and place reliably.
- **Deeply nested, asymmetric tiling trees** are reproduced best-effort. One or
  two windows per split (the common case) is exact; unusual 4+ window trees may
  differ slightly in split ratios.
- **In-app layouts** (tmux panes, editor splits) are not captured — only the
  window/workspace arrangement.
- **Single monitor** is the tested target. Multi-monitor should mostly work
  (workspaces carry their monitor binding) but focus-return only tracks the
  primary monitor.

## Development

```sh
cargo test            # layout reconstruction unit tests
cargo build --release
```

Module map: `capture` (build a snapshot) · `layout` (BSP reconstruction) ·
`launch` (classify + command) · `restore` (spawn + place) · `daemon` (recorder) ·
`state` (atomic IO) · `hypr`/`proc` (Hyprland IPC and `/proc`).
