# wmcterm

Rust-native GPU terminal workspace based on `libghostty-vt`, `winit`, `wgpu`
and `glyphon`. Each terminal surface owns a PTY/VT actor; the UI receives
immutable render snapshots and coalesces redraw wakeups.

Run the GPU terminal workspace:

```bash
cargo run
```

## Shortcuts

- `Ctrl+Shift+D`: split right
- `Ctrl+Shift+E`: split down
- `Ctrl+Shift+T`: new terminal surface tab
- `Ctrl+Shift+A`: new workspace
- `Ctrl+Tab` / `Ctrl+Shift+Tab`: next / previous surface tab
- `Ctrl+Shift+X`: close active terminal surface
- `Ctrl+Shift+W`: close active pane
- `Alt+Arrow`: focus the pane in that direction
- `Ctrl+1` … `Ctrl+9`: switch directly to a workspace
- `Ctrl+Shift+B`: toggle the workspace sidebar
- `Ctrl+Shift+P`: command palette (`Up`, `Down`, `Enter`, `Esc`)
- `Ctrl+Shift+N`: notification center
- Mouse wheel over the notification center: browse older/newer notifications
- `Mark all read` / `全部已读`: clear ordinary unread badges without deleting
  notifications or resolving pending Agent authorization requests
- `Ctrl+F`: search all terminal scrollback (`Enter`/`Shift+Enter` to navigate)
- `Ctrl+Shift+L`: filter the active log surface (`level:error,warn text`)
- `Ctrl+Shift+F`: search files in the active workspace with `rg`
- `Ctrl++` / `Ctrl+-` / `Ctrl+0`: increase, decrease, or reset terminal font size
- `Ctrl+Shift+K`: clear terminal screen and scrollback
- `Ctrl+Shift+Z`: zoom/unzoom the active pane
- `Ctrl+Shift+R`: rename the active workspace
- `Ctrl+Shift+PageUp` / `Ctrl+Shift+PageDown`: reorder the active workspace
- `Ctrl+Shift+U`: jump to the latest unread notification
- `Alt+1` … `Alt+9`: switch directly to a surface in the active pane
- `Ctrl+,`: create/open the user configuration in the system editor
- `Ctrl+Shift+C` / `Ctrl+Shift+V`: copy terminal selection / paste
- Mouse click: focus workspace/pane or activate a surface tab
- Click `×` on a surface tab: close it; the final tab leaves an empty pane with
  a clickable new-terminal entry
- Mouse drag: select and copy terminal text to the clipboard; dragging beyond
  the terminal's top or bottom edge automatically scrolls through history
- Drag a pane divider: resize adjacent panes
- Drag the terminal scrollbar thumb or click its track: seek/page through scrollback
- Right click in a terminal: paste
- Middle click in a terminal: paste
- Right click a surface tab: close it, close other tabs, or create a new terminal tab
- Right click a workspace row: create a terminal tab, rename, edit its description,
  change its directory, pin/unpin, or close the workspace
- Hover command-palette rows: preview the item that a click will execute
- Click Prompt/Search text: move the Unicode-aware insertion cursor
- Mouse wheel: terminal scrollback
- Drop files on a terminal: insert safely shell-quoted paths into the terminal under the pointer

When a TUI enables DEC mouse tracking (including SGR mode), mouse press,
release, motion and wheel events are encoded by `libghostty-vt` and forwarded
to the PTY. Hold `Shift` to bypass the application and select text or scroll
wmcterm's own scrollback instead.

Terminal-generated protocol replies are forwarded while PTY output is parsed,
independently of coalesced render snapshots. Reply bytes and pending reply
batches are bounded during a continuous output burst, so displaying an
untrusted binary cannot accumulate an unbounded input backlog that is injected
after the shell prompt returns. The reply budget automatically recovers after
the output stream has been quiet for 250 ms.

The `+ New Workspace` sidebar entry is clickable. The same commands are also
available from the command palette.
The final workspace may be closed; the empty application view keeps a clickable
new-workspace entry.

## CLI and IPC

The GUI exposes JSON-RPC through `wmcterm.sock` (and a local loopback endpoint on
Windows). Examples:

