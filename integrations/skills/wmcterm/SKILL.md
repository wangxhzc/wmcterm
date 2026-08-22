---
name: wmcterm
description: Control the calling wmcterm workspace, panes, terminal surfaces, screen contents, notifications, and agent state. Use when an agent running inside wmcterm needs to inspect or operate its terminal UI; do not use for ordinary shell work outside wmcterm.
---

# wmcterm Control

Use `wmcterm` as the stable control interface. Start with `wmcterm capabilities`
when the protocol is uncertain.

## Preserve caller identity

Treat `WMCTERM_WORKSPACE_ID`, `WMCTERM_PANE_ID`, `WMCTERM_SURFACE_ID`, and
`WMCTERM_CONTROL_SOCKET` as the caller's authority. Never infer an Agent target
from the currently focused UI item: the user may be viewing another workspace.

- Inspect with `wmcterm status`, `wmcterm workspace list`, `wmcterm pane list`,
  `wmcterm surface list`, or
  `wmcterm screen read --surface "$WMCTERM_SURFACE_ID"`.
- Pass the corresponding environment ID explicitly to every mutating command
  that accepts a target. Stop and report unavailable control context when an
  ID is absent, invalid, or the socket cannot be reached.
- Change the caller workspace's directory only with an explicit target:
  `wmcterm workspace cwd "$WMCTERM_WORKSPACE_ID" /absolute/project/path`.
  Never substitute the currently focused workspace when
  `WMCTERM_WORKSPACE_ID` is missing.
- Use `--no-focus` when creating background work. Activate or focus another
  item only when the user explicitly requests it.
- Read the affected screen or list again when the outcome matters. Stop on a
  failed or ambiguous operation; do not retry against a different target.

## Launch Codex safely

- Launch managed Codex work with `wmcterm agent codex` or `wmcterm codex-teams`.
- Rely on the default `approval_policy=on-request` and
  `sandbox_mode=workspace-write`; requests outside the workspace sandbox are
  routed to wmcterm's centralized approval UI. Pass an explicit approval or
  sandbox option only when the user requests one.
- Never pass `--yolo` or `--dangerously-bypass-approvals-and-sandbox`; wmcterm
  rejects both shortcuts instead of weakening the workspace sandbox.
- Only when the user explicitly requests long-running unattended execution, use
  `wmcterm agent --mode unattended codex`. This keeps `workspace-write` and uses
  Codex's native automatic reviewer; it is not blanket approval. Full Access is
  human-only in the wmcterm UI and must never be selected or reconstructed by an
  Agent.
- The GUI may require a human project-trust decision before launching unattended
  Codex when the workspace contains `.codex`. This decision is intentionally not
  available through Agent, CLI, or RPC control; never edit Codex trust settings to
  imitate it.

## Launch Claude safely

- Launch a managed Claude Code process with `wmcterm agent claude`.
- Only when the user explicitly requests unattended execution, use
  `wmcterm agent --mode unattended claude`; this selects Claude's native `auto`
  permission mode. Never pass `bypassPermissions` or dangerous-skip flags.
- `wmcterm agent PROGRAM` runs in the terminal that invokes it. When it is the
  `--command` of a new pane, it must not create another tab or depend on UI
  focus.
- To give a delegated task its own visible terminal, create a new pane from the
  caller and launch a separate Claude process there:

```bash
wmcterm pane split --pane "$WMCTERM_PANE_ID" --right --no-focus \
  --command "wmcterm agent claude"
```

Claude Code matches Bash permission rules before shell variable expansion.
When issuing wmcterm commands through Claude's Bash tool, materialize the UUIDs
from the caller context or a previous wmcterm JSON response into the command;
do not submit literal `$WMCTERM_*` expressions or compound shell pipelines.
Always use IDs returned by `pane split` for the newly created target.
The installed Claude integration keeps exact, read-only recovery commands such
as `wmcterm status`, `wmcterm capabilities`, `wmcterm pane list`,
`wmcterm surface list`, `wmcterm screen read` (the caller only), and
`wmcterm agent-request list` available without recursively approving the
command used to inspect an approval.

- Read `wmcterm pane list` after creation and use the returned pane and surface
  IDs for later operations. Do not infer the new target from UI focus.
- Send the task only to the new surface ID. Keep the caller alive and use the
  atomic `close-others` commands below when cleaning up sibling panes.
- Claude Code's internal subagents run inside their owning Claude process.
  Lifecycle Hooks can report them, but do not claim that wmcterm moves an
  existing internal subagent into another pane. Create a separate managed
  Claude process when the user asks for a dedicated pane.

## Operate terminals

- Paste prompt text atomically with
  `wmcterm send --surface "$WMCTERM_SURFACE_ID" -- TEXT`.
- Submit a shell command as one actor operation with
  `wmcterm send-line --surface "$WMCTERM_SURFACE_ID" -- COMMAND`. This preserves
  bracketed-paste encoding and appends Enter without an observable half-sent
  state. Prefer it over separate `send` and `send-key Enter` calls for ordinary
  command lines.
- Send Enter and other individual keys separately with
  `wmcterm send-key Enter --surface "$WMCTERM_SURFACE_ID"`.
- Wait for a prompt or other stable output without blind sleeps with
  `wmcterm screen wait --surface "$WMCTERM_SURFACE_ID" --until TEXT --timeout 10s`.
  Use `--revision N` with the revision from an earlier screen read when any
  subsequent screen update is sufficient. `--change` starts its baseline when
  the wait command begins and can miss an update that already arrived.
- Read bounded full history without moving the user's viewport with
  `wmcterm screen read --surface "$WMCTERM_SURFACE_ID" --scrollback --lines 200`.
  Ordinary reads now include a revision, cursor and viewport metadata;
  `truncated_top: true` means the visible snapshot has older content above it.
- Split from the caller with
  `wmcterm pane split --pane "$WMCTERM_PANE_ID" --right` or `--down`; add
  `--no-focus` for background work.
- Create a tab in the caller pane with
  `wmcterm surface new --pane "$WMCTERM_PANE_ID"`; add `--no-focus` for
  background work.
- Scroll with
  `wmcterm terminal scroll -20 --surface "$WMCTERM_SURFACE_ID"`.

Do not run a destructive operation unless the user requested it or it is a
necessary, safe part of the task. Close the caller only with an explicit ID:
`wmcterm pane close "$WMCTERM_PANE_ID"` or
`wmcterm surface close "$WMCTERM_SURFACE_ID"`.

To keep the caller and close its siblings, use only the atomic commands:

```bash
wmcterm pane close-others --anchor "$WMCTERM_PANE_ID"
wmcterm surface close-others --anchor "$WMCTERM_SURFACE_ID"
```

Never implement close-others by enumerating items and closing them one by one;
never close the caller first. Raw RPC callers must pass an explicit UUID as
`{"anchor":"UUID"}` to `pane.close-others` or `surface.close-others`.
Explicitly targeted Agent/RPC close operations do not open a UI confirmation;
the caller's authorization and UUID are the confirmation boundary. Interactive
mouse and keyboard closes still warn before terminating live processes.

## Agent state and notifications

Let installed Agent Hooks report lifecycle state. For explicit updates, use
`wmcterm agent-event JSON`. Notify with
`wmcterm notify --surface "$WMCTERM_SURFACE_ID" --title TITLE --body BODY` only
for results, errors, permission requests, or input that needs attention.

Inspect actionable requests with `wmcterm agent-request list`. Reply with
`wmcterm agent-request reply ID --approve`, `--deny`, `--choice ID`, or
`--text TEXT`; cancel a stale request with `wmcterm agent-request cancel ID`.
