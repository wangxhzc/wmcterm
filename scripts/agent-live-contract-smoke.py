#!/usr/bin/env python3
"""Real installed CLIs, local model fixtures, isolated configuration; no paid API calls.

Run with xvfb-run -a. Codex traverses the managed watcher and GUI. Claude's
actual permission engine exercises hook failure policies against a local API.
"""

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parent.parent


def wait_until(predicate, seconds=40):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.1)
    raise TimeoutError("local Agent contract smoke")


class ModelFixture(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        self.server.requests.append((self.path, body))
        if self.path.endswith("/responses"):
            calls = sum(path.endswith("/responses") for path, _ in self.server.requests)
            events = [{"type": "response.created", "response": {"id": f"resp-{calls}"}}]
            if calls == 1:
                events.append({"type": "response.output_item.done", "item": {
                    "type": "function_call", "call_id": "ask-target", "name": "request_user_input",
                    "arguments": json.dumps({"questions": [{"id": "target", "header": "Target",
                        "question": "Choose a target", "options": [
                            {"label": "test", "description": "Test only"},
                            {"label": "production", "description": "Production"}]}]}),
                }})
            else:
                events.append({"type": "response.output_item.done", "item": {
                    "type": "message", "role": "assistant", "id": "done", "content": [
                        {"type": "output_text", "text": "Completed local input test."}]}})
            events.append({"type": "response.completed", "response": {"id": f"resp-{calls}",
                "usage": {"input_tokens": 0, "output_tokens": 0, "total_tokens": 0}}})
            self.reply("text/event-stream", "".join(
                f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events))
            return
        if "count_tokens" in self.path:
            self.reply("application/json", json.dumps({"input_tokens": 1}))
            return
        # Anthropic Messages API, deterministic Bash tool followed by final text.
        tool_seen = any(item.get("type") == "tool_result" for message in body.get("messages", [])
                        for item in message.get("content", []) if isinstance(item, dict))
        content = [{"type": "text", "text": "Local test complete."}] if tool_seen else [{
            "type": "tool_use", "id": "tool-local", "name": "Bash",
            "input": {"command": f"touch {shlex.quote(str(self.server.marker))}", "description": "Local permission fixture"},
        }]
        message = {"id": "msg_local", "type": "message", "role": "assistant",
            "model": "claude-sonnet-4-6", "content": content,
            "stop_reason": "end_turn" if tool_seen else "tool_use", "stop_sequence": None,
            "usage": {"input_tokens": 1, "output_tokens": 1}}
        if not body.get("stream"):
            self.reply("application/json", json.dumps(message))
            return
        start = dict(message, content=[], stop_reason=None)
        events = [{"type": "message_start", "message": start}]
        for index, item in enumerate(content):
            block = dict(item)
            if item["type"] == "tool_use":
                block["input"] = {}
                delta = {"type": "input_json_delta", "partial_json": json.dumps(item["input"])}
            else:
                block["text"] = ""
                delta = {"type": "text_delta", "text": item["text"]}
            events.extend([
                {"type": "content_block_start", "index": index, "content_block": block},
                {"type": "content_block_delta", "index": index, "delta": delta},
                {"type": "content_block_stop", "index": index},
            ])
        events.extend([{"type": "message_delta", "delta": {"stop_reason": message["stop_reason"],
            "stop_sequence": None}, "usage": {"output_tokens": 1}}, {"type": "message_stop"}])
        self.reply("text/event-stream", "".join(
            f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events))

    def reply(self, content_type, text):
        data = text.encode()
        self.send_response(200)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def codex_roundtrip(binary, folder, env, server):
    codex_home = folder / "codex"
    codex_home.mkdir()
    (codex_home / "config.toml").write_text(f'''model = "mock-model"
model_provider = "local-fixture"
[model_providers.local-fixture]
name = "Local test fixture"
base_url = "http://127.0.0.1:{server.server_port}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
requires_openai_auth = false
''')
    results = folder / "codex-results.jsonl"
    env = dict(env, CODEX_HOME=str(codex_home), WMCTERM_TEST_INPUT_ROUNDTRIP="1",
        WMCTERM_TEST_REAL_CODEX=shutil.which("codex"), WMCTERM_TEST_RESULTS=str(results),
        WMCTERM_CODEX_PATH=str(ROOT / "scripts/fixtures/codex-managed-smoke"))

    def rpc(method, params=None):
        return json.loads(subprocess.check_output([str(binary), "rpc", method, json.dumps(params or {})],
            env=env, stderr=subprocess.DEVNULL, timeout=5))

    log = open(folder / "gui.log", "w+")
    app = subprocess.Popen([str(binary), "gui"], cwd=folder / "workspace", env=env, stdout=log, stderr=log)
    try:
        def started():
            try:
                return rpc("pane.list")[0]["id"]
            except (subprocess.CalledProcessError, IndexError):
                return None
        pane = wait_until(started)
        rpc("surface.new", {"pane": pane, "focus": True,
            "command": f"{shlex.quote(str(binary))} agent codex"})
        request = wait_until(lambda: next((item for item in rpc("agent.request.list") if item["kind"] == "input"), None))
        window = subprocess.check_output(["xdotool", "search", "--pid", str(app.pid), "--name", "^wmcterm$"], text=True).splitlines()[0]
        subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True)
        time.sleep(0.3)
        subprocess.run(["xdotool", "key", "--clearmodifiers", "Right", "Return"], check=True)
        time.sleep(0.2)
        subprocess.run(["xdotool", "key", "--clearmodifiers", "Down", "Return"], check=True)
        wait_until(lambda: results.exists() and results.read_text().strip())
        records = [json.loads(line) for line in results.read_text().splitlines()]
        assert records[0]["completed"], records
        requests = [body for path, body in server.requests if path.endswith("/responses")]
        assert len(requests) == 2, requests
        outputs = [item for item in requests[1]["input"] if item.get("type") == "function_call_output"]
        assert any(item.get("call_id") == "ask-target" and "test" in item["output"] for item in outputs), outputs
        assert rpc("agent.input.poll", {"request_id": request["request_id"]})["status"] != "pending"
        print("PASS real Codex app-server -> watcher -> GUI -> upstream tool output -> turn completed")
    except Exception:
        log.flush()
        print((folder / "gui.log").read_text()[-12000:])
        raise
    finally:
        app.terminate()
        try:
            app.wait(timeout=5)
        except subprocess.TimeoutExpired:
            app.kill()
            app.wait()
        log.close()


