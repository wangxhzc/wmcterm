#!/usr/bin/env python3
"""Check crash recovery in an isolated display and XDG profile."""

import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time


def main():
    binary = Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/wmcterm").resolve()
    with tempfile.TemporaryDirectory(prefix="wmcterm-checkpoint-") as directory:
        root = Path(directory)
        environment = {key: value for key, value in os.environ.items() if not key.startswith("WMCTERM")}
        for variable, name in [("XDG_RUNTIME_DIR", "runtime"), ("XDG_CONFIG_HOME", "config"), ("XDG_DATA_HOME", "data")]:
            path = root / name
            path.mkdir(mode=0o700)
            environment[variable] = str(path)
        environment["WGPU_BACKEND"] = "gl"
        workspace = root / "workspace"
        workspace.mkdir()

        def rpc(method, params=None):
            result = subprocess.run([str(binary), "rpc", method, json.dumps(params or {})], env=environment, cwd=workspace, capture_output=True, text=True, timeout=7, check=True)
            return json.loads(result.stdout)

        def wait_for(check):
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                try:
                    result = check()
                    if result:
                        return result
                except (OSError, ValueError, subprocess.SubprocessError):
                    pass
                time.sleep(0.1)
            raise AssertionError("checkpoint smoke timed out")

        def stop(process):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)

        with (root / "gui.log").open("w") as log:
            process = subprocess.Popen([str(binary), "gui"], env=environment, cwd=workspace, stdout=log, stderr=log, start_new_session=True)
            try:
                status = wait_for(lambda: rpc("app.status"))
                rpc("workspace.rename", {"id": status["active_workspace"], "name": "checkpoint-recovered"})

                def checkpoint():
                    for path in (root / "data").rglob("state.json"):
                        document = json.loads(path.read_text())
                        if document["app"]["workspaces"][0]["name"] == "checkpoint-recovered":
                            return document
                    return None

                wait_for(checkpoint)
                stop(process)
                process = subprocess.Popen([str(binary), "gui"], env=environment, cwd=workspace, stdout=log, stderr=log, start_new_session=True)
                wait_for(lambda: rpc("app.status"))
                restored = rpc("workspace.list")
                assert restored[0]["name"] == "checkpoint-recovered", restored
            except Exception:
                log.flush()
                print((root / "gui.log").read_text(), file=sys.stderr)
                raise
            finally:
                stop(process)
    print("Session checkpoint crash recovery passed")


if __name__ == "__main__":
    main()
