# zmux

A small GPUI terminal shell around Zed's `terminal` and `terminal_view` crates.

![zmux welcome screen](assets/zmux_welcome_screen.png)

Run with:

```sh
cargo run --release
```

Shortcuts:

- Copy: `Ctrl-Shift-C`, `Ctrl-Insert`, or `Cmd-C`.
- Paste: `Ctrl-Shift-V`, `Shift-Insert`, or `Cmd-V`.
- New terminal tab: `Ctrl-Shift-T` or `Ctrl-Shift-N`.
- New workspace: `Ctrl-Shift-E`. Toggle the workspaces sidebar: `Ctrl-Shift-B`.
- Notify the current terminal pane (manual test trigger): `Ctrl-Shift-M`.
- Jump to the latest unread notification: `Ctrl-Shift-U`.
- Toggle notification history: `Ctrl-Shift-I` or `Cmd-I`.
- CLI: `zmux notify "Title" "Body"` or `zmux notify --title "Title" --body "Body"`.
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
installs `zmux` on `PATH` together with its XDG desktop identity, macOS is a
Developer ID-signed and Apple-notarized app bundle, and Windows is an
Authenticode-signed MSI whose shortcut shares zmux's AppUserModelID. Install
the Linux package with `sudo apt install ./zmux-linux-x86_64.deb`; use the app
bundle or MSI normally on macOS and Windows. Non-tag CI artifacts whose names
end in `-unsigned` are macOS/Windows packaging smoke-test outputs, not end-user
releases. Tagged builds fail closed unless all Apple notarization or Windows
Authenticode credentials are available, and the release job accepts only the
three exact signed platform artifact names. The Linux package requires a
Vulkan-capable driver and declares the Vulkan loader dependency explicitly.

Build notes:

- `zmux` wraps Zed's GPUI terminal view, which pulls in substantial editor/workspace/UI code. The required Zed crates are fetched from `https://github.com/zed-industries/zed` at the pinned revision recorded in `Cargo.toml` and `Cargo.lock`
- Release builds strip symbols and use `panic = "abort"` to reduce artifact size without enabling slower size optimizations such as LTO by default.
- Linux and FreeBSD builds enable `gpui_platform`'s `font-kit`, `wayland`, and `x11` backends. macOS and Windows avoid those Linux display features in this crate's target-specific dependency configuration.
- Cross-platform builds still require the appropriate platform toolchain and native QA for GUI, PTY, clipboard, and font behavior.
- Release CI pins Ubuntu 24.04, macOS 15 (with a macOS 12 deployment target), and Windows 2025; the Linux `.deb` therefore targets the Ubuntu 24.04/glibc compatibility baseline.
- After `Cargo.lock` is committed, use `cargo build --locked` and `cargo test --locked` to reproduce the pinned dependency set.
