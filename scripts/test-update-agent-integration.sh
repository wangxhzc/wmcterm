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
mkdir -p "$HOME/.codex" "$CLAUDE_CONFIG_DIR"

cat >"$HOME/.codex/hooks.json" <<'JSON'
{
  "theme": "keep-codex",
  "hooks": {
    "Stop": [
      {"hooks": [{"type": "command", "command": "/old/wmcterm hook receive --agent codex"}]},
      {"hooks": [{"type": "command", "command": "keep-codex-hook"}]}
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
      {"hooks": [{"type": "command", "command": "keep-claude-hook"}]}
    ]
  }
}
JSON

"$project_root/scripts/update-agent-integration.sh" "$wmcterm_binary"
first_digest="$(sha256sum \
    "$HOME/.codex/hooks.json" \
    "$CLAUDE_CONFIG_DIR/settings.json" \
    "$HOME/.agents/skills/wmcterm/SKILL.md" \
    "$HOME/.codex/skills/wmcterm/SKILL.md" \
    "$CLAUDE_CONFIG_DIR/skills/wmcterm/SKILL.md")"
"$project_root/scripts/update-agent-integration.sh" "$wmcterm_binary"
second_digest="$(sha256sum \
    "$HOME/.codex/hooks.json" \
    "$CLAUDE_CONFIG_DIR/settings.json" \
    "$HOME/.agents/skills/wmcterm/SKILL.md" \
    "$HOME/.codex/skills/wmcterm/SKILL.md" \
    "$CLAUDE_CONFIG_DIR/skills/wmcterm/SKILL.md")"
if [ "$first_digest" != "$second_digest" ]; then
    echo "Agent integration update was not idempotent" >&2
    exit 1
fi

python3 - "$HOME" "$CLAUDE_CONFIG_DIR" <<'PY'
import json
import pathlib
import sys

home = pathlib.Path(sys.argv[1])
claude = pathlib.Path(sys.argv[2])
codex_hooks = json.loads((home / ".codex/hooks.json").read_text(encoding="utf-8"))
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
assert "keep-claude-hook" in claude_text

skill_paths = (
    home / ".agents/skills/wmcterm/SKILL.md",
    home / ".codex/skills/wmcterm/SKILL.md",
    claude / "skills/wmcterm/SKILL.md",
)
skill_contents = [path.read_text(encoding="utf-8") for path in skill_paths]
assert skill_contents[0] == skill_contents[1] == skill_contents[2]
PY

echo "isolated Agent integration update test passed"
