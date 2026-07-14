# Native agent session forks

Status: **TODO**

When a user runs a native session-fork command inside a supported coding-agent
CLI, zmux should keep the original conversation available and open the fork in
another terminal. This should work with stock harness installations: users
should not need to install a zmux-specific Codex plugin, Claude Code hook, or
shell extension.

## Intended experience

With the default placement set to `right`:

```text
Before /fork           After /fork
+--------------+       +--------------+--------------+
| Parent agent |  -->  | Parent agent | Forked agent |
+--------------+       +--------------+--------------+
```

The new terminal should inherit the current working directory and logical zmux
workspace. Both conversations remain in the same checkout; this feature does
not create a Git worktree.

## Proposed behavior

1. Track input sent to a recognized agent TUI and notice an exact native fork
   command, initially Codex `/fork` and the equivalent Claude Code session
   branch command supported by the installed version.
2. Let the stock CLI perform its native fork. zmux should observe the result,
   not replace or emulate the harness command.
3. Resolve the exact parent and child session IDs from bounded, harness-owned
   metadata. Examples include Codex parent/fork identifiers and Claude session
   branch ancestry fields.
4. Materialize the result in the layout:
   - keep the current live PTY as the forked conversation;
   - move or place that PTY in the configured destination; and
   - resume the original session in the original pane using an argv such as
     `codex resume <parent-id>` or `claude --resume <parent-id>`.
5. Give the new terminal fresh notification capability state and otherwise
   preserve normal zmux workspace and session behavior.

Never fall back to `--last`, a most-recent-session heuristic, or a guessed ID.
If zmux cannot prove the parent/child relationship, it should leave the native
fork running in the current terminal and create no extra pane.

## Harness adapter boundary

Each supported harness should be implemented behind a small adapter that owns:

- process and executable recognition;
- supported fork or branch commands;
- session metadata locations and parsers;
- parent/child relationship validation;
- safe resume argv construction; and
- an explicitly tested range of harness versions or metadata schemas.

This keeps the layout transaction harness-agnostic and allows Codex, Claude
Code, and future agents to evolve independently.

## Settings

- Enable or disable automatic materialization of native agent forks.
- Placement: `right` (default), `down`, `tab`, or `workspace`.
- Focus after the fork: parent or forked conversation.

If the detected harness or installed version is unsupported, zmux should leave
its native fork behavior untouched and explain why no additional terminal was
opened.

## Failure and race handling

- Scope detection to the exact terminal, window, logical workspace, and process
  generation that issued the command.
- Ignore subagent sessions and unrelated session files created concurrently.
- Use a short timeout and cancel cleanly if no unambiguous child appears.
- Cancel if the originating pane closes, changes process generation, or the
  layout becomes stale before the transaction commits.
- Validate session IDs as UUIDs or the adapter's documented identifier format.
- Bound metadata reads by directory, file type, file size, and time window.
- Pass resume commands as fixed argv; never evaluate metadata or captured
  terminal text as shell code.
- Commit the layout mutation only after the session relationship and target
  placement have both been validated.

## Implementation checklist

- [ ] Add the harness adapter interface and capability/version reporting.
- [ ] Implement exact command observation for recognized agent TUIs.
- [ ] Implement the Codex metadata adapter and exact parent resume argv.
- [ ] Implement the Claude Code metadata adapter and exact parent resume argv.
- [ ] Add the atomic layout transaction for right/down/tab/workspace placement.
- [ ] Add enable, placement, and focus settings to the settings UI.
- [ ] Reset notification capability state for the newly materialized terminal.
- [ ] Surface unsupported versions and ambiguous-session failures without
      interrupting the native fork.
- [ ] Document that this is session branching, not checkout isolation.

## Verification

- Command-detection tests covering exact commands, pasted input, editing,
  cancellation, alternate shells, and unsupported agent processes.
- Metadata fixture tests for supported Codex and Claude Code schemas, malformed
  files, oversized files, missing ancestry, concurrent forks, and subagents.
- Layout tests for every placement and focus setting, including stale-pane and
  closed-pane races.
- Resume-argv tests proving that exact validated IDs are used and no shell
  interpolation is possible.
- Notification-state tests for the newly created terminal.
- End-to-end manual QA on Linux, macOS, and Windows with stock Codex and Claude
  Code installations.

## Open questions

- Should the original pane always remain in place, or should the focused pane
  follow the fork when the destination is a tab or workspace?
- Should zmux expose a reusable manual action for materializing a fork when
  automatic detection times out?
- Which harness versions and metadata schema changes should disable an adapter
  until compatibility is revalidated?
