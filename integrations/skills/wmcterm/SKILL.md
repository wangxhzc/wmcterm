---
name: wmcterm
description: Control wmcterm terminals, workspaces, notifications, and agent state. Use when an agent inside wmcterm operates its terminal UI, or when the user explicitly requests wmcterm SSH for remote-server tasks. Does not apply to ordinary SSH or shell work.
---

# wmcterm Control

Managed Agent launch, detection, hooks and session resume support only Codex
and Claude Code. Other programs can be run as ordinary terminal commands.
The UI labels `unattended` mode as "Auto Review" and `full-access` as "Full
Access"; default interactive mode has no mode badge. CLI mode values are unchanged.

Terminal input follows negotiated TUI protocols for Codex and Claude: mouse
hover/buttons/wheel, focus reports and enhanced keyboard release events.
Shift forces local mouse operations. Ordinary right click follows application
mouse reporting; Shift+right click opens the terminal menu.
Local history search uses Ctrl+Alt+F / Ctrl+Alt+G; Ctrl+F / Ctrl+G reach the TUI
unless explicitly overridden in user keybindings.

Use `wmcterm` as the stable control interface. Start with `wmcterm capabilities`
when the protocol is uncertain.
Use `wmcterm diagnostics` (`app.diagnostics`) to inspect running/client binary
paths, provider versions, observed Hook source and per-terminal input protocols.
It does not change configuration or installation. New Hook receivers timestamp
SessionStart at invocation; do not replay or invent these timestamps. Retired
sessions require a fresh receiver invocation or a new terminal lifecycle.
Approval IDs are opaque and scoped to terminal lifecycle and session generation.

The GUI pane-focus shortcut is `Ctrl+Alt+Arrow`; `Alt+Arrow` is passed through
to the terminal application.
The workspace rename shortcut is `Ctrl+Shift+R`; F2 is passed to the terminal
application, including when the pointer is in the sidebar.

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

## Operate SSH from a local Agent

- Use managed SSH below only when the user explicitly requests `wmcterm ssh`
  or asks to use wmcterm for the SSH connection. A generic request to SSH or work
  on a remote server does not opt in, even when the Agent runs inside wmcterm;
  use ordinary SSH through the Agent's shell tool by default.
  Ordinary system `ssh` is not intercepted or converted by Hooks.
- Use the caller IDs and returned connection record directly. A successful
  connection or `exec --wait` response supplies the state and result; do not
  follow it with routine status, screen, pane-list, or help calls. Inspect only
  on timeout, authentication, failure, or uncertain protocol support; older
  builds support `wmcterm --help`, newer builds also accept `wmcterm ssh --help`.
- Reuse only a dedicated surface recorded for this task, with its saved host and
  connection ID. Execute with both saved `--connection ID` and `--host HOST`:
  the terminal actor atomically checks the host, connection and ready state,
  so a separate status query is unnecessary. On mismatch, stop and report the
  changed connection; never adopt the replacement ID automatically. If no recorded
  connection exists, create a dedicated terminal rather than taking over another
  task's session.
- Use a dedicated idle terminal, not the surface currently running the Agent.
  Prefer `wmcterm ssh HOST_ALIAS --pane CALLER_PANE_UUID` to create a dedicated
  right split, like a separate Agent pane, and wait for local and remote readiness
  in one tool call. New connections default to a split, not a tab; `--new` is
  optional. Without `--pane`, the CLI uses `WMCTERM_PANE_ID`, never UI focus.
  This always preserves focus and defaults to a total 30-second wait; use
  `--timeout-ms N` (maximum 300000) to adjust it. The default local shell must be Bash 4.2.46+;
  explicitly launched commands and older terminals may not have integration.
- Save `{surface_id, host, connection_id}` from the response as this task's
  connection record, including on timeout. A local initialization timeout has
  `wait_phase: "local_ready"`, `requested_host` and no connection ID. Keep that
  surface; read its screen and resume with `ssh HOST --surface UUID --wait`
  after resolving initialization. Never repeat `--new` to recover a timeout.
  RPC failures include recovery IDs in the error where available; query that
  surface before retrying, and never send input to force readiness.
