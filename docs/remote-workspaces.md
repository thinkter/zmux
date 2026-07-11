# Experimental SSH workspaces

SSH workspaces are an opt-in foundation. They are intentionally separate from
ordinary terminal workspaces: creating one requires a typed
`SshWorkspaceConfig`, and its terminal is launched as `ssh` plus separate argv
arguments. zmux never accepts an arbitrary SSH command string or runs `sh -c`.

The current API entry point is `WorkspacesPanel::create_ssh_workspace`. A
future host picker can construct the same configuration after explicit user
confirmation. Calling it creates a remote sidebar entry in `remote:
connecting` state; a PTY/SSH lifecycle owner must call
`mark_remote_connected` only after it observes a usable connection, or
`mark_remote_connection_lost` when the connection exits. The latter uses a
bounded exponential retry controller and reports the delay/exhausted status
for the sidebar.

## SSH policy

`SshWorkspaceConfig` uses the user's normal OpenSSH configuration by default:
no `-F` argument is emitted, so `~/.ssh/config`, `Include`, `ProxyJump`, host
aliases, known-host configuration, and `SSH_AUTH_SOCK` work as they do in a
regular terminal. An explicit SSH config path is possible, but is still passed
as an individual `-F` argv value.

The safe defaults are deliberate:

- `StrictHostKeyChecking=yes` requires a known host key. `accept_new` and
  `use_user_config` are explicit alternatives; there is no insecure
  host-key-off option.
- Local SSH-agent authentication is inherited. Agent *forwarding* is forced
  off unless `AgentForwarding::ExplicitlyEnabled` is selected.
- `ClearAllForwardings=yes` stops implicit forwards from SSH config. Port and
  relay forwarding must be requested in the workspace configuration.
- Host aliases, usernames, remote TCP hosts, and tmux session names are
  validated compact values. The launcher exposes no raw `extra_args` or raw
  remote-command field.

An ordinary plan therefore has the shape below, with each item a distinct
process argument:

```text
ssh -o ClearAllForwardings=yes -o ForwardAgent=no \
  -o StrictHostKeyChecking=yes work-host
```

Credentials, private-key paths, agent sockets, relay tokens, and arbitrary
commands are not stored in the remote workspace state file.

## Durable identity and reconnects

`RemoteWorkspaceStore` persists a small versioned JSON file under zmux's own
`paths::state_dir()` as `remote-workspaces-v1.json`; it never reads or writes
Zed recent-project/remote state. The store is atomic, size-bounded, and owner
only on Unix. Identity is deterministic from SSH host, optional username,
optional port, and optional remote root, so changing a sidebar label does not
create a second remote identity.

Reconnect bookkeeping is runtime-only. Its default budget is four attempts,
starting at 250 ms and capped at 5 seconds; policy validation caps every
configuration at eight attempts. No failed host retries forever and no stale
timer survives an application restart.

## Authenticated remote notifications

`RemoteRelayGrant` creates an in-memory 256-bit capability token bound to
exactly one local `workspace_id` and `surface_id`. The listener owner must
discard or rotate the grant when that remote session ends. It signs a
length-bounded notification envelope with HMAC-SHA-256 and a 128-bit nonce.
`RemoteRelayVerifier` checks:

1. protocol version and notification size;
2. exact target binding, before any UI routing;
3. HMAC authentication; and
4. a bounded replay window.

There are no remote relay commands for sending terminal input, selecting other
workspaces, creating browser surfaces, or targeting an arbitrary local pane.
`RemoteRelayListener` binds only to `127.0.0.1`; the explicit
`RemoteRelayMode::ReverseTunnel(port)` adds a loopback-only SSH `-R` forward.
The relay token is never persisted or automatically injected into a remote
shell. A remote CLI integration must deliberately deliver it through a channel
the user has chosen, discard it when the session ends, and generate a fresh
128-bit nonce for every event.

## Port and browser routing

`RemotePortRouting::Disabled` is the default. `Loopback` routes emit only
`-L 127.0.0.1:LOCAL:REMOTE_HOST:REMOTE_PORT`, reject duplicate local ports, and
are capability-gated. The foundation advertises safe TCP routing but does not
advertise browser routing, so setting `browser_surface: true` is rejected
rather than opening a browser or exposing a port unexpectedly.

## Experimental tmux bridge

`TmuxBridgeConfig::Experimental(session)` is the only path that launches tmux
control mode. It creates this fixed, validated token sequence:

```text
ssh [safe ssh options] -tt HOST tmux -CC new-session -A -s SESSION
```

`SESSION` accepts only ASCII letters, digits, `.`, `_`, and `-`. The bridge
parses tmux `%window-*`, `%pane-*`, `%layout-change`, and active-window events
into `NativeTmuxLayoutModel`; tmux `{}` layouts project to horizontal native
splits and `[]` layouts to vertical splits. Output and unknown future control
events are ignored, not interpreted as commands.

This is intentionally experimental: the parser/model is ready for a dedicated
PTY bridge, but it does not change ordinary SSH terminals, and it does not yet
claim full remote input/lifecycle synchronization. No automatic SSH process
lifecycle observer is wired by this foundation, so an integration must report
actual connection/disconnection observations to the sidebar API. Disable tmux
mode to obtain the normal SSH terminal behavior.
