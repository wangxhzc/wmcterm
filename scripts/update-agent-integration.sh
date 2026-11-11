#!/usr/bin/env bash
set -euo pipefail

# Run this as the desktop/login user after replacing the wmcterm executable.
# Maintainer scripts must not guess a user's HOME or write user config as root.
if [ "$(id -u)" -eq 0 ]; then
    echo "refusing to register Agent integration in root's HOME; run as the desktop user" >&2
    exit 2
fi

wmcterm_binary="${1:-/usr/bin/wmcterm}"
if [ ! -x "$wmcterm_binary" ]; then
    echo "wmcterm executable not found: $wmcterm_binary" >&2
    exit 2
fi
if [ -z "${HOME:-}" ] || [ ! -d "$HOME" ]; then
    echo "a valid desktop-user HOME is required" >&2
    exit 2
fi

wmcterm_binary="$(readlink -f "$wmcterm_binary")"
claude_config_root="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
codex_config_root="${CODEX_HOME:-$HOME/.codex}"
"$wmcterm_binary" skill install --scope user --force
"$wmcterm_binary" hook install --agent codex --scope user
"$wmcterm_binary" hook install --agent claude --scope user

for skill_file in \
    "$HOME/.agents/skills/wmcterm/SKILL.md" \
    "$codex_config_root/skills/wmcterm/SKILL.md" \
    "$claude_config_root/skills/wmcterm/SKILL.md"
do
    if ! cmp -s <("$wmcterm_binary" skill print) "$skill_file"; then
        echo "installed skill does not match $wmcterm_binary: $skill_file" >&2
        exit 1
    fi
done

python3 - \
    "$wmcterm_binary" \
    "$codex_config_root/hooks.json" \
    "$claude_config_root/settings.json" <<'PY'
import json
import pathlib
import subprocess
import sys

binary = sys.argv[1]

def commands(value):
    if isinstance(value, dict):
        command = value.get("command")
        if isinstance(command, str):
            args = value.get("args")
            yield (command, tuple(args) if isinstance(args, list) else None)
        for child in value.values():
            yield from commands(child)
    elif isinstance(value, list):
        for child in value:
            yield from commands(child)

def validate(agent, filename):
    path = pathlib.Path(filename)
    document = json.loads(path.read_text(encoding="utf-8"))
    plan = json.loads(subprocess.check_output(
        [binary, "hook", "plan", "--agent", agent],
        text=True,
    ))
    fragment = json.loads(plan["snippets"][0]["content"])
    expected_hooks = fragment.get("hooks", {})
    expected_receivers = set(commands(fragment))
    if not expected_receivers:
        raise SystemExit(f"hook plan for {agent} did not contain a receiver")

    installed_hooks = document.get("hooks", {})
    for event, expected_entries in expected_hooks.items():
        missing = set(commands(expected_entries)) - set(commands(installed_hooks.get(event, [])))
        if missing:
            raise SystemExit(
                f"missing current wmcterm receiver for {agent} event {event}: "
                + ", ".join(repr(item) for item in sorted(missing, key=repr))
            )

    stale = {
        command
        for command in commands(document)
        if is_receiver(command, agent)
        and command not in expected_receivers
    }
    if stale:
        raise SystemExit(
            f"stale {agent} wmcterm hook receivers remain: "
            + ", ".join(repr(item) for item in sorted(stale, key=repr))
        )

def is_receiver(receiver, agent):
    command, args = receiver
    if args is not None:
        executable = command.replace("\\", "/").rsplit("/", 1)[-1]
        return executable in ("wmcterm", "wmcterm.exe") and args[:2] == ("hook", "receive") and any(
            args[index:index + 2] == ("--agent", agent) for index in range(len(args) - 1)
        )
    return "wmcterm" in command and "hook receive" in command and f"--agent {agent}" in command

validate("codex", sys.argv[2])
validate("claude", sys.argv[3])
PY

echo "wmcterm Agent skills and Codex/Claude hooks are current for $HOME"
