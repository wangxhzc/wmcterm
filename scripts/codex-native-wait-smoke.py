#!/usr/bin/env python3
"""Native Codex permission -> PostToolUse -> GUI, using only a loopback model.

Run under xvfb-run -a. All homes, Hooks, sockets and processes are disposable.
"""

import json
import os
from pathlib import Path
import runpy
import shlex
import shutil
import subprocess
import sys
import tempfile
import threading


COMMON = runpy.run_path(str(Path(__file__).with_name("agent-live-contract-smoke.py")))
wait_until = COMMON["wait_until"]


class Fixture(COMMON["ModelFixture"]):
    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["Content-Length"])))
        assert self.path.endswith("/responses"), self.path
        title_request = any("Generate a concise, single-line task title" in item.get("text", "")
            for message in body.get("input", []) if isinstance(message, dict)
            for item in message.get("content", []) if isinstance(item, dict))
        if title_request:
            events = [
                {"type": "response.created", "response": {"id": "title"}},
                {"type": "response.output_item.done", "item": {"type": "message", "role": "assistant", "id": "title",
                    "content": [{"type": "output_text", "text": "Run fixture"}]}},
                {"type": "response.completed", "response": {"id": "title", "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}},
            ]
            self.reply("text/event-stream", "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events))
            return
        self.server.requests.append(body)
        index = len(self.server.requests)
        events = [{"type": "response.created", "response": {"id": f"native-{index}"}}]
        if index == 1:
            names = {tool["name"] for tool in body["tools"] if tool.get("type") == "function"}
            name = "exec_command" if "exec_command" in names else "shell_command"
            assert name in names, names
            arguments = {"cmd" if name == "exec_command" else "command": "printf native-wait-fixture",
                "sandbox_permissions": "require_escalated", "justification": "Disposable local fixture"}
            events.append({"type": "response.output_item.done", "item": {
                "type": "function_call", "call_id": "native-command", "name": name,
                "arguments": json.dumps(arguments)}})
        else:
            self.server.model_waiting.set()
            assert self.server.finish.wait(60), "test did not release local model"
            events.append({"type": "response.output_item.done", "item": {
                "type": "message", "role": "assistant", "id": "done", "content": [
                    {"type": "output_text", "text": "Native fixture complete."}]}})
        events.append({"type": "response.completed", "response": {"id": f"native-{index}",
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}})
        self.reply("text/event-stream", "".join(
            f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events))


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/wmcterm").resolve())
    decision = sys.argv[2] if len(sys.argv) > 2 else "allow"
    assert decision in ("allow", "deny", "cancel"), decision
    with tempfile.TemporaryDirectory(prefix="wmcterm-native-wait-") as directory:
        root = Path(directory)
        for name in ("runtime", "config", "data", "workspace", "codex", "home"):
            (root / name).mkdir(mode=0o700)
        env = {key: value for key, value in os.environ.items()
            if not key.startswith(("WMCTERM", "CODEX", "OPENAI", "ANTHROPIC"))
            and (key == "XAUTHORITY" or not any(marker in key.upper() for marker in ("TOKEN", "API_KEY", "AUTH")))}
        env.update(HOME=str(root / "home"), CODEX_HOME=str(root / "codex"),
            XDG_RUNTIME_DIR=str(root / "runtime"), XDG_CONFIG_HOME=str(root / "config"),
            XDG_DATA_HOME=str(root / "data"), WGPU_BACKEND="gl", OTEL_SDK_DISABLED="true")
        server = COMMON["ThreadingHTTPServer"](("127.0.0.1", 0), Fixture)
        server.requests = []
        server.model_waiting = threading.Event()
        server.finish = threading.Event()
        threading.Thread(target=server.serve_forever, daemon=True).start()
        (root / "codex/config.toml").write_text(f'''model = "mock-model"
model_provider = "local"
check_for_update_on_startup = false
[projects.{json.dumps(str(root / "workspace"))}]
trust_level = "trusted"
[features]
code_mode = false
[model_providers.local]
name = "Local fixture"
base_url = "http://127.0.0.1:{server.server_port}/v1"
wire_api = "responses"
requires_openai_auth = false
request_max_retries = 0
stream_max_retries = 0
''')
        plan = json.loads(subprocess.check_output([binary, "hook", "plan", "--agent", "codex"], env=env))
        hooks = json.loads(plan["snippets"][0]["content"])
        for event in ("PreToolUse", "PostToolUse", "PreModelRequest"):
            receiver = hooks["hooks"][event][0]["hooks"][0]
            receiver["command"] = f"tee {shlex.quote(str(root / (event + '.json')))} | {receiver['command']}"
        shim = root / "permission.py"
        shim.write_text('''import json, pathlib, subprocess, sys, time
payload = sys.stdin.read()
subprocess.run([sys.argv[1], "hook", "receive", "--agent", "codex", "--event", "PermissionRequest"], input=payload, text=True, stdout=subprocess.DEVNULL, check=True)
root = pathlib.Path(sys.argv[2])
(root / "permission.json").write_text(payload)
if sys.argv[3] == "cancel":
    print("{}")
    sys.exit(0)
deadline = time.monotonic() + 30
while not (root / "release").exists():
    if time.monotonic() > deadline: raise TimeoutError("permission fixture")
    time.sleep(0.05)
print(json.dumps({"hookSpecificOutput": {"hookEventName": "PermissionRequest", "decision": {"behavior": sys.argv[3]}}}))
''')
        hooks["hooks"]["PermissionRequest"] = [{"hooks": [{"type": "command",
            "command": shlex.join([sys.executable, str(shim), binary, str(root), decision]), "timeout": 40}]}]
        (root / "codex/hooks.json").write_text(json.dumps(hooks))

        def rpc(method, params=None):
            return json.loads(subprocess.check_output([binary, "rpc", method, json.dumps(params or {})],
                env=env, stderr=subprocess.DEVNULL, timeout=5))

        with (root / "gui.log").open("w+") as log, (root / "codex.log").open("w+") as cli_log:
            app = subprocess.Popen([binary, "gui"], cwd=root / "workspace", env=env, stdout=log, stderr=log)
            cli = None
            try:
                def started():
                    try:
                        return rpc("workspace.list")[0]["id"]
                    except (subprocess.CalledProcessError, IndexError):
                        return None
                workspace = wait_until(started)
                surface = rpc("surface.new", {"workspace": workspace, "focus": False,
                    "command": shlex.join([shutil.which("codex"), "--ask-for-approval", "on-request",
                        "--sandbox", "workspace-write", "--no-alt-screen", "Run the disposable local fixture."])})["surface_id"]
                # Trust only this test's generated Hooks in its disposable home.
                wait_until(lambda: "Trust all and continue" in rpc("screen.read", {"surface": surface})["text"])
                for key in ("Down", "Enter"):
                    subprocess.run([binary, "send-key", key, "--surface", surface], env=env, check=True, stdout=subprocess.DEVNULL)
                wait_until(lambda: (root / "permission.json").exists())
                def state():
                    return next(item for item in rpc("surface.list", {"workspace": workspace})
                        if item["id"] == surface)["agent"]["state"]
                wait_until(lambda: state() == "waiting_for_permission")
                assert not rpc("agent.request.list"), "native telemetry must not create approval buttons"
                if decision == "cancel":
                    wait_until(lambda: "Would you like" in rpc("screen.read", {"surface": surface})["text"])
                    subprocess.run([binary, "send-key", "Ctrl+C", "--surface", surface], env=env, check=True, stdout=subprocess.DEVNULL)
                    wait_until(lambda: state() == "idle")
                    terminal = next(item for item in rpc("app.diagnostics")["terminals"] if item["surface_id"] == surface)
                    assert terminal["agent_session"]["lifecycle"]["phase"] == "running"
                    assert not rpc("agent.request.list")
                    print("PASS real native Codex cancellation: permission -> Interrupt -> idle, session alive")
                    return
                (root / "release").touch()
                wait_until(lambda: server.model_waiting.is_set())
                wait_until(lambda: state() == "running", seconds=5)
                assert rpc("workspace.list")[0]["status"] == "running"
                assert not rpc("agent.request.list")
                outputs = [item for item in server.requests[1]["input"] if item.get("type") == "function_call_output"]
                assert outputs, [item.get("type") for item in server.requests[1]["input"]]
                output = str(outputs[-1]["output"])
                assert ("native-wait-fixture" in output) if decision == "allow" else ("rejected" in output.lower() or "denied" in output.lower()), output
                server.finish.set()
                wait_until(lambda: state() == "completed")
                print(f"PASS real native Codex {decision}: permission -> tool/model boundary -> running -> completed")
            except BaseException:
                log.flush()
                cli_log.flush()
                print((root / "gui.log").read_text()[-5000:])
                print((root / "codex.log").read_text()[-10000:])
                if "surface" in locals():
                    print(rpc("screen.read", {"surface": surface})["text"])
                    print(next(item for item in rpc("app.diagnostics")["terminals"] if item["surface_id"] == surface))
                for event in ("permission", "PreToolUse", "PostToolUse", "PreModelRequest"):
                    path = root / (event + ".json")
                    if path.exists():
                        print(event, path.read_text())
                raise
            finally:
                server.finish.set()
                for process in (cli, app):
                    if process is not None and process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait(timeout=5)
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    main()