```bash
cargo run -- workspace list
cargo run -- workspace new --name demo --cwd /tmp
cargo run -- workspace activate WORKSPACE_ID
cargo run -- workspace rename WORKSPACE_ID "Agent review"
cargo run -- workspace describe WORKSPACE_ID "Reviewing parser changes"
cargo run -- workspace cwd WORKSPACE_ID /path/to/project
cargo run -- workspace move WORKSPACE_ID -1
cargo run -- workspace pin WORKSPACE_ID [--off]
cargo run -- pane split --right
cargo run -- pane split --down --command "codex"
cargo run -- pane list
cargo run -- pane zoom [PANE_ID]
cargo run -- surface new --command "cargo test"
cargo run -- surface markdown README.md
cargo run -- surface logs /tmp/build.log
cargo run -- surface list
cargo run -- surface read [SURFACE_ID]
cargo run -- surface search error [SURFACE_ID]
cargo run -- agent codex
cargo run -- ssh user@host
cargo run -- ssh user@host --tmux SESSION
cargo run -- docker CONTAINER [COMMAND...]
cargo run -- kube POD [COMMAND...]
cargo run -- send "echo hello"
cargo run -- send-key Enter
cargo run -- screen read [--surface SURFACE_ID]
cargo run -- terminal search error [--surface SURFACE_ID]
cargo run -- terminal clear [--surface SURFACE_ID]
cargo run -- terminal scroll -20 [--surface SURFACE_ID]
cargo run -- notify --title Build --body "Needs review"
cargo run -- notification list
cargo run -- agent-event '{"event":"permission_request","message":"Approve?"}'
cargo run -- hook plan --agent codex
cargo run -- hook install --agent codex --scope project
cargo run -- hook install --agent claude --scope user
cargo run -- hook receive --agent claude --event Stop < hook.json
cargo run -- skill install --scope user
cargo run -- window new /path/to/project
cargo run -- status
```

`wmcterm send --surface ID -- TEXT` uses the terminal's bracketed-paste
protocol. This keeps multiline and long prompts atomic and lets TUIs such as
Claude Code and Codex apply their normal pasted-content handling. Use
`wmcterm send-key KEY --surface ID` for Enter and other individual keys.

Hook receivers inherit the wmcterm context variables from terminals launched by
wmcterm, so concurrent agents report state and notifications to the correct
workspace, pane, and surface.
`hook plan` prints a merge-only integration fragment and never edits an agent's
configuration. `hook install` safely merges JSON hook definitions for Codex,
Claude Code, and Gemini at project or user scope; repeated installation is
deduplicated. Other supported agents still receive a plan when their plugin
format cannot be merged safely. Codex requires reviewing a newly installed or
changed command hook with `/hooks` before it runs.