- Connect using `wmcterm ssh HOST_ALIAS --surface UUID --wait`. SSH config
  supplies ports, identities and jump hosts. This never changes focus. `--wait`
  also waits for local Bash initialization, so manual status polling is unnecessary.
  Save the returned connection ID together with the host and surface UUID, even
  if the wait times out. Continue polling that same connection until `state` is
  `ready`; stop on a changed ID, disconnection or error. Never execute while
  authentication or bootstrap is pending.
- Run `wmcterm ssh exec --surface UUID --connection CONNECTION_ID
  --host HOST_ALIAS --command-file /absolute/script.sh --wait` using the saved
  connection record, and inspect
  `command.state`, `command.exit_code` and `command.output`. Commands execute
  in the same remote Bash (4.2.46+), retaining cwd. A zero CLI exit status only
  means the RPC succeeded, not that the remote command succeeded.
- Use exactly one command source: `--command TEXT`, `--command-file PATH`, or
  `--command-stdin`. File/stdin input is UTF-8, at most 16384 bytes, without NUL;
  it is sent as remote Bash source, not uploaded as a remote filename. Prefer
  files or stdin for quotes, dollar signs and multiline scripts. With a process
  API, pass an argument array and stdin directly; when a shell is necessary,
  use a quoted heredoc delimiter to prevent local expansion. Never nest raw
  single quotes or treat JSON encoding as shell escaping. Combine independent
  read-only checks in one script with explicit output labels and exit handling;
  structured commands on one connection are serial, and only the latest result
  is retained.
- Query `wmcterm ssh status --surface UUID`. A wait timeout does not cancel or
  rerun the command. Retain connection/command IDs, and do not retry a command
  whose outcome is unknown. Only the latest command result is retained.
  Always pass the saved `--connection` to exec and cancel: omitting it makes the
  CLI bind the current connection, which may belong to a different server now.
- Cancel with `wmcterm ssh cancel --surface UUID --connection CONNECTION_ID
  --command-id COMMAND_ID --wait`. Wait for readiness before another command.
- `connecting` includes authentication and bootstrap waits. Read the target
  screen and let the user handle passwords, MFA and host-key confirmation.
  Do not bypass host verification or derive authentication decisions from text.
  Report the saved surface ID and host when user input is needed. Keep that
  terminal and connection while waiting; do not create another terminal or
  reconnect merely because password authentication is required. After the user
  authenticates, query the same connection and resume once it is ready.
- `send`, `send-key` and `screen read` remain available for interactive programs;
  input during a tracked command makes its result unknown. Resume structured
  execution only after the Bash prompt is ready. Never clear an unknown input
  line or interrupt a busy terminal just to establish SSH.
- Remote scripts live temporarily in `~/.wmcterm/tmp/` and delete themselves
  when loaded. No remote Agent, daemon, or persistent shell configuration is
  installed. Protocol events coordinate terminal state, never authorize actions.
- Explicit legacy SSH arguments (such as `--tmux`) still create an ordinary SSH
  workspace without this managed protocol. Bare `ssh HOST` creates a managed split.

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
- Treat Codex approval requests with `kind: "writeStdin"` as input to an
  existing command, not as a new command launch. They may be denied or approved
  once, but must never be converted into a persistent or session-wide grant.
- A Codex `Interrupt` hook ends the active turn without ending the live Codex
  session. Do not close its pane, surface, or PTY in response to that event.
- Codex `SubagentStart` and `SubagentStop` hooks keep the parent workspace
  running while any subagent remains active. Stops retain a bounded per-session
  tombstone so a recent out-of-order start cannot reactivate the same agent.
  Normal execution and upgrades do not change Codex configuration. Only an
  explicit `wmcterm hook install --agent codex --scope user` writes under
  `CODEX_HOME` (default `~/.codex`); user Skill installation uses the same root.
  Use `--scope project` for project configuration, and have the user
  review installed changes in Codex `/hooks`.
- Manually launched Codex processes share the same turn-state tracking. wmcterm
  reports `--approve-for-me` as auto-review and
  `--dangerously-bypass-approvals-and-sandbox` as dangerous full access without
  treating either permission mode as evidence that a turn is running.
