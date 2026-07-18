# zmux

A small GPUI terminal shell around Zed's `terminal` and `terminal_view` crates.

<img width="1918" height="1163" alt="image" src="https://github.com/user-attachments/assets/539356a7-2e13-40a5-9fd5-d2988f13cf0d" />

Run with:

```sh
cargo run --release
```

Shortcuts:

- Copy: `Ctrl-Shift-C`, `Ctrl-Insert`, or `Cmd-C`.
- Paste: `Ctrl-Shift-V`, `Shift-Insert`, or `Cmd-V`.
- New terminal tab: `Cmd-T` on macOS or `Ctrl-Shift-T`/`Ctrl-Shift-N` elsewhere.
- New workspace: `Cmd-N` on macOS or `Ctrl-Shift-E` elsewhere. Toggle the
  workspaces sidebar with `Cmd-B` on macOS or `Ctrl-Shift-B` elsewhere.
- Worktree picker: `Cmd-Ctrl-W` on macOS, `Alt-Ctrl-Shift-W` on Linux, or
  `Shift-Alt-W` on Windows.
- Notify the current terminal pane (manual test trigger): `Cmd-Shift-M` on macOS
  or `Ctrl-Shift-M` elsewhere.
- Jump to the latest unread notification: `Cmd-Shift-U` on macOS or
  `Ctrl-Shift-U` elsewhere.
- Toggle notification history: `Ctrl-Shift-I` or `Cmd-I`.
- CLI: `zmux notify "Title" "Body"` or `zmux notify --title "Title" --body "Body"`.
- Zoom terminal font: `Ctrl-=`/`Ctrl-+`, `Ctrl--`, `Ctrl-0`; macOS-style `Cmd` variants are also bound.
- Plain `Ctrl-C` is intentionally left for the shell interrupt signal.
- macOS also uses `Cmd-W` to close a tab, `Cmd-Option-W` to close all tabs,
  `Cmd-{`/`Cmd-}` to switch workspaces, and `Cmd-D`/`Cmd-Shift-D` to split a
  terminal right/down.

## Workspaces

The left sidebar lists independent **workspaces**. Each workspace keeps its own
open terminal tabs *and* their split layout. Switching is instant: the active
workspace lives in the center pane group, while inactive ones are detached and
parked with their terminals still running (no PTY restart), so a build or watch
process keeps going in the background while you work elsewhere.

- Click **+** (or press `Cmd-N` on macOS or `Ctrl-Shift-E` elsewhere) to create a
  workspace.
- Click a workspace to switch to it.
- Double-click a workspace (or use the pencil button) to rename it inline.
- Drag a workspace up or down to reorder the list.
- Workspaces with unread agent notifications show a dot; the latest notification appears at the bottom of the sidebar.
- Use the trash button to close a workspace; the last one can't be closed.

### Git branches and worktrees

When the active workspace contains a Git repository, the top of the workspace
sidebar shows Zed's worktree and branch selectors.

- The branch selector searches local and remote branches, checks out an
  existing branch, and creates a branch from a typed name using Zed's native
  branch picker and Git error handling.
- The worktree selector lists the main checkout and every linked worktree. Pick
  an open worktree to jump to its existing zmux workspace, or pick a closed one
  to open a fresh terminal workspace rooted there.
- Choose a generated worktree based on the default/current branch, or type a
  name to create one. Creation honors Zed's `git.worktree_directory` setting,
  fetches remote branch bases with the normal credential prompt, and rolls back
  partial multi-repository creation failures.
- A worktree with live terminals cannot be deleted. Close its zmux workspace
  first, then use the picker trash action; hold Alt for Zed's force-delete flow.
- Secondary-open from the picker opens the selected worktree in another zmux
  window. Worktree names, paths, terminal layouts, and the selected repository
  survive session restore.

### Trusting terminal-discovered repositories

Terminal working directories and the Git repositories containing them are
treated as untrusted input. zmux periodically invokes the `git` executable from
its trusted host environment to populate workspace-rail metadata, but those
automatic commands do not inherit `GIT_*` overrides. They also disable optional
index writes and repository-local filesystem monitors; diff statistics disable
external diff drivers and text conversion helpers.

This hardening applies to zmux's automatic branch, status, and line-count
collection. It is not a general Git sandbox: standard configuration and
repository data are still parsed, and Git commands that you explicitly run in
a terminal retain their normal environment and behavior. The executable search
path and other non-Git host environment are part of the trusted application
launch context.

