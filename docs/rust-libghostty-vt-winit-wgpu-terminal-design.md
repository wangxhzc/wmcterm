# Managed Codex Restore

Implementation update: 2026-09-15. The broader terminal architecture is documented
in [the product design](wmcterm终端产品设计方案.md).

## SSH CLI Help

SSH help flags are handled locally before managed connection dispatch or legacy
workspace creation. A leading option cannot be used as a legacy workspace's
host. Password authentication continues in the dedicated managed terminal;
Agents retain its surface and connection IDs while awaiting user authentication.

## Managed SSH Terminal Modes

Agents opt into managed SSH only on an explicit user request; ordinary remote
tasks use their shell tool's SSH. `ssh HOST` and `ssh HOST --new` reuse the
existing `pane.split` RPC with direction `right` and `focus: false`, then the
existing readiness/connection flow. The anchor is an explicit `--pane` or
`WMCTERM_PANE_ID`, never UI focus. No new RPC or configuration is required.
Explicit `--surface` connections and reuse retain their existing behavior.

The Bash integration saves shell terminal modes before Readline starts. Its
`bind -x` dispatcher restores those modes before launching SSH or executing a
remote command, so SSH does not propagate Readline's disabled echo and canonical
input to the remote PTY. The prompt hook restores the saved Readline modes before
publishing readiness, including when Ctrl+C unwinds a running callback.
The isolated SSH smoke test checks manual input and visible echo after connection,
command completion, cancellation, and reconnection, plus command terminal modes.

## Workspace Rename Shortcut

Ctrl+Shift+R is the built-in workspace rename shortcut. F2 is passed to the
terminal application, including when the pointer is in the sidebar. Explicit
configured keybindings retain precedence.

## Persisted State

`TerminalResume.managed_codex` records the thread ID, mode, permission overrides,
and optional parent surface. `TerminalResume.cwd` retains the working directory.
Only registered managed launches populate this record; foreground process
detection and sidebar metadata do not authorize a launch. The additive field
preserves session format version 1. Legacy records use the previous interactive
resume policy. Missing rollouts clear stale resume data.

All three managed root modes survive a restart. Full access uses the original
GUI confirmation, whose text now states that restart preserves this choice.
Restoring never replays the initial prompt. A root gets a fresh app-server and a
fresh permission proxy; children attach to the new server without permission
overrides. Saved child surfaces are reused. Orphaned children remain at a shell.

## Permission Ownership

Claude permission suggestion choice IDs remain compatible (`always`), but the
displayed action names the operation. The request summary includes destination,
persistence, and exact rules, directories, or target mode. Multiple suggestions
remain unavailable as an ambiguous single action. Elicitation dialogs and stale
quota resumes normalize to `NeedsInput`; disabled quota waits normalize to an
idle turn boundary, and resumed execution remains `ToolUse`.

User Hook and Skill installation resolve Codex paths from `CODEX_HOME`, matching
the managed launch configuration lookup. The integration updater uses the same
root. Project installation remains relative to the project. The updater smoke
test verifies an isolated custom directory containing spaces, idempotence, and
absence of writes to the default Codex directory.

The remote root TUI receives connection, model, cwd, and ordinary UI arguments.
The permission proxy applies `approvalPolicy`, `approvalsReviewer`, and `sandbox`
on `thread/start`, `thread/resume`, and `thread/fork`. It verifies the response
before allowing turns, and keeps `turn/start` consistent with the verified
policy. This also covers the TUI's in-session `/resume` path. The proxy has a
random loopback endpoint path, bounded clients and pending requests, and bounded
socket waits. Watcher and child connections go directly to the app-server and
never inherit root permission overrides from the proxy.

The 0.158.0 `thread/settings/update` and `turn/settings/update` paths require
a verified thread. Non-null `approvalPolicy`, `approvalsReviewer`,
`sandboxPolicy`, `permissions`, and `cwd` updates are rejected before forwarding:
the managed launch profile remains authoritative until restart. This prevents
live reviewer changes and stale cached workspace roots. Ordinary settings and
null permission no-ops pass through without disconnecting the client.

Managed Claude interactive launches explicitly select `--permission-mode default`
unless the caller supplies a safe permission mode. Unattended launches default
to `auto`. This avoids inheriting Claude 2.1.283/284's changed interactive default.

Codex 0.154.0 exposes these fields in its generated protocol schema. The real
provider smoke test checks all three modes without starting a model turn. A
provider that rejects or fails to apply the fields fails closed; startup does
not silently fall back to native Codex. Custom named permission profiles,
`--add-dir`, and external remote endpoints are rejected by this managed path.

## Launch Authority and Routing

UI mode labels are Auto Review and Full Access; default interactive mode has
no mode badge. CLI, action IDs and persisted mode values remain `unattended`,
`full-access` and `interactive` for compatibility.

