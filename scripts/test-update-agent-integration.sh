#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd "$(dirname "$0")/.." && pwd)"
wmcterm_binary="${1:-$project_root/target/debug/wmcterm}"
if [ ! -x "$wmcterm_binary" ]; then
    echo "wmcterm executable not found: $wmcterm_binary" >&2
    exit 2
fi

test_home="$(mktemp -d)"
trap 'rm -rf -- "$test_home"' EXIT
export HOME="$test_home/home"
export CLAUDE_CONFIG_DIR="$test_home/claude-config"
test_codex_home="$test_home/codex config"
mkdir -p "$HOME" "$test_codex_home" "$CLAUDE_CONFIG_DIR"

cat >"$test_codex_home/hooks.json" <<'JSON'
{
  "theme": "keep-codex",
  "hooks": {
    "Stop": [
      {"hooks": [{"type": "command", "command": "/old/wmcterm hook receive --agent codex"}]},
      {"hooks": [{"type": "command", "command": "keep-codex-hook"}]}
    ],
    "Interrupt": [
      {"hooks": [{"type": "command", "command": "keep-codex-interrupt"}]}
    ]
  }
}
JSON

cat >"$CLAUDE_CONFIG_DIR/settings.json" <<'JSON'
{
  "theme": "keep-claude",
  "permissions": {"deny": ["Read(./secret/**)"]},
  "hooks": {
    "Stop": [
      {"hooks": [{"type": "command", "command": "/old/wmcterm hook receive --agent claude"}]},
      {"hooks": [{"type": "command", "command": "/old/wmcterm", "args": ["hook", "receive", "--agent", "claude", "--event", "Stop"]}]},
      {"hooks": [{"type": "command", "command": "keep-claude-hook"}]}
    ],
    "StopFailure": [
      {"hooks": [{"type": "command", "command": "keep-claude-failure-hook"}]}
    ]
  }
}
JSON

env CODEX_HOME="$test_codex_home" "$project_root/scripts/update-agent-integration.sh" "$wmcterm_binary"
first_digest="$(sha256sum \
    "$test_codex_home/hooks.json" \
    "$CLAUDE_CONFIG_DIR/settings.json" \
    "$HOME/.agents/skills/wmcterm/SKILL.md" \
    "$test_codex_home/skills/wmcterm/SKILL.md" \
    "$CLAUDE_CONFIG_DIR/skills/wmcterm/SKILL.md")"
env CODEX_HOME="$test_codex_home" "$project_root/scripts/update-agent-integration.sh" "$wmcterm_binary"
second_digest="$(sha256sum \
    "$test_codex_home/hooks.json" \
    "$CLAUDE_CONFIG_DIR/settings.json" \
    "$HOME/.agents/skills/wmcterm/SKILL.md" \
    "$test_codex_home/skills/wmcterm/SKILL.md" \
    "$CLAUDE_CONFIG_DIR/skills/wmcterm/SKILL.md")"
if [ "$first_digest" != "$second_digest" ]; then
    echo "Agent integration update was not idempotent" >&2
    exit 1
fi

python3 - "$HOME" "$CLAUDE_CONFIG_DIR" "$test_codex_home" <<'PY'
import json
import pathlib
import sys

home = pathlib.Path(sys.argv[1])
claude = pathlib.Path(sys.argv[2])
codex = pathlib.Path(sys.argv[3])
assert not (home / ".codex").exists(), "custom CODEX_HOME must not modify ~/.codex"
codex_hooks = json.loads((codex / "hooks.json").read_text(encoding="utf-8"))
claude_settings = json.loads((claude / "settings.json").read_text(encoding="utf-8"))

assert codex_hooks["theme"] == "keep-codex"
assert claude_settings["theme"] == "keep-claude"
assert claude_settings["permissions"]["deny"] == ["Read(./secret/**)"]
for safe_command in (
    "Bash(wmcterm capabilities)",
    "Bash(wmcterm status)",
    "Bash(wmcterm pane list)",
    "Bash(wmcterm surface list)",
    "Bash(wmcterm screen read)",
    "Bash(wmcterm agent-request list)",
):
    assert safe_command in claude_settings["permissions"]["allow"]

codex_text = json.dumps(codex_hooks)
claude_text = json.dumps(claude_settings)
assert "/old/wmcterm" not in codex_text
assert "/old/wmcterm" not in claude_text
assert "keep-codex-hook" in codex_text
assert "keep-codex-interrupt" in codex_text
assert "keep-claude-hook" in claude_text
assert "keep-claude-failure-hook" in claude_text
assert codex_hooks["hooks"]["Interrupt"]
for event in ("PreToolUse", "PostToolUse", "PreModelRequest"):
    receivers = [hook for entry in codex_hooks["hooks"][event] for hook in entry.get("hooks", [])]
    assert len(receivers) == 1, (event, receivers)
    assert receivers[0]["command"].endswith("--event " + event)
failure_hooks = claude_settings["hooks"]["StopFailure"]
failure_receivers = [
    hook
    for entry in failure_hooks
    for hook in entry.get("hooks", [])
    if hook.get("args", [])[:4] == ["hook", "receive", "--agent", "claude"]
]
assert len(failure_receivers) == 1, failure_receivers
assert failure_receivers[0]["args"][-2:] == ["--event", "StopFailure"]
for event in ("PermissionDenied", "CwdChanged", "DirectoryAdded", "Elicitation", "ElicitationResult"):
    receivers = [hook for entry in claude_settings["hooks"][event] for hook in entry.get("hooks", [])
                 if hook.get("args", [])[:4] == ["hook", "receive", "--agent", "claude"]]
    assert len(receivers) == 1, (event, receivers)
    assert receivers[0]["args"][-2:] == ["--event", event]
assert claude_settings["hooks"]["PermissionRequest"][0]["hooks"][0]["onFailure"] == "block"

skill_paths = (
    home / ".agents/skills/wmcterm/SKILL.md",
    codex / "skills/wmcterm/SKILL.md",
    claude / "skills/wmcterm/SKILL.md",
)
skill_contents = [path.read_text(encoding="utf-8") for path in skill_paths]
assert skill_contents[0] == skill_contents[1] == skill_contents[2]
PY

echo "isolated Agent integration update test passed"