def claude_failures(binary, folder, env, server):
    plan = json.loads(subprocess.check_output([str(binary), "hook", "plan", "--agent", "claude"], env=env, text=True))
    template = json.loads(plan["snippets"][0]["content"])["hooks"]["PermissionRequest"][0]["hooks"][0]
    assert template["onFailure"] == "block"
    home = folder / "claude-home"
    home.mkdir()
    # Trust only this disposable project. No real credentials or personal settings.
    (home / ".claude.json").write_text(json.dumps({"hasCompletedOnboarding": True,
        "projects": {str(folder / "workspace"): {"hasTrustDialogAccepted": True}}}))
    env = {key: value for key, value in env.items()
        if not key.startswith(("CLAUDE", "ANTHROPIC"))}
    env.update(HOME=str(home), CLAUDE_CONFIG_DIR=str(home), ANTHROPIC_API_KEY="local-fixture-only",
        ANTHROPIC_BASE_URL=f"http://127.0.0.1:{server.server_port}",
        CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1", DISABLE_AUTOUPDATER="1")
    allow = json.dumps({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": "allow"}}})
    cases = [("allow", f"printf '%s' {shlex.quote(allow)}", 5),
        ("crash", "exit 1", 5), ("missing", "/wmcterm-test-does-not-exist", 5),
        ("timeout", "sleep 3", 1), ("invalid-json", "printf '{broken'", 5)]
    for name, command, timeout in cases:
        marker = folder / f"claude-executed-{name}"
        observed = folder / f"claude-hook-{name}"
        server.marker = marker
        settings = {"hooks": {"PermissionRequest": [{"matcher": "Bash", "hooks": [{
            "type": "command", "command": f"touch {shlex.quote(str(observed))}; {command}",
            "timeout": timeout, "onFailure": template["onFailure"]}]}]}}
        result = subprocess.run(["claude", "-p", "Run the local permission fixture", "--model", "claude-sonnet-4-6",
            "--settings", json.dumps(settings), "--setting-sources", "", "--permission-mode", "default", "--strict-mcp-config",
            "--mcp-config", '{"mcpServers":{}}', "--tools", "Bash", "--max-turns", "2",
            "--output-format", "stream-json", "--verbose", "--include-hook-events"],
            env=env, cwd=folder / "workspace", capture_output=True, text=True, timeout=40)
        assert observed.exists(), (name, result.stdout[-5000:], result.stderr[-2000:])
        assert marker.exists() == (name == "allow"), (name, result.stdout[-5000:])
        if name != "allow":
            assert '"is_error":true' in result.stdout or '"is_error": true' in result.stdout, (name, result.stdout[-5000:])
        print(f"PASS real Claude PermissionRequest {name}: tool {'executed' if marker.exists() else 'blocked'}")


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target/debug/wmcterm").resolve()
    with tempfile.TemporaryDirectory(prefix="wmcterm-agent-live-") as directory:
        folder = Path(directory)
        for name in ("runtime", "config", "data", "workspace"):
            (folder / name).mkdir(mode=0o700)
        env = {key: value for key, value in os.environ.items() if not key.startswith("WMCTERM_")}
        env.update(XDG_RUNTIME_DIR=str(folder / "runtime"), XDG_CONFIG_HOME=str(folder / "config"),
            XDG_DATA_HOME=str(folder / "data"), WGPU_BACKEND="gl", WMCTERM_UI_LANGUAGE="en")
        server = ThreadingHTTPServer(("127.0.0.1", 0), ModelFixture)
        server.requests = []
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            codex_roundtrip(binary, folder, env, server)
            claude_failures(binary, folder, env, server)
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == "__main__":
    main()
