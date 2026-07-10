# zmux

A small GPUI terminal shell around Zed's `terminal` and `terminal_view` crates.

![zmux welcome screen](assets/zmux_welcome_screen.png)

Run with:

```sh
cargo run -j 6 --release
```

Shortcuts:

- Copy: `Ctrl-Shift-C`, `Ctrl-Insert`, or `Cmd-C`.
- Paste: `Ctrl-Shift-V`, `Shift-Insert`, or `Cmd-V`.
- New terminal tab: `Ctrl-Shift-T` or `Ctrl-Shift-N`.
- New workspace: `Ctrl-Shift-E`. Toggle the workspaces sidebar: `Ctrl-Shift-B`.
- Notify the current terminal pane (manual test trigger): `Ctrl-Shift-M`.
- Jump to the latest unread notification: `Ctrl-Shift-U`.
- CLI: `zmux notify "Title" "Body"` or `zmux notify --title "Title" --body "Body"`. Terminals spawned by zmux inherit a private per-instance notification endpoint, so the command always reaches the zmux instance that launched that terminal.
- Zoom terminal font: `Ctrl-=`/`Ctrl-+`, `Ctrl--`, `Ctrl-0`; macOS-style `Cmd` variants are also bound.
- Plain `Ctrl-C` is intentionally left for the shell interrupt signal.

## Workspaces

The left sidebar lists independent **workspaces**. Each workspace keeps its own
open terminal tabs *and* their split layout. Switching is instant: the active
workspace lives in the center pane group, while inactive ones are detached and
parked with their terminals still running (no PTY restart), so a build or watch
process keeps going in the background while you work elsewhere.

- Click **+** (or press `Ctrl-Shift-E`) to create a workspace.
- Click a workspace to switch to it.
- Double-click a workspace (or use the pencil button) to rename it inline.
- Drag a workspace up or down to reorder the list.
- Workspaces with unread agent notifications show a dot; the latest notification appears at the bottom of the sidebar.
- Use the trash button to close a workspace; the last one can't be closed.

## State isolation and reset

zmux intentionally does not read, write, or migrate Zed's user state. Settings,
sessions, databases, cache, and logs use the `Zmux`/`zmux` application namespace:

- Linux and FreeBSD: `$XDG_DATA_HOME/zmux`, `$XDG_CONFIG_HOME/zmux`,
  `$XDG_CACHE_HOME/zmux`, and `$XDG_STATE_HOME/zmux` (with the usual XDG
  defaults when those variables are unset).
- macOS: `~/Library/Application Support/Zmux`, `~/.config/zmux`,
  `~/Library/Caches/Zmux`, and `~/Library/Logs/Zmux`.
- Windows: `%LOCALAPPDATA%\Zmux` for data/state and `%APPDATA%\Zmux` for
  configuration.

To reset zmux, quit it and delete only the matching **zmux** directories above.
Do not delete or copy Zed's directories: zmux starts with a clean, independent
database and session store by design.

Build notes:

- `zmux` wraps Zed's GPUI terminal view, which pulls in substantial editor/workspace/UI code. The required Zed crates are fetched from `https://github.com/zed-industries/zed` at the pinned revision recorded in `Cargo.toml` and `Cargo.lock`
- Release builds strip symbols and use `panic = "abort"` to reduce artifact size without enabling slower size optimizations such as LTO by default.
- Linux and FreeBSD builds enable `gpui_platform`'s `font-kit`, `wayland`, and `x11` backends. macOS and Windows avoid those Linux display features in this crate's target-specific dependency configuration.
- Cross-platform builds still require the appropriate platform toolchain and native QA for GUI, PTY, clipboard, and font behavior.
- After `Cargo.lock` is committed, use `cargo build -j 6 --locked` and `cargo test -j 6 --locked` to reproduce the pinned dependency set.
