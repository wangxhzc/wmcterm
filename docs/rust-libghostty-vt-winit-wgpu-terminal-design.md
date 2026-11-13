# wmcterm Architecture and Implementation Notes

wmcterm is a Rust 2024 GPU terminal workspace built on `libghostty-vt`,
`portable-pty`, `winit`, `wgpu`, and `glyphon`. The application model is
`Window -> Workspace -> Pane tree -> Surface`. Each terminal actor owns its
PTY/VT state and publishes immutable render snapshots to the UI.

This document records implementation mechanisms, compatibility findings, and
validation procedures. Dated entries identify the provider versions examined
for that change; later entries may cover additional versions.

- [Runtime resource boundaries](#runtime-resource-boundaries-2026-11-11)
- [Persisted state](#persisted-state)
- [Managed resume compatibility](#managed-resume-compatibility)
- [Permission ownership](#permission-ownership)
- [Launch authority and routing](#launch-authority-and-routing)
- [Validation](#validation)

See the [README](../README.md) for commands and configuration, the
[product design](wmcterm终端产品设计方案.md) for product models and design history,
and [AGENTS.md](../AGENTS.md) for the source map and development requirements.

## Native Codex Permission Wait Recovery (2026-11-10)

Native `codex resume` sessions have no managed approval reply channel. Previously
their `PermissionRequest` hook set a telemetry wait with no tool completion hook
to clear it, leaving the sidebar waiting while Codex continued working.

The generated Codex plan now includes `PreToolUse` and `PostToolUse`, verified
against the installed 0.167.0 command input schemas and isolated runtime fixtures.
The receiver forwards only correlation metadata, never tool responses. Native
tool events require an explicit surface, current terminal lifecycle, matching
root session, turn, tool name/input and receiver timestamp. The bounded tracker
keeps at most 64 native waits, 128 active tools and 128 completion tombstones;
each tool input is limited to 64 KiB and is not persisted to the session file.
Codex's Bash approval input adds a description absent from execution hooks;
normalization removes only that approval prose when comparing command identity.
Parallel identical calls retain their wait until all candidate calls complete.
Denied tools do not emit PostToolUse. A root `PreModelRequest` for the same turn
ends its preceding native waits, with a bounded per-turn timestamp barrier to
reject late permission/tool delivery. Model request content is not forwarded.
Duplicate/late deliveries cannot reactivate a completed turn. Prompt submission
opens a new turn; stop/interruption closes it without ending the session.

Native telemetry cannot complete or hide an actionable request. Managed Codex
continues to use its app-server watcher; duplicate native tool hooks are ignored.
Older installed hooks retain their legacy behavior until explicitly reinstalled
and reviewed in Codex. Packaging never changes the user's Hooks or running PTYs.

Validation includes tracker and RPC regressions plus
`xvfb-run -a python3 scripts/codex-native-wait-smoke.py target/debug/wmcterm`.
The smoke uses a disposable Codex home, an independent GUI/socket and a loopback
model fixture; it does not call paid APIs or modify existing workspaces.

## Agent Release Compatibility (2026-10-20)

The audit uses installed Codex 0.163.0 and Claude Code 2.1.304, GitHub release
tags, upstream source, and generated app-server schemas. Codex 0.163.0 fixes
connection-bound request replies and cold resume of saved permissions/network
grants, and enables instant turn interruption. Regression coverage checks the
watcher's same-connection replies and stale-connection cancellation. The managed
permission profile continues to take precedence over restored settings, including
whole-table sandbox overrides. Claude's public tree exposes plugins and mods,
not the CLI core; core behavior remains a documented/runtime contract.

Claude PermissionRequest command hooks set `onFailure: "block"` (requires
2.1.295+). Telemetry hooks retain non-blocking failures. UserPromptSubmit
projects activity without storing prompt text. Managed launches reject detached,
cloud, Desktop and tmux entry points. Background service sessions need a future
explicit ownership/resume design before they can be managed safely.

The OSC 7501 scanner keeps at most 64 records, evicts the least recently updated,
validates hierarchical ids and base64/control-character limits, supports clear,
shell-prompt cleanup and RIS reset, and deduplicates aggregate state. Reports
travel through the terminal actor. The UI prefers identity-bearing hooks and
pending actionable requests; OSC never grants permissions or creates requests.

Codex InputBridge handles ordinary question sets and primitive MCP form schemas.
Secret questions, URL/device verification and unsupported schema constraints
remain native. `agent.input.register` reuses scoped request storage and UI;
`agent.input.poll` returns the generic completion record after checking the live
surface/session generation. Answers are maps of field ids to strings, validated
before resolution and converted to upstream response shapes. The existing text
editor collects fields sequentially and draws its caret as an overlay. A maximum
nine-minute deadline, external-resolution reconciliation, turn completion and
disconnect cancellation bound request lifetime. Failed registration leaves the
native TUI response channel intact. Completed replies remain bounded/idempotent.

Permissions requests expose a separate `strict` decision, mapped to
`strictAutoReview: true, scope: "turn"`. Other approval kinds reject it. Existing
once/session choices retain their semantics. Test contracts use the installed
generated schemas rather than copying assumptions from release notes.

Sources: [Codex release](https://github.com/openai/codex/releases/tag/rust-v0.163.0),
[permission protocol](https://github.com/openai/codex/blob/rust-v0.163.0/codex-rs/app-server-protocol/src/protocol/v2/permissions.rs),
[Claude release](https://github.com/anthropics/claude-code/releases/tag/v2.1.304),
[Claude hooks](https://code.claude.com/docs/en/hooks),
[OSC 7501](https://www.superlogical.com/rex/docs/build/program-status).

## Managed Codex Turn State

The permission proxy projects verified app-server `turn/started` and
`turn/completed` notifications into the shared Agent event reducer. The UI
validates the explicit surface, terminal lifecycle and registered session.
Completion must match the active turn ID; unverified threads and stale
completions cannot overwrite the current state. This covers remote TUI launches
whose task-start Hook is unavailable, including command-palette auto review.
Ephemeral reviewer threads remain permission-verified but cannot claim the
surface session or project turn telemetry. Codex hooks carry their native
session ID, and managed surfaces reject hooks for a different registered thread.

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

The application checks session changes every five seconds and writes checkpoints
on a single worker with a one-job queue. Terminal histories are requested
asynchronously every sixty seconds. Final shutdown requests histories concurrently
with a shared 500 ms deadline, then waits for its final save after earlier jobs.
Cached histories survive metadata-only checkpoints. Writes use unique sibling
temporary files, sync contents, atomically replace the destination and sync its
parent directory on Unix. Failed checkpoints are retried.

## Runtime Resource Boundaries (2026-11-11)

IPC accepts at most sixteen concurrent connections, with five-second socket
read/write inactivity timeouts, 1 MiB request frames and 32 MiB response frames.
Connection handlers do not block the accept loop or server shutdown. Frame
limits include the newline. Windows authentication lines are limited to 128 bytes.

PTY reader and event queues each retain at most 64 chunks; terminal events retain
64 messages. Input queues retain at most 128 commands and 16 MiB of payload at
each actor boundary, including PTY writes in progress. Shutdown uses independent
atomic state and interrupts backpressure waits. Raw terminal output is preserved
under backpressure; excess input is rejected rather than partially enqueued.
The UI bridge retains at most 4096 events and coalesces adjacent snapshots for
the same terminal. Actionable Agent events are never evicted to make room.

Document refresh uses one worker and bounded request/result queues, with fair
scheduling once per second. UI-side filter edits survive in-flight log refreshes.
Each log batch reads at most 1 MiB from the captured file extent and advances by
actual bytes read. Partial lines and stored lines are limited to 64 KiB; retained
text is limited to 8 MiB as well as the configured line count.

Application responsibilities are separated into `rpc`, `agent_coordinator`,
`document_refresh`, `session_persistence` and `terminal_runtime` modules.
Closing surfaces, panes and workspaces shares terminal runtime cleanup, including
hook guards, input ownership and cancellation of requests for the closed lifecycle.
Business regression tests use a headless event proxy; GUI smoke tests retain winit.

## Managed Resume Compatibility

Foreground Agent detection matches executable names and Node/Bun CLI scripts,
and excludes management commands (updates, authentication, help, diagnostics,
and MCP/plugin administration). Session, resume, and execution commands remain
eligible; prompt arguments and unrelated filenames do not establish identity.

`TerminalResume.managed_codex` records the thread ID, mode, permission overrides,
and optional parent surface. `TerminalResume.cwd` retains the working directory.
Only registered managed launches populate this record; foreground process
detection and sidebar metadata do not authorize a launch. The additive field
preserves session format version 1. Legacy records use the previous interactive
resume policy. Missing rollouts clear stale resume data.

Restored manual Agent commands remain eligible for persistence without a new
interaction or hook. Retained surface session identity bridges consecutive
restarts; an exited/failed lifecycle or foreground detection clearing the Agent
prevents replay after the Agent has returned to the shell.

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

The permission proxy also records the verified response's cwd and runtime
workspace roots. `turn/start` accepts omitted, null, or identical workspace
fields, removes them before forwarding, and rejects changes. A legacy response
without runtime roots is treated as an empty list. Settings updates also reject
runtime workspace root overrides. Model and other ordinary settings still pass.

Managed Claude interactive launches explicitly select `--permission-mode manual`
unless the caller supplies a safe permission mode. Unattended launches default
to `auto`. This avoids inheriting Claude 2.1.283/284's changed interactive default.
The legacy `default` name remains accepted when explicitly supplied. The launcher
rejects bare, safe, restricted, and custom setting-source modes that suppress
settings-file hooks. Additional JSON/file settings and discovered user/project
settings are checked for hook-disabling fields, and inherited SIMPLE/SAFE_MODE
flags are rejected without modifying the environment. This is launch validation,
not proof of hook execution: installed hooks and administrator policy still
determine actual delivery, and configuration can change after launch.

Codex 0.160.0's generated protocol schema and Claude Code 2.1.289's CLI and
official changelog are the integration baseline. The real provider smoke test
checks all three modes, saved permission resumes, explicit mode changes, and
restoration without starting a model turn. A
provider that rejects or fails to apply the fields fails closed; startup does
not silently fall back to native Codex. Custom named permission profiles,
`--add-dir`, and external remote endpoints are rejected by this managed path.
In 0.160.0, resuming an already-loaded thread retains its live permissions even
when overrides are supplied. The proxy rejects a mismatched response. The real
permission smoke unsubscribes before testing changed restore profiles, while the
managed GUI restart smoke verifies restoration with a new app-server process.

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

## Hook and Input Integration

The release baseline is Codex 0.163.0 and Claude Code 2.1.304. Claude command
hooks use native executable-plus-args invocation, retaining `onFailure: block`
for permissions. Explicit reinstall removes obsolete wmcterm shell entries while
preserving unrelated hooks. No worktree creation/removal hooks are installed:
`WorktreeCreate` replaces provider behavior and is not an observation API.

Claude `PermissionDenied` is informational auto-review telemetry, including the
tool and denial reason. It never creates an actionable permission or requests a
retry. Root `CwdChanged` and `DirectoryAdded` observations update the core Agent
session; child observations cannot change root directory metadata. Additional
directories are deduplicated, bounded to 128, persisted compatibly and cleared on
session replacement. Only the workspace's selected surface updates its displayed
cwd; per-terminal cwd and session diagnostics retain inactive surfaces' paths.

The IPC crate owns bounded provider-neutral form fields, schema validation and
answer conversion. Codex retains its public input API while delegating shared
validation. Claude `Elicitation` supports the same primitive MCP form subset. A
supported hook blocks and owns the dialog, returning typed content or `cancel` on
denial, timeout, stale session or control failure. Unsupported schemas, secrets
and URL requests return no decision and retain native interaction. Claude skips
`ElicitationResult` for a hook-owned answer. Native result reconciliation requires
an explicit elicitation ID and matching server, session, surface and lifecycle;
unidentified results never cancel other requests. Input polling and late replies
check the current session generation and terminal lifecycle. Neither read-all nor
ordinary auto-denial notifications resolve actionable forms.

SessionStart delivery carries a receiver-generated nanosecond timestamp. A
per-terminal guard rejects older starts and ambiguous legacy re-entry into a
retired session, while allowing a fresh invocation to resume it. Guards reset
on PTY restart and cap identity history at 4096 sessions without evicting replay
protection. Approval registration checks Claude session identity and scopes IDs
to terminal lifecycle and core session generation. Session replacement cancels
pending approvals; polling old completed grants returns deny after replacement.
Claude `SessionEnd` also ends the lifecycle and cancels all requests bound to
that surface/lifecycle, including hooks without PID metadata. Completed input
answers and approvals become unreadable after exit or resume. Ordinary `Stop`
and `StopFailure` retain their turn-level semantics.

### GUI Input Ownership

GUI keyboard events supply winit's layout-aware unmodified codepoint. Printable
AltGr text does not become a Control+Alt command. Modified Enter never predicts
submission, and Hook-owned state is not overridden by input heuristics. Local
editors and modal input release terminal focus until they close or relinquish
keyboard ownership. Notification panels release terminal focus unless an
explicit editor retains keyboard ownership. IME commits and preedit obey the
same modal boundaries as keyboard input. Terminal search visibility is separate
from search focus: clicking a terminal restores input while preserving search
highlights; clicking the search field resumes editing. Scrolling tabs in a
background pane changes its selected tab without moving keyboard focus.
File drops insert the shell-quoted path into the terminal under the pointer,
then focus that pane and release local search ownership without submitting the
command. Drops blocked by menus or modal editors leave terminal input and focus
unchanged. Window activation is requested only after a successful user drop.

### Diagnostics and Terminal Protocols

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

### Notifications and Mode Metadata

Ordinary notification retention defaults to seven days, configured with
`[notifications] retention-days`. On startup, configuration reload, and every
sixty seconds, the UI event loop removes expired read and
unread entries, reconciles surface attention through the shared request-state
reducer, and clamps notification selection and scrolling. Pending actionable
requests keep their independent deadlines. Sidebar rows containing Agent
metadata use a neutral gray leading rail when inactive. The selected workspace
uses the existing blue rail; title and Agent label colors retain their usual styling.

Claude input-wait telemetry observes state without registering an actionable
request. Informational `notification` events preserve both reducer and core
session state; only explicit quota resume activity marks the turn running.
Managed Codex terminals and Hook-connected terminals with current Agent metadata
suppress generic BEL notifications: explicit Agent notifications own completion
and attention delivery, avoiding a second terminal-bell entry regardless of event
ordering. Ordinary terminals retain the configured BEL policy and debounce.
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

## Validation

The following checks cover the implementation contracts described above. Run
the relevant checks for the changed area; results belong to the individual
validation run.

- Unit tests cover mode parsing, verification failures, blocked premature turns,
  exact-once launch claims, stale lifecycle routing, and legacy serialization.
- Input regression tests cover RPC/socket failures, concurrent reply/disconnect,
  64 bridge requests, 256 retained forms, bounded completion-history eviction,
  duplicate scope validation, numeric retries, optional fields, IME preedit/commit,
  and 32-option geometry at 1x/1.5x/2x scales. Duplicate input registration checks
  workspace membership before returning an existing request and ignores form
  records whose pending/completed request has already been evicted.
- `xvfb-run -a node scripts/codex-input-smoke.mjs target/debug/wmcterm` checks
  real PTY OSC fallback/Hook precedence, pending-request precedence, sequential
  forms, Unicode paste, keyboard/mouse selection, cancellation and expiry while
  editing. Repeat with `WINIT_X11_SCALE_FACTOR=2` and a 2560x1440 Xvfb screen for
  physical high-DPI rendering; screenshots are written under `/tmp` by default.
- `xvfb-run -a python3 scripts/agent-live-contract-smoke.py target/debug/wmcterm`
  drives installed Codex/Claude CLIs against a loopback-only model fixture.
  Codex traverses app-server, watcher and GUI before verifying the answer in the
  next upstream tool output and turn completion. Claude exercises an actual
  stdio MCP elicitation through the installed CLI and checks the typed answers
  received by the MCP server. Claude also verifies a working allow
  control and denied execution for crashing, missing, timed-out and invalid-JSON
  PermissionRequest hooks. Configuration and tool marker files are disposable;
  these tests never call a paid model API or modify user Hook installations.
- `python3 scripts/codex-permissions-smoke.py` uses an isolated Codex home and
  real app-server to check permission round trips, with no model turns.
- `python3 scripts/codex-release-smoke.py` checks Codex 0.163.0 cold resume,
  saved sandbox/network settings and explicit managed permission overrides
  against the real app-server, with no model turns.
- `xvfb-run -a node scripts/claude-input-smoke.mjs target/debug/wmcterm` checks
  concurrent and duplicate forms, typed replies, native reconciliation,
  unsupported-schema fallback and session isolation.
- PTY compatibility tests cover Ctrl+C, Ctrl+X and F9 in legacy/Kitty modes and
  CJK/emoji bracketed paste. CRLF is normalized before libghostty paste encoding
  so Windows line endings produce one newline instead of two. The input
  protocol GUI smoke also checks key press/release delivery.
- `xvfb-run -a --server-args='-screen 0 1280x800x24' python3 scripts/codex-managed-restore-smoke.py`
  uses an isolated GUI, real server, and deterministic Node 22+ TUI fixture to
  check initial managed launch, three restart modes, `/resume`, labels and save.
- Standard fmt, workspace Clippy/tests, release builds, UI/runtime/window smoke,
  Windows cross-check and packaging remain required for release validation.
