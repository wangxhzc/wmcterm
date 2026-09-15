#!/usr/bin/env python3
"""Check installed Codex permission round trips without starting model turns."""

import json
import os
import selectors
import subprocess
import tempfile
import time


def main():
    with tempfile.TemporaryDirectory(prefix="wmcterm-codex-permissions-") as directory:
        environment = dict(os.environ, CODEX_HOME=directory)
        with subprocess.Popen(
            ["codex", "app-server", "--listen", "stdio://"],
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        ) as server:
            selector = selectors.DefaultSelector()
            selector.register(server.stdout, selectors.EVENT_READ)
            buffer = b""
            request_id = 0

            def send(value):
                server.stdin.write(json.dumps(value).encode() + b"\n")
                server.stdin.flush()

            def request(method, params):
                nonlocal request_id, buffer
                request_id += 1
                send({"id": request_id, "method": method, "params": params})
                deadline = time.monotonic() + 30
                while time.monotonic() < deadline:
                    if b"\n" not in buffer:
                        if not selector.select(1):
                            continue
                        chunk = os.read(server.stdout.fileno(), 65536)
                        if not chunk:
                            raise RuntimeError("Codex app-server exited")
                        buffer += chunk
                    while b"\n" in buffer:
                        line, buffer = buffer.split(b"\n", 1)
                        response = json.loads(line)
                        if response.get("id") == request_id:
                            if "error" in response:
                                raise RuntimeError(response["error"])
                            return response["result"]
                raise TimeoutError(method)

            try:
                request("initialize", {
                    "clientInfo": {"name": "wmcterm_permission_smoke", "version": "1"},
                    "capabilities": {"experimentalApi": True},
                })
                send({"method": "initialized"})
                modes = [
                    ("interactive", "on-request", "user", "workspace-write", "workspaceWrite"),
                    ("unattended", "on-request", "auto_review", "workspace-write", "workspaceWrite"),
                    ("full-access", "never", "user", "danger-full-access", "dangerFullAccess"),
                ]
                for name, policy, reviewer, sandbox, wire_sandbox in modes:
                    permissions = {"approvalPolicy": policy, "approvalsReviewer": reviewer, "sandbox": sandbox}
                    started = request("thread/start", dict(permissions, cwd=directory))
                    thread_id = started["thread"]["id"]
                    request("thread/inject_items", {
                        "threadId": thread_id,
                        "items": [{"type": "message", "role": "user", "content": [
                            {"type": "input_text", "text": "Permission smoke fixture; do not execute a turn."}
                        ]}],
                    })
                    resumed = request("thread/resume", dict(permissions, threadId=thread_id))
                    for result in (started, resumed):
                        assert result["approvalPolicy"] == policy, result["approvalPolicy"]
                        assert result["approvalsReviewer"] == reviewer, result["approvalsReviewer"]
                        assert result["sandbox"]["type"] == wire_sandbox, result["sandbox"]
                    print(f"PASS {name}: thread/start + thread/resume verified")
            finally:
                selector.close()
                server.terminate()
                try:
                    server.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    server.kill()
                    server.wait()


if __name__ == "__main__":
    main()
