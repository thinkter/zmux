# Architecture

Zmux is a GPUI terminal application assembled from pinned Zed crates. It keeps
Zed's `Workspace`, pane, terminal, settings, theme, Git, and editor machinery,
then adds logical workspaces, terminal-scoped notifications, and agent status.
`src/lib.rs` is the public facade used by both executables.

## Entry points and application setup

- `main.rs` is the console entry point. It handles `zmux notify`, configures the
  data directory, and launches the GUI for all other invocations.
- `bin/zmux-gui.rs` is the Windows GUI-subsystem entry point. Both binaries
  converge in `launcher.rs`, so path setup is identical.
- `launcher.rs` selects zmux-owned data, settings, and database paths before any
  Zed path is resolved.
- `app/mod.rs` initializes GPUI and Zed globals, fonts, themes, syntax, settings,
  keymaps, Git integration, the notification runtime, and the CLI server.
- `app/open.rs` opens a Zed window and installs the `WorkspacesPanel`, Git panel,
  welcome item, actions, and startup layout.
- `app/terminal.rs` is the only terminal-construction path. It creates a fresh
  CLI-route capability per terminal and rejects asynchronous completions whose
  logical workspace generation or destination pane is stale.
- `app/actions.rs` registers zmux actions and capture-phase interceptors so Zed's
  generic new-terminal and split actions still use that construction path.

## Logical workspaces

`workspaces/mod.rs` owns the logical workspace list, activation state machine,
and two refresh loops. Inactive workspaces are parked as live entities rather
than serialized processes, so their PTYs keep running.

- Every 300 ms, `workspaces/agent_chat.rs` samples terminal processes and a
  bounded terminal tail. It reconciles the agent chat rail with confirmation
  hysteresis supplied by `agent_detection.rs`.
- Every 2 seconds, `workspaces/git_context.rs` samples shell working directories,
  discovers Git roots, reconciles Zed project worktrees, refreshes bounded Git
  metadata from `metadata.rs`, and updates automatic workspace names.
- `workspaces/persistence.rs` captures and restores pane trees, tabs, ratios, and
  fresh-shell directories. It also coalesces logical-session writes.
- `workspaces/panel.rs` renders the workspace rail, agent rows, Git metadata and
  pickers, rename controls, and notification drawer.
- `prefs.rs` stores zmux-owned UI preferences such as agent-rail scope.
- `workspace_switcher.rs` provides the modifier-aware workspace switcher.

The durable schema and atomic file I/O live in `session.rs`. Sessions contain
layout and fresh-shell working directories only; command lines, output,
environment variables, and process state are never persisted.

## Agent chat rail

`agent_detection.rs` maps known process names to `AgentKind` and classifies the
bounded live UI as working, needing input, idle, quiet, or a transient view.
`workspaces/agent_chat.rs` owns row identity, ordering, presentation metadata,
and the confirmation state machine. The rail is global by default, with a
Settings toggle to scope it to the active workspace. Preferences live in
`prefs.rs` (`state/prefs-v1.json`). See [Agent chat rail](agent-chat-rail.md).

## Notifications

There are two ingress paths with one canonical store:

1. Shell OSC 9, 99, and 777 sequences cross the vendored VTE/Alacritty bridge
   as replayable breadcrumb events. `osc.rs` validates, bounds, orders, and
   decodes them.
2. `zmux notify` sends a bounded request through `cli_server.rs`. Each terminal
   knows only its own capability; the server maps it to a server-owned route.

`notification_runtime.rs` binds terminal entities to exact window, logical
workspace, and pane targets. It publishes both ingress paths into the observable
`NotificationStore` in `notifications.rs`, drives unread state and navigation,
and delegates native delivery to `desktop_notifications.rs`. The desktop layer
owns platform tokens and callbacks but never becomes the canonical history.

## Supporting modules

- `assets.rs` embeds fonts, icons, images, and themes.
- `fonts.rs` registers the embedded font families.
- `theme.rs` defines first-run appearance and terminal font defaults.
- `settings_page.rs` renders settings and writes appearance/terminal options
  through Zed's settings store; `app/mod.rs` watches the same file for hand
  edits. Agent-rail scope is stored separately in `prefs.rs`.
- `syntax.rs` registers bundled parsers and connects the active theme.
- `keymap.rs` defines terminal, pane, workspace, settings, zoom, and quit keys.
- `env.rs` builds scrubbed shell environments and injects per-terminal CLI
  notification endpoints.
- `welcome.rs` renders the empty-workspace welcome item.

## Vendored Zed crates

The root manifest pins a Zed revision and patches selected crates locally:

- `vendor/terminal` carries two runtime patches: process-table refreshes are
  coalesced to at most about five per second per PTY, and default scrollback is
  5,000 lines instead of 10,000 (the user setting can still raise it).
- `vendor/workspace` keeps pane bounding-box caches safe across structural
  mutations, uses checked cache lookups, and exposes the center pane group for
  layout persistence. It also contains zmux's intentional terminal-tab chrome
  changes (no navigation arrows and a direct new-terminal button).
- `vendor/terminal_view` contains zmux terminal title, indicator, and tab UI
  integration.
- `vendor/alacritty_terminal` and `vendor/vte` carry the bounded notification
  OSC bridge consumed by `osc.rs`.

When updating the pinned Zed revision, compare every vendored crate with its
new upstream source and reapply these changes deliberately.
