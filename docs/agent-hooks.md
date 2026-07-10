# Agent hooks and trusted resume

zmux accepts a small, vendor-neutral hook event for terminal agents. It uses
the existing v1 control API to create a notification, so every event must name
the terminal surface and workspace that produced it. It never targets the
currently focused pane.

This document describes a contract, not a background listener. The local IPC
implementation owns authentication and endpoint discovery; it should parse a
hook event with `parse_hook_rpc_frame`, pass it to `AgentHookRouter`, then send
the returned `notification_create` request through the normal control API.

## Generic event contract

One RPC frame is one UTF-8 JSON object, at most 8 KiB. It has no literal
terminal control bytes and rejects unknown fields.

```json
{
  "version": 1,
  "origin": { "workspace_id": 12, "surface_id": 99 },
  "kind": "permission_request",
  "agent": "my-build-agent",
  "title": "Needs approval",
  "body": "Allow publishing the preview?",
  "role": "primary",
  "public_session_id": "7f78d154-7513-4b80-b043-6a8f6b969d16"
}
```

`kind` is one of:

- `permission_request`
- `task_complete`
- `idle`
- `waiting`
- `error`

`role` defaults to `primary`; `subagent` and `teammate` are also supported.
`public_session_id` is optional and must be a short, opaque identifier using
only letters, numbers, `.`, `_`, `:`, or `-`. It is not a prompt, transcript,
path, environment value, or command line.

The equivalent OSC payload is:

```text
ESC ] 777;zmux;hook;{one-line JSON from above} BEL
```

Use `ESC \` instead of `BEL` if the terminal prefers ST termination. The OSC
parser receives the payload after `ESC ]`; it only claims the exact
`777;zmux;hook;` prefix. Existing OSC 9/99/777 notification handling remains
separate.

## Routing and filtering

`AgentHookRouter` maps the event into this existing control command:

```json
{
  "version": 1,
  "id": 73,
  "method": "notification_create",
  "params": {
    "workspace_id": 12,
    "surface_id": 99,
    "source": "agent_hook",
    "level": "warning",
    "title": "my-build-agent: Needs approval",
    "body": "Allow publishing the preview?"
  }
}
```

Permission requests map to `warning`, completed tasks to `success`, idle and
waiting states to `info`, and errors to `error`. The router never infers an
origin from focus state.

The `HookFilter` can hide `subagent` and/or `teammate` notifications. A hidden
event still receives an ID and remains in the bounded in-memory audit log with
its origin, role, kind, title, body, and `filtered_by_role` outcome. Filtering
therefore reduces noise without silently losing operational evidence.

## Opt-in adapters

`AgentAdapter` supports `codex`, `claude-code`, `opencode`, and `gemini`.
Each adapter accepts the same event frame **without** `agent`; zmux inserts the
known adapter label itself. This prevents an agent subprocess from claiming to
be a different supported adapter.

All adapters start disabled (`AdapterSettings::default()`). `AdapterPlan` only
describes the enable and removal steps for a UI: it writes no configuration.
zmux does not edit `~/.codex`, Claude Code, OpenCode, Gemini, shell, or terminal
configuration files. A user must explicitly review and add a vendor hook, and
can remove that user-added hook to reverse the integration.

For example, after a user has opted in to the Codex adapter, a wrapper can send:

```json
{
  "version": 1,
  "origin": { "workspace_id": 12, "surface_id": 99 },
  "kind": "waiting",
  "title": "Waiting for your response",
  "public_session_id": "7f78d154-7513-4b80-b043-6a8f6b969d16"
}
```

The adapter normalizes that to `agent: "codex"` before routing it. It returns
an `AdapterHookEvent`, whose provenance cannot be forged through the generic
event constructor; call `into_event()` only when handing it to the normal
router. The same shape works for Claude Code, OpenCode, and Gemini after their
separate opt-in.

## Safe native resume

`TrustedResumeRecord` serializes only:

```json
{
  "version": 1,
  "adapter": "codex",
  "public_session_id": "7f78d154-7513-4b80-b043-6a8f6b969d16"
}
```

It deliberately excludes the workspace, surface, current directory, prompt,
transcript, environment, and a prebuilt shell command. zmux only creates it
when both the adapter and resume are explicitly enabled and the event was
normalized by that adapter. The record validates the ID again when it is read.

The first end-to-end adapter is Codex: its native resume request is represented
as the argv vector `codex`, `resume`, `<public-session-id>`. zmux neither runs
that command automatically nor interpolates the ID through a shell. Claude
Code and OpenCode use pinned argv forms as well; Gemini remains notification
only until its native resume contract has a dedicated integration test.
