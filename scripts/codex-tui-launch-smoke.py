#!/usr/bin/env python3
"""Real Codex TUI launched from an isolated command palette; no model turns."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/wmcterm").resolve())
    codex = shutil.which("codex")
    assert codex, "codex must be installed"
    with tempfile.TemporaryDirectory(prefix="wmcterm-real-codex-") as directory:
        root = Path(directory)
        env = {key: value for key, value in os.environ.items() if not key.startswith("WMCTERM_")}
        for name in ("runtime", "config", "data", "workspace", "codex"):
            (root / name).mkdir(mode=0o700)
        env.update(XDG_RUNTIME_DIR=str(root / "runtime"), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_DATA_HOME=str(root / "data"), CODEX_HOME=str(root / "codex"),
                   WMCTERM_CODEX_PATH=codex, WMCTERM_UI_LANGUAGE="en", WGPU_BACKEND="gl")

        def rpc(method, params=None):
            return json.loads(subprocess.check_output(
                [binary, "rpc", method, json.dumps(params or {})], env=env,
                stderr=subprocess.DEVNULL, timeout=5))

        def wait(check):
            for _ in range(200):
                try:
                    value = check()
                    if value:
                        return value
                except (subprocess.SubprocessError, OSError):
                    pass
                time.sleep(0.1)
            raise AssertionError("timed out waiting for real Codex TUI")

        def xdo(*args):
            return subprocess.check_output(["xdotool", *args], env=env, text=True).strip()

        with (root / "gui.log").open("wb") as log:
            app = subprocess.Popen([binary, "gui"], cwd=root / "workspace", env=env, stdout=log, stderr=log)
            screen = ""
            try:
                workspace = wait(lambda: rpc("workspace.list"))[0]["id"]
                initial = {surface["id"] for surface in rpc("surface.list", {"workspace": workspace})}
                window = wait(lambda: xdo("search", "--pid", str(app.pid))).splitlines()[0]
                time.sleep(1)
                xdo("windowfocus", "--sync", window)
                xdo("mousemove", "--window", window, "400", "20")
                xdo("click", "1")
                time.sleep(0.3)
                xdo("type", "New Codex")
                time.sleep(0.3)
                xdo("key", "Return")
                target = wait(lambda: next((surface["id"] for surface in rpc("surface.list", {"workspace": workspace})
                                           if surface["id"] not in initial), None))

                def ready():
                    nonlocal screen
                    screen = rpc("screen.read", {"surface": target})["text"]
                    assert "invalid remote address" not in screen, screen
                    assert "exited with status" not in screen, screen
                    return "OpenAI Codex" in screen or "Welcome to Codex" in screen

                wait(ready)
                time.sleep(1)
                assert ready(), screen
                print("PASS real Codex: command palette launch, remote address accepted, authenticated TUI connected")
            except Exception:
                print(screen)
                print(rpc("surface.list", {"workspace": workspace}))
                subprocess.run(["import", "-window", window, "/tmp/wmcterm-codex-launch-failure.png"], env=env, check=False)
                print((root / "gui.log").read_text())
                raise
            finally:
                app.terminate()
                try:
                    app.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait(timeout=5)


if __name__ == "__main__":
    main()