- Session restore keeps wmcterm-launched Agents resumable even after their PTY
  exits. A manually launched Agent is resumable only while its lifecycle is
  still running and it remains the terminal's foreground process when wmcterm
  closes; an explicit manual exit must reopen as a normal terminal instead of
  reconnecting the old session. A restored manual Agent runs under an
  interactive shell so exiting the Agent returns to the prompt without closing
  the terminal surface or losing its restored scrollback.

## Launch Claude safely

- Launch a managed Claude Code process with `wmcterm agent claude`.
- Claude `StopFailure` reports a failed turn while its session and background
  subagents remain alive. Do not close the terminal in response to this hook.
- Claude Code 2.1.268+ also fires `PermissionRequest` in `-p`/`--print` mode.
  Inside wmcterm these requests use the same approval bridge and may wait for
  a human. Registration and polling share a nine-minute approval deadline;
  late or cancelled approvals are denied. In-flight IPC and cancellation
  cleanup can finish later. Auto mode does not guarantee no prompts.
- Only when the user explicitly requests unattended execution, use
  `wmcterm agent --mode unattended claude`; this selects Claude's native `auto`
  permission mode. Never pass `bypassPermissions` or dangerous-skip flags.
- Managed Claude interactive launches default to `--permission-mode default`;
  an explicitly supplied safe permission mode takes precedence.
- Claude MCP elicitation dialogs and stale quota auto-resume notifications mean
  waiting for input. A disabled quota wait returns to idle without ending the session.
  Input telemetry creates no actionable request; subsequent activity clears the wait.
  Informational `notification` events preserve task state, including `agent_completed`.
  Real approvals use `agent.approval.register` and remain pending across telemetry.
- Native Claude permission flags set display metadata. Validated root Hook
  `permission_mode` overrides startup flags for the current session and lifecycle;
  child hooks and unknown modes do not change it or grant permissions.
  Child `SessionStart` / `SessionEnd` hooks are rejected. Session changes reset
  cached mode metadata; sequenced duplicate/stale hooks cannot overwrite it.
- For Claude approvals, `always` applies the one displayed permission suggestion.
  Review its operation, scope, storage destination, and rules or directories;
  it may change a session mode rather than create a permanent allow rule.
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

In the terminal UI, Ctrl+C is forwarded to the foreground process even when text
is selected. Use Ctrl+Shift+C to copy the selection.

The workspace sidebar shows a scrollbar only when its filtered list overflows.
Drag its thumb or click its track to scroll; the rail is separate from workspace
rows and the sidebar resize edge. Mouse-wheel scrolling remains available.

The sidebar scrollbar track extends to the bottom. The 32px blank context-menu
area follows the last workspace as part of the scrollable list.

Managed Codex sessions with a saved managed restore record retain their original
interactive, auto-review, or dangerous full-access mode after restarting wmcterm.
The application rebuilds the Teams bridge, verifies server-side permissions, and
reattaches saved child surfaces without focus changes or duplicate panes. Child
threads retain their own permissions and cwd. An unavailable parent leaves its
child at a shell with an explanation. Legacy records and manually launched Agents
keep the ordinary interactive resume policy.

Full-access restore is performed by the application using the persisted GUI
choice. Agents must not call private `agent.codex.*` launch/restore methods or try
to obtain GUI launch tokens. CLI full-access and bypass options remain blocked.
An integration or permission-verification failure stops managed startup instead
of falling back automatically. Enter retries an exited terminal; a native
`codex resume` in a new shell is an explicit alternative without Teams ownership.
Managed remote `/resume` applies permissions through the server protocol, not TUI
permission flags. `--add-dir`, an external `--remote`, and custom `permissions`
overrides are currently unsupported by the managed permission bridge.
Managed Codex also rejects non-null permission, reviewer, and cwd changes through
`thread/settings/update` or `turn/settings/update`. Restart through wmcterm with
the desired mode and workspace; ordinary model settings remain available.

The sidebar displays interactive, auto-review, or dangerous full access
independently of idle/running/waiting/completed state. Managed labels follow
verified server settings; manual labels follow foreground process detection.

A Codex session ID alone is not sufficient for automatic resume. On save/load,
wmcterm checks for a nonempty matching rollout in the current Codex home; absent
records restore a shell and clear stale Agent metadata while preserving terminal
history and layout. Explicit launch commands are preserved.
