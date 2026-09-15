# Managed Codex Restore

Implementation update: 2026-09-15. The broader terminal architecture is documented
in [the product design](wmcterm终端产品设计方案.md).

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

The remote root TUI receives connection, model, cwd, and ordinary UI arguments.
The permission proxy applies `approvalPolicy`, `approvalsReviewer`, and `sandbox`
on `thread/start`, `thread/resume`, and `thread/fork`. It verifies the response
before allowing turns, and keeps `turn/start` consistent with the verified
policy. This also covers the TUI's in-session `/resume` path. The proxy has a
random loopback endpoint path, bounded clients and pending requests, and bounded
socket waits. Watcher and child connections go directly to the app-server and
never inherit root permission overrides from the proxy.

Codex 0.154.0 exposes these fields in its generated protocol schema. The real
provider smoke test checks all three modes without starting a model turn. A
provider that rejects or fails to apply the fields fails closed; startup does
not silently fall back to native Codex. Custom named permission profiles,
`--add-dir`, and external remote endpoints are rejected by this managed path.

## Launch Authority and Routing

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

- Unit tests cover mode parsing, verification failures, blocked premature turns,
  exact-once launch claims, stale lifecycle routing, and legacy serialization.
- `python3 scripts/codex-permissions-smoke.py` uses an isolated Codex home and
  real app-server to check permission round trips, with no model turns.
- `xvfb-run -a --server-args='-screen 0 1280x800x24' python3 scripts/codex-managed-restore-smoke.py`
  uses an isolated GUI, real server, and deterministic Node 22+ TUI fixture to
  check initial managed launch, three restart modes, `/resume`, labels and save.
- Standard fmt, workspace Clippy/tests, release builds, UI/runtime/window smoke,
  Windows cross-check and packaging remain required for release validation.
