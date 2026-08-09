# zmux

A small GPUI terminal workspace built on Zed's `terminal` and `terminal_view` crates.

<img width="1918" height="1163" alt="zmux" src="https://github.com/user-attachments/assets/539356a7-2e13-40a5-9fd5-d2988f13cf0d" />

## Features

* Multiple terminal workspaces
* Tabs and split panes
* Git branch and worktree management
* Persistent terminals when switching workspaces
* Native terminal notifications
* Session restore
* Linux, macOS, and Windows support

## Run from source

```sh
cargo run --release
```

The required Zed crates are fetched from the pinned revision in `Cargo.toml` and `Cargo.lock`.

For reproducible builds:

```sh
cargo build --locked
```

## Shortcuts

| Action               | macOS         | Linux / Windows                    |
| -------------------- | ------------- | ---------------------------------- |
| New terminal         | `Cmd-T`       | `Ctrl-Shift-T`                     |
| New workspace        | `Cmd-N`       | `Ctrl-Shift-E`                     |
| Toggle sidebar       | `Cmd-B`       | `Ctrl-Shift-B`                     |
| Worktree picker      | `Cmd-Ctrl-W`  | `Alt-Ctrl-Shift-W` / `Shift-Alt-W` |
| Latest notification  | `Cmd-Shift-U` | `Ctrl-Shift-U`                     |
| Notification history | `Cmd-I`       | `Ctrl-Shift-I`                     |

Copy and paste use the usual terminal shortcuts:

* Copy: `Ctrl-Shift-C`, `Ctrl-Insert`, or `Cmd-C`
* Paste: `Ctrl-Shift-V`, `Shift-Insert`, or `Cmd-V`

`Ctrl-C` remains the shell interrupt signal.

On macOS:

* `Cmd-W` closes a tab
* `Cmd-D` splits right
* `Cmd-Shift-D` splits down
* `Cmd-{` / `Cmd-}` switches workspaces

## Workspaces

Each workspace keeps its own terminal tabs and split layout.

Switching workspaces does not restart terminals, so builds, servers, and other processes continue running in inactive workspaces.

Workspaces can be created, renamed, reordered, and closed from the sidebar.

## Git worktrees

When a workspace is inside a Git repository, zmux exposes Zed's branch and worktree pickers.

You can:

* switch or create branches
* open existing worktrees
* create new worktrees
* switch directly to an already-open worktree
* open a worktree in another zmux window

Terminal layouts and worktree state are restored between sessions.

## Notifications

zmux supports terminal notifications through OSC 9, OSC 777, and Kitty OSC 99.

```sh
printf '\033]9;Build complete\007'
```

You can also send notifications using:

```sh
zmux notify "Build complete" "All checks passed"
```

Notifications are attached to the terminal pane that created them. Opening one returns you to the correct window, workspace, and pane.

## Releases

Tagged releases provide:

* Linux `.deb` and `.tar.gz`
* notarized macOS app bundle
* signed Windows MSI

Linux:

```sh
sudo apt install ./zmux-linux-x86_64.deb
```

Non-tagged `-unsigned` CI artifacts are development builds and are not intended for normal installation.

## Roadmap

* [ ] [Open native agent session forks in a new zmux terminal](docs/agent-session-forking.md), starting with Codex and Claude Code.
