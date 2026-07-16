# Agent chat rail

The agent chat rail discovers stock terminal-based agent CLIs without plugins
or wrapper processes. It observes only terminal state already available to
zmux: the foreground executable, current directory, OSC title, custom tab title,
and a bounded recent-text tail.

## Recognition and signals

`AgentKind` recognizes Claude, Codex, opencode, Pi, Amp, Gemini, Aider, and
Goose from normalized foreground executable names. Claude and Codex have
detailed UI detection; other recognized agents are shown as open while their
process exists.

For detailed agents, `detect_agent` returns a `DetectionSignal`:

- `Working`: current title or live UI contains active-progress evidence.
- `NeedsInput`: a permission prompt, form, or other actionable question is live.
- `Idle`: the normal prompt is visible or a stable idle title is present.
- `Quiet`: the process exists but evidence is insufficient for a stronger claim.
- `Hold`: a transient picker or transcript viewer covers the live agent UI, so
  the previously published row state must be preserved.

The detector favors `Quiet` and `Hold` over guessing. Historical text alone
must not turn an old permission prompt or spinner into current activity.

## Reconciliation and hysteresis

Every 300 ms, `WorkspacesPanel` builds an `AgentWorkspaceObservation` for each
logical workspace. A row is keyed by `(workspace_id, terminal_item_id)`, which
keeps identity stable when an agent restarts in the same terminal.

Transitions from an active state to `Idle` or `Quiet` require
`AGENT_STATE_CONFIRMATIONS` consecutive matching samples. A different sample
clears the pending transition. Active `Working` and `NeedsInput` evidence is
published immediately so attention state is responsive.

Process disappearance uses the same confirmation count. One missing sample is
treated as a transient process-table miss; after the second consecutive missing
sample, the row is removed. Closing the terminal tab removes the row immediately.
If the process reappears before confirmation, the existing row is reused and
its missing-sample count is cleared.

## Presentation

Rows sort by attention state and then recent activity. Titles prefer an explicit
terminal title, a bounded submitted-prompt snippet, and sanitized OSC metadata
before falling back to the agent label. Focusing the terminal updates read state;
the rail never owns or persists the agent process itself.
