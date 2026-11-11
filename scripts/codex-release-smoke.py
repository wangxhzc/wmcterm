#!/usr/bin/env python3
"""Exercise installed app-server cold restores without credentials or model turns."""

import argparse
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading


class AppServer:
    def __init__(self, program, root, *config):
        # No live wmcterm endpoint, user hooks, credentials, or model provider
        # participates in this test. Even an accidental model call has no route.
        env = {
            key: value
            for key, value in os.environ.items()
            if not any(marker in key.upper() for marker in ("TOKEN", "API_KEY", "AUTH", "WMCTERM"))
        }
        env["CODEX_HOME"] = str(root)
        env["OTEL_SDK_DISABLED"] = "true"
        args = [program]
        for override in (
            'model_provider="wmcterm_smoke"',
            'model_providers.wmcterm_smoke={name="wmcterm smoke",base_url="http://127.0.0.1:1",wire_api="responses"}',
            "check_for_update_on_startup=false",
            *config,
        ):
            args.extend(("-c", override))
        self.process = subprocess.Popen(
            [*args, "app-server", "--stdio"], cwd=root, env=env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True,
        )
        self.messages = queue.Queue()
        self.reader = threading.Thread(target=self.read_messages, daemon=True)
        self.reader.start()
        self.next_id = 0
        try:
            self.call("initialize", {
                "clientInfo": {"name": "wmcterm_release_smoke", "version": "1"},
                "capabilities": {"experimentalApi": True},
            })
            self.send({"method": "initialized"})
        except BaseException:
            self.close()
            raise

    def read_messages(self):
        try:
            for line in self.process.stdout:
                self.messages.put(json.loads(line))
        finally:
            self.messages.put(None)

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def call(self, method, params):
        assert method in ("initialize", "thread/start", "thread/resume", "thread/name/set"), method
        self.next_id += 1
        self.send({"id": self.next_id, "method": method, "params": params})
        while True:
            message = self.messages.get(timeout=30)
            assert message is not None, f"app-server exited during {method}"
            if message.get("id") != self.next_id:
                assert "id" not in message, f"unexpected server request: {message.get('method')}"
                continue
            assert "error" not in message, f"{method}: {message.get('error')}"
            return message["result"]

    def close(self):
        self.process.stdin.close()
        try:
            self.process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=5)
        self.reader.join(timeout=5)
        self.process.stdout.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


def check(result, sandbox, policy, reviewer, network=None):
    assert result["approvalPolicy"] == policy, result["approvalPolicy"]
    assert result["approvalsReviewer"] in (
        ("auto_review", "guardian_subagent") if reviewer == "auto_review" else (reviewer,)
    ), result["approvalsReviewer"]
    assert result["sandbox"]["type"] == sandbox, result["sandbox"]
    if network is not None:
        assert result["sandbox"].get("networkAccess", False) is network, result["sandbox"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex", default="codex")
    args = parser.parse_args()
    print(subprocess.check_output([args.codex, "--version"], text=True, timeout=10).strip())
    with tempfile.TemporaryDirectory(prefix="wmcterm-codex-release-") as directory:
        root = Path(directory)
        with AppServer(args.codex, root, "sandbox_workspace_write.network_access=true") as server:
            saved = server.call("thread/start", {
                "cwd": directory, "approvalPolicy": "never", "approvalsReviewer": "user",
                "sandbox": "workspace-write", "ephemeral": False,
            })
            check(saved, "workspaceWrite", "never", "user", network=True)
            thread = saved["thread"]["id"]
            server.call("thread/name/set", {"threadId": thread, "name": "wmcterm release regression"})
            assert Path(saved["thread"]["path"]).is_relative_to(root)
        # The second server must read the rollout from disk. Explicit managed
        # overrides must win over both saved state and conflicting server config.
        with AppServer(args.codex, root, 'approval_policy="never"', 'sandbox_mode="danger-full-access"') as server:
            restored = server.call("thread/resume", {
                "threadId": thread, "excludeTurns": True, "approvalPolicy": "on-request",
                "approvalsReviewer": "user", "sandbox": "read-only",
            })
            check(restored, "readOnly", "on-request", "user", network=False)
            assert restored["thread"]["id"] == thread
            assert restored["cwd"] == directory
        for reviewer in ("user", "auto_review"):
            with AppServer(args.codex, root) as server:
                restored = server.call("thread/resume", {
                    "threadId": thread, "excludeTurns": True, "approvalPolicy": "on-request",
                    "approvalsReviewer": reviewer, "sandbox": "workspace-write",
                })
                check(restored, "workspaceWrite", "on-request", reviewer, network=False)
        with AppServer(args.codex, root) as server:
            restored = server.call("thread/resume", {
                "threadId": thread, "excludeTurns": True, "approvalPolicy": "never",
                "approvalsReviewer": "user", "sandbox": "danger-full-access",
            })
            check(restored, "dangerFullAccess", "never", "user")
    print("PASS installed Codex cold restore, explicit launch policies, reviewer and network sandbox")
    print("No model turns were started; temporary CODEX_HOME was removed")


if __name__ == "__main__":
    main()