Agent integrations are limited to Codex and Claude Code across the action
registry, CLI, process detection, hooks and session resume. Loading older
sessions discards unsupported Agent metadata and resume commands while keeping
the workspace, terminals and scrollback. Ordinary terminal commands remain
unrestricted.

The root TUI connects to a pathless loopback WebSocket endpoint, compatible
with Codex 0.158.0 remote address validation. A per-launch bearer token is
passed using `--remote-auth-token-env` and checked by the permission proxy
before it connects upstream. Tokens are not placed in URLs or command arguments.

The GUI prepares a one-use launch token scoped to a terminal lifecycle and
surface. `codex-managed` claims arguments using that token; it does not accept
full-access command-line arguments. Normal `wmcterm agent`/`codex-teams` launches
continue to reject full-access shortcuts. Private `agent.codex.*` methods check
explicit surface and lifecycle identities. Full-access registration additionally
requires the GUI claim, and duplicate claims/registrations are rejected.

The proxy reports verified root settings to the UI. The watcher reports child
settings without changing them. Mode labels are independent of task lifecycle
and turn status. Saving retains waiting child surfaces even if their PTY could
not start. Enter on an exited root reissues a fresh claim and retries the saved
thread. Explicit native resume remains available as a user-selected alternative.

## Validation

SessionStart delivery carries a receiver-generated nanosecond timestamp. A
per-terminal guard rejects older starts and ambiguous legacy re-entry into a
retired session, while allowing a fresh invocation to resume it. Guards reset
on PTY restart and cap identity history at 4096 sessions without evicting replay
protection. Approval registration checks Claude session identity and scopes IDs
to terminal lifecycle and core session generation. Session replacement cancels
pending approvals; polling old completed grants returns deny after replacement.

GUI keyboard events supply winit's layout-aware unmodified codepoint. Printable
AltGr text does not become a Control+Alt command. Modified Enter never predicts
submission, and Hook-owned state is not overridden by input heuristics. Local
editors and modal input release terminal focus until they close.

`wmcterm diagnostics` / `app.diagnostics` reports executable paths, observed
Hook provenance and snapshot protocol flags; CLI version probes have a timeout.
Observed provenance is diagnostic data, not an authorization credential.

Terminal input routing honors TUI mouse tracking before local scrollback, with
Shift as the local override. Right/middle releases target the original terminal;
unpressed motion reaches the VT encoder for mode filtering. Focus transitions
and keyboard releases are encoded in the actor. Only keys whose press reached
the PTY produce releases, and restart clears input ownership. Legacy Ctrl bytes
remain available without Kitty negotiation; enhanced input supplies unshifted
codepoints and excludes misleading platform text on Control chords. Local search
uses Ctrl+Alt+F/G, leaving Ctrl+F/G for Codex and Claude.
`xvfb-run -a python3 scripts/input-protocol-smoke.py` exercises actual window
events against an isolated PTY byte recorder, including application scrolling
with local history. Ordinary right click follows application mouse reporting;
Shift+right click opens the local terminal menu.

Claude input-wait telemetry observes state without registering an actionable
request. Informational `notification` events preserve both reducer and core
session state; only explicit quota resume activity marks the turn running.
Real approval registrations remain independent and retain surface precedence.
Claude mode metadata uses native flags until a validated root Hook supplies
`permission_mode`, cached by terminal lifecycle and session. Unknown modes and
child hooks cannot overwrite it, and metadata never authorizes execution.
Application regression checks reuse the isolated winit fixture to exercise
approval/notification interleaving, read-all, allow/deny/expiry and late clicks;
sequenced Hook replay, independent terminals and rejected child lifecycle hooks;
session replacement, missing mode metadata and PTY restart with session resume.
Session replacement refreshes the core lifecycle identity even within the same
PTY. Root SessionStart sequence checks run before mode-cache mutation.
`xvfb-run -a node scripts/claude-notifications-smoke.mjs` verifies notification
state and mode transitions through a GUI with temporary config and RPC socket.

- Unit tests cover mode parsing, verification failures, blocked premature turns,
  exact-once launch claims, stale lifecycle routing, and legacy serialization.
- `python3 scripts/codex-permissions-smoke.py` uses an isolated Codex home and
  real app-server to check permission round trips, with no model turns.
- `xvfb-run -a --server-args='-screen 0 1280x800x24' python3 scripts/codex-managed-restore-smoke.py`
  uses an isolated GUI, real server, and deterministic Node 22+ TUI fixture to
  check initial managed launch, three restart modes, `/resume`, labels and save.
- Standard fmt, workspace Clippy/tests, release builds, UI/runtime/window smoke,
  Windows cross-check and packaging remain required for release validation.
