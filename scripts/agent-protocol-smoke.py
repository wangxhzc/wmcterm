#!/usr/bin/env python3
"""Check installed release contracts without credentials or model requests."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile


def run(*args):
    return subprocess.check_output(args, text=True, timeout=60)


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/wmcterm").resolve())
    print(run("codex", "--version").strip())
    print(run("claude", "--version").strip())
    with tempfile.TemporaryDirectory(prefix="wmcterm-agent-schema-") as directory:
        run("codex", "app-server", "generate-json-schema", "--experimental", "--out", directory)
        root = Path(directory)
        response = json.loads((root / "PermissionsRequestApprovalResponse.json").read_text())
        assert "strictAutoReview" in response["properties"], "Codex lacks strict review responses"
        assert set(response["definitions"]["PermissionGrantScope"]["enum"]) == {"turn", "session"}
        requests = (root / "ServerRequest.json").read_text()
        for method in ("item/tool/requestUserInput", "mcpServer/elicitation/request", "item/permissions/requestApproval"):
            assert method in requests, method
        for filename, fields in (
            ("ToolRequestUserInputResponse.json", ("answers",)),
            ("McpServerElicitationRequestResponse.json", ("action", "content", "_meta")),
        ):
            schema = json.loads((root / filename).read_text())
            assert all(field in schema["properties"] for field in fields), filename
    for agent in ("codex", "claude"):
        plan = json.loads(run(binary, "hook", "plan", "--agent", agent))
        hooks = json.loads(plan["snippets"][0]["content"])["hooks"]
        assert "UserPromptSubmit" in hooks
        permission = hooks["PermissionRequest"][0]["hooks"][0]
        if agent == "claude":
            assert permission["onFailure"] == "block"
        else:
            assert "onFailure" not in permission
    print("PASS installed Codex schemas and generated Claude/Codex Hook contracts")


if __name__ == "__main__":
    main()
