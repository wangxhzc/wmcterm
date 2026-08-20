# AGENTS.md

This file applies to the entire repository. It records the working contract for
humans and coding agents; product behavior and user-facing commands belong in
`README.md`, while architecture decisions and implementation status belong in
`docs/rust-libghostty-vt-winit-wgpu-terminal-design.md`.

## Project snapshot

wmcterm is a Rust 2024 GPU terminal workspace built on `libghostty-vt`,
`portable-pty`, `winit`, `wgpu`, and `glyphon`. The executable and package name
are both `wmcterm`. The minimum Rust version declared by the workspace is 1.90.

The application model is `Window -> Workspace -> Pane tree -> Surface`.
Terminal surfaces own PTY/VT actors and publish immutable render snapshots to
the UI. Markdown and bounded/tailing log surfaces are implemented; Browser is
only a placeholder backend and URLs are intentionally opened externally.

## Source map

- `src/main.rs`: executable, CLI, local JSON-RPC client, config/hook/skill and
  managed Agent launch commands.
- `crates/wmcterm-core`: IDs, workspace/pane/surface state, split tree, commands,
  notifications, and Agent lifecycle state.
- `crates/wmcterm-pty`: portable PTY/ConPTY process actor.
- `crates/wmcterm-terminal`: `libghostty-vt` owner actor, input encoding,
  scrollback, selection, search, and render snapshots.
- `crates/wmcterm-renderer`: `wgpu` rectangles and `glyphon` text rendering.
- `crates/wmcterm-app`: `winit` application, UI layout/input, terminal routing,
  session coordination, and JSON-RPC dispatch.
- `crates/wmcterm-ipc`: local transport and bounded actionable Agent requests.
- `crates/wmcterm-agent`: OSC notifications, normalized Agent/hook events, and
  generated Hook installation plans.
- `crates/wmcterm-codex`, `crates/wmcterm-claude`: managed Agent integrations and
  fail-closed approval bridges.
- `crates/wmcterm-config`, `crates/wmcterm-session`: layered configuration and
  persisted application state.
- `crates/wmcterm-surface`: non-terminal Markdown, log, and placeholder Browser
  backends.
- `tests/compatibility.rs`: real PTY/VT compatibility checks.
- `scripts/`: packaging, UI/runtime smoke tests, and Agent integration updater.
- `integrations/skills/wmcterm/SKILL.md`: checked-in source for the bundled
  wmcterm Agent skill; keep it aligned with the CLI and RPC behavior.

## Development rules

- Preserve the separation between terminal actors and the UI thread. PTY reads
  and VT mutation stay in the terminal actor; the renderer consumes snapshots.
- Keep UI, CLI, and Agent automation on the same core command/RPC semantics.
  Destructive RPC operations require explicit UUID targets. Interactive GUI
  closes may confirm when live processes, Agents, or requests would be lost.
- Do not infer an IPC target from UI focus when a caller supplied a workspace,
  pane, or surface ID. Respect `focus: false`/`--no-focus` atomically.
- Pending Agent authorization requests are not ordinary notifications. Marking
  notifications read must never resolve, delete, or hide an actionable request.
- Agent approvals fail closed. Preserve terminal lifecycle and surface routing
  checks; reject stale hooks, duplicate replies, unsafe bypass modes, and
  ambiguous decisions.
- Terminal byte input and GUI text editing are different paths. `wmcterm send`
  uses bracketed paste; `send-key` sends encoded control input. GUI editors draw
  their caret as an overlay and must not insert a caret glyph into text layout.
- Preserve UTF-8 boundaries and terminal cell widths. Add regression tests for
  CJK, emoji, combining characters, selection, cursor movement, and ANSI/VT
  behavior when relevant.
- Keep English and Simplified Chinese UI strings in sync. Route new actions
  through the shared action registry and configuration keybinding validation.
- Avoid unrelated formatting or generated-file churn. Never edit `target/` or
  generated wmcterm integration targets under `.agents`, `.codex`, or
  `.claude`; edit `integrations/skills` or the Hook implementation in
  `crates/wmcterm-agent` instead. `example-cmux/` is reference material only.
- `Cargo.lock` is committed because this repository builds an application.
- Update `README.md`, the design document, CLI help/capabilities, smoke scripts,
  and the bundled skill when a public contract changes.

## Required validation

Run the smallest relevant test while iterating. Before handing off a Rust code
change, run all of:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --release --bin wmcterm
```

Use these when the changed area warrants them and the host dependencies exist:

```bash
scripts/ui-smoke.sh
scripts/runtime-control-smoke.sh
scripts/window-state-smoke.sh
scripts/test-update-agent-integration.sh
cargo check --workspace --target x86_64-pc-windows-gnu
scripts/package-release.sh
```

Smoke tests may create windows, sockets, temporary directories, or screenshots;
do not run them in an unrelated live wmcterm workspace without checking their
scope. Installation into `/usr/bin`, user hook updates, package publication,
and Git pushes require explicit user authorization. Never close the caller's
pane, surface, or workspace as part of a test.

## Commit expectations

- Inspect `git status` before editing and preserve unrelated user changes.
- Stage only the intended source, documentation, tests, and required assets.
- Use an imperative, scoped commit subject (for example,
  `docs: document the current architecture`).
- Report the validation performed and any platform checks not run.