The CLI is also the stable agent-control surface. Commands launched inside a
wmcterm terminal inherit `WMCTERM=1`, `WMCTERM_WORKSPACE_ID`, `WMCTERM_PANE_ID`,
`WMCTERM_SURFACE_ID`, `WMCTERM_TERMINAL_LIFECYCLE_ID`, and
`WMCTERM_CONTROL_SOCKET`. The lifecycle value changes whenever that terminal's
PTY is recreated, so delayed hooks from an older process cannot update the new
session. The legacy
`WMCTERM_SOCKET` alias is also exported for compatibility. Screen reads, sends,
control keys, searches, scrolling, notifications, and surface operations
therefore target the calling agent's own terminal by default. Use `--surface
ID` to orchestrate another terminal explicitly. `wmcterm capabilities` returns
the machine-readable protocol version, RPC method list, context variables, and
supported control keys.

`wmcterm codex-teams [CODEX_ARG...]` starts an experimental private Codex
app-server and attaches the root Codex TUI. Subagent thread-spawn events create
native wmcterm panes without stealing focus: the first opens to the right and
later agents open downward. Automatic expansion defaults to two levels and can
be capped with `WMCTERM_CODEX_MAX_AUTO_DEPTH`. This integration requires a
Codex CLI whose `app-server --listen` and `--remote` options are available.
The watcher resumes both threads loaded at startup and threads announced later
with turns excluded. This makes app-server approvals from the initial thread or
a root thread selected with `/resume` reach the centralized UI without loading
the transcript into the watcher. It recognizes every `thread/*` announcement
that carries a thread ID and periodically reconciles the loaded-thread list, so
a missed resume notification cannot permanently bypass the approval bridge.
Managed Codex launches default to `--ask-for-approval on-request --sandbox
workspace-write`: ordinary workspace operations proceed without prompts, while
requests outside the sandbox are routed to wmcterm's centralized approval UI.
For these managed app-server sessions, the generic Codex `PermissionRequest`
hook is treated as duplicate telemetry and does not create a second actionable
request; unmanaged Codex hook notifications keep their normal behavior.
If the root Codex TUI answers the same app-server request first, wmcterm consumes
Codex's `serverRequest/resolved` acknowledgement or the exact matching item
lifecycle event and closes only that notification card; terminal keystrokes and
unrelated item activity never clear an approval.
An explicit approval or sandbox option still takes precedence.
`--yolo` and `--dangerously-bypass-approvals-and-sandbox` are always rejected;
run Codex outside the managed launcher if an externally isolated environment
requires a different trust model.

Actionable Agent requests use a bounded request/reply channel rather than
ordinary notifications. Inspect them with `wmcterm agent-request list`, then
use `agent-request reply ID --approve|--deny|--choice ID|--text TEXT` or
`agent-request cancel ID [REASON]`. Requests carry their Surface and terminal
lifecycle identity, expire safely, and reject stale or duplicate responses.

Interactive close actions warn before terminating live processes. Agent and
CLI control calls that name an exact workspace, pane, or surface UUID execute
directly without opening a UI confirmation; the explicit target and the
calling Agent's approval boundary are authoritative.

The bundled `wmcterm` Agent Skill documents this control interface. Its single
checked-in source is `integrations/skills/wmcterm/SKILL.md`; hidden Agent
directories are generated discovery targets, not source locations.
`wmcterm skill install --scope user` registers the bundled source for every
repository in `~/.agents/skills`, `~/.codex/skills`, and Claude Code's personal
`~/.claude/skills` directory; project scope is the default and writes ignored
copies to `.agents/skills` and Claude Code's `.claude/skills` discovery
directory. Hook plans and JSON fragments are generated from
`crates/wmcterm-agent`, while `.codex/hooks.json` and other Agent configuration
files are merge targets only.
When `CLAUDE_CONFIG_DIR` is set, the Claude copy and user Hook configuration
follow that directory instead. Existing different content is preserved unless
`--force` is provided.

Claude Code inherits `WMCTERM_WORKSPACE_ID`, `WMCTERM_PANE_ID`,
`WMCTERM_SURFACE_ID`, and `WMCTERM_CONTROL_SOCKET`. Because Claude's Bash
permission matcher runs before shell expansion, Agent-authored control commands
should contain the literal UUIDs from that context or an earlier JSON response,
not `$WMCTERM_*` expressions or compound shell pipelines. For a separately
visible delegated task, create a pane and launch a new managed
`wmcterm agent claude` process in it. The launcher runs inside that pane's
terminal and does not allocate a second tab. Claude's own internal subagents
remain inside the owning Claude process; Hooks expose their lifecycle but do
not migrate an existing internal subagent into another terminal pane.

After replacing an installed wmcterm binary, refresh the user-level skill and
Codex hooks as the desktop/login user (not as root):

```bash
/usr/lib/wmcterm/update-agent-integration
```

The updater refreshes the wmcterm skill, removes stale wmcterm Codex and Claude
hook receivers while preserving unrelated settings and hooks, installs the
current receivers, and verifies all three skill locations plus every event in
the current Codex and Claude Hook plans. The Debian maintainer scripts
intentionally do not write a guessed user's home directory. Review a newly
changed Codex command hook once with `/hooks`.

The release executable is named `wmcterm`. Raw RPC remains available as
`wmcterm rpc METHOD PARAMS_JSON`.

## Configuration and projects

User configuration is loaded from the platform config directory under
`wmcterm/config.toml`. A project can define `.wmcterm.toml`:

Set a default directory for newly created workspaces with:

```toml
[workspace]
inherit-cwd = false
default-cwd = "~/code"
```

When `inherit-cwd` is true and an active workspace exists, a new workspace
inherits that directory. Otherwise `default-cwd` is used, followed by the
wmcterm process directory. The configured path must be an existing directory.

```toml
[workspace]
name = "wmcterm"

[[command]]
name = "Tests"
command = "cargo test"
```

Workspace layout, surfaces, terminal commands and supported agent session IDs
are restored from the platform data directory. Visible scrollback is preserved;
PTY process memory is never serialized, and commands plus Claude/Codex/OpenCode
resume identifiers are restarted.

Use `wmcterm config init`, `config show`, `config validate`, or
`config import-ghostty [PATH]`. The UI reloads valid configuration changes once
per second. Theme colors, custom keybindings, font size, scrollback size, sidebar
width, workspace cwd inheritance, notifications and window decorations are
supported. Terminal `scrollback` is a byte limit and defaults to 128 MB, matching
Ghostty's `scrollback-limit` semantics; `scrollback-lines` independently limits
logical history to 100,000 rows. Whichever limit is reached first prunes the
oldest accessible history. `config import-ghostty` prints a merged preview and
does not overwrite either configuration.

wmcterm includes English and Simplified Chinese interfaces. The default is
`auto`, which follows the operating-system locale. It can be selected explicitly:

```toml
[ui]
language = "zh-CN" # "en" or "auto"

# Set false when terminal applications should receive every shortcut that is
# not present in [keybindings].
legacy_shortcuts = true

[navigation]
# Set false to hide the clickable navigation bar. Position is "top" or "bottom".
visible = true
position = "bottom"
# Reorder or remove: sidebar, new-workspace, command-palette, notifications.
items = ["sidebar", "notifications"]

[terminal]
# Ordinary BEL events from the terminal: "never", "background", or "always".
# The default avoids notifications for Backspace/Tab boundary bells in the
# terminal currently being used, while still notifying for background work.
bell-notifications = "background"
bell-debounce-ms = 1000
```

This bell policy only applies to the terminal BEL character. Explicit Agent and
OSC notifications for permission requests, completion, and errors are not
suppressed.

The bundled default typeface is `Maple Mono Normal NF`, copied from the
`MapleMonoNormal-NF-unhinted` distribution; its OFL license is included with
the font assets.

The application/window icon is bundled under `assets/icons`, copied unchanged
from the sibling wmcssh project. The PNG is loaded by winit at runtime and the
multi-resolution ICO is embedded in Windows builds.

OSC 9/99 notifications and the wmcterm OSC 777 agent protocol are supported. OSC
777 payloads use `wmcterm;STATE;MESSAGE`, where state is `running`, `input`,
`permission`, `completed`, or `error`.

## Workspace crates

- `wmcterm-core`: Workspace, Pane tree, Surface, actions and notifications
- `wmcterm-pty`: portable PTY/ConPTY actor
- `wmcterm-terminal`: Ghostty VT owner actor and render snapshots
- `wmcterm-renderer`: wgpu rectangles plus glyphon font/atlas rendering
- `wmcterm-app`: winit UI, frame scheduling and command routing
- `wmcterm-ipc`: JSON-RPC local transport
- `wmcterm-agent`: OSC and agent state transitions
- `wmcterm-codex`: isolated experimental Codex app-server/subagent bridge
- `wmcterm-config`, `wmcterm-session`: layered configuration and restore
- `wmcterm-surface`: Markdown documents and bounded, filtering, tailing Log surfaces

Automatic updates and an embedded browser are intentionally outside wmcterm's
scope. URLs remain available through external system applications.

The Phase 0 headless VT snapshot tool remains available:

```bash
cargo run -- dump --cols 80 --rows 24 -- sh -lc 'printf "\033[32mhello\033[0m\n"'
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --workspace
cargo check --workspace --target x86_64-pc-windows-gnu
```

Build the supported install packages (Linux x86_64 Deb and Windows x86_64
NSIS installer) with:

```bash
scripts/package-release.sh
```

Artifacts are written to `target/dist/`. Packaging requires `dpkg-deb`,
`x86_64-w64-mingw32-gcc`, `x86_64-w64-mingw32-strip`, and `makensis`.

Pure layout, hit-testing, localization and renderer color-space behavior are
covered by `cargo test --workspace`. On Linux, the real-window smoke test renders
both English and Chinese interfaces under Xvfb and validates the screenshots:

```bash
# Debian/Ubuntu dependencies: xvfb xdotool imagemagick
scripts/ui-smoke.sh
scripts/runtime-control-smoke.sh
```

The resulting screenshots are written to `target/ui-smoke-en.png` and
`target/ui-smoke-zh-CN.png`. The runtime control smoke test sends real keyboard
space events through winit into a shell, creates a pane through the local IPC
socket, and verifies that the child terminal receives the wmcterm control
environment.