## Notifications

zmux accepts the terminal notification protocols used by contemporary terminal
tools: OSC 9, OSC 777 `notify`, and Kitty OSC 99 (including chunked/Base64
payloads, named replacement, anonymous notifications, activation reports,
close requests, alive queries, and capability queries). For example:

```sh
printf '\033]9;Build complete\007'
printf '\033]777;notify;Build;All checks passed\007'
printf '\033]99;i=build;Build complete\033\\'
zmux notify --title "Build complete" --body "All checks passed"
```

Each current notification is retained in the originating window's sidebar and
routed back to its exact workspace and terminal pane when opened. Ordinary
native banners are suppressed only while that exact pane is focused; Kitty's
explicit/default `o=always` delivery contract is honored even there. Focusing
or typing in the originating pane marks its current notifications read;
unrelated panes and other zmux windows are unaffected.

`zmux notify` uses a random capability inherited by one terminal only. It must
run inside the originating zmux shell; requests never select a pane from a PID
or from whichever window happens to be focused. A capability is revoked when
its terminal exits, and the command reports success only after the in-app row
has actually been recorded.

Native delivery uses the same application identity on Linux, macOS, and
Windows. Release artifacts include the Linux desktop entry, macOS app bundle,
and Windows installer metadata required by those operating systems; source
builds still retain the full in-app history if the host has no usable
notification service or packaged identity.

Tagged releases are the distributable artifacts: Linux is a `.deb` that
installs `zmux` on `PATH` together with its XDG desktop identity (plus a
distro-neutral `.tar.gz` carrying the binary, license, and desktop file for
non-Debian distributions), macOS is a Developer ID-signed and Apple-notarized
app bundle, and Windows is an Authenticode-signed MSI whose shortcut shares
zmux's AppUserModelID. Install the Linux package with
`sudo apt install ./zmux-linux-x86_64.deb`, or unpack the tarball anywhere and
run its `zmux` binary directly; use the app bundle or MSI normally on macOS and
Windows. Non-tag CI artifacts whose names end in `-unsigned` are macOS/Windows
packaging smoke-test outputs, not end-user releases. Tagged builds fail closed
unless all Apple notarization or Windows Authenticode credentials are
available, and the release job accepts only the four exact platform artifact
names. The Linux packages require a Vulkan-capable driver; the `.deb` declares
the Vulkan loader dependency explicitly.

Build notes:

- `zmux` wraps Zed's GPUI terminal view, which pulls in substantial editor/workspace/UI code. The required Zed crates are fetched from `https://github.com/zed-industries/zed` at the pinned revision recorded in `Cargo.toml` and `Cargo.lock`
- On the first run after moving away from Zed's data directory, legacy databases are copied into a private staging directory and atomically installed without changing the Zed copy. Each live WAL-mode `db.sqlite` is captured with SQLite's online backup API; raw WAL, shared-memory, and rollback-journal sidecars are not copied.
- Release builds strip symbols and use `panic = "abort"` to reduce artifact size without enabling slower size optimizations such as LTO by default.
- Linux and FreeBSD builds enable `gpui_platform`'s `font-kit`, `wayland`, and `x11` backends. macOS and Windows avoid those Linux display features in this crate's target-specific dependency configuration.
- Cross-platform builds still require the appropriate platform toolchain and native QA for GUI, PTY, clipboard, and font behavior.
- Release CI pins Ubuntu 24.04, macOS 15 (with a macOS 12 deployment target), and Windows 2025; the Linux `.deb` therefore targets the Ubuntu 24.04/glibc compatibility baseline.
- Release builds are dispatched manually from the Actions tab. Plain dispatches package immediately and skip formatting, Clippy, and the test suites unless the `full_validation` input is enabled; dispatches on a `v*` tag always run every check and require the signing credentials. Windows runs the suite under `cargo nextest` so each test gets its own process (ConPTY state is process-wide), with retries configured in `.config/nextest.toml` for spawn-timeout flakes.
- After `Cargo.lock` is committed, use `cargo build --locked` and `cargo test --locked` to reproduce the pinned dependency set.

## Roadmap

- [ ] [Open native agent session forks in a new zmux terminal](docs/agent-session-forking.md), starting with stock Codex and Claude Code CLIs.
