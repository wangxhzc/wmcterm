#!/usr/bin/env python3
"""Run under xvfb-run: real server + fixture TUI, isolated GUI restart, no turns."""

import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import tempfile
import time


def main():
    root = Path(__file__).resolve().parent.parent
    binary = root / "target/debug/wmcterm"
    real_codex = shutil.which("codex")
    with tempfile.TemporaryDirectory(prefix="wmcterm-codex-restore-") as directory:
        folder = Path(directory)
        for name in ("runtime", "config", "data", "workspace", "codex"):
            (folder / name).mkdir(mode=0o700)
        results = folder / "results.jsonl"
        environment = {key: value for key, value in os.environ.items() if not key.startswith("WMCTERM_")}
        environment.update({
            "XDG_RUNTIME_DIR": str(folder / "runtime"), "XDG_CONFIG_HOME": str(folder / "config"),
            "XDG_DATA_HOME": str(folder / "data"), "CODEX_HOME": str(folder / "codex"),
            "WGPU_BACKEND": "gl", "WMCTERM_UI_LANGUAGE": "en",
            "WMCTERM_CODEX_PATH": str(root / "scripts/fixtures/codex-managed-smoke"),
            "WMCTERM_TEST_REAL_CODEX": real_codex, "WMCTERM_TEST_RESULTS": str(results),
        })

        def rpc(method, params=None):
            output = subprocess.check_output([str(binary), "rpc", method, json.dumps(params or {})], env=environment, stderr=subprocess.DEVNULL)
            return json.loads(output)

        def wait_until(predicate, seconds=30):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                try:
                    value = predicate()
                    if value:
                        return value
                except (OSError, subprocess.CalledProcessError, json.JSONDecodeError):
                    pass
                time.sleep(0.1)
            raise TimeoutError("isolated Codex restore smoke")

        def records():
            return [json.loads(line) for line in results.read_text().splitlines()] if results.exists() else []

        app = None
        log = open(folder / "gui.log", "w+")

        def start():
            nonlocal app
            app = subprocess.Popen([str(binary), "gui"], cwd=folder / "workspace", env=environment, stdout=log, stderr=log)
            wait_until(lambda: rpc("app.status"))
            return wait_until(lambda: subprocess.check_output(["xdotool", "search", "--pid", str(app.pid), "--name", "wmcterm"], env=environment).decode().splitlines()[0])

        def close(window):
            subprocess.run(["xdotool", "mousemove", "--window", window, "980", "20", "click", "1"], check=True, env=environment)
            try:
                app.wait(timeout=1)
                return
            except subprocess.TimeoutExpired:
                # Confirm closing only this isolated test application's processes.
                subprocess.run(["xdotool", "mousemove", "--window", window, "690", "376", "click", "1"], check=True, env=environment)
            app.wait(timeout=15)

        try:
            window = start()
            pane = rpc("pane.list")[0]["id"]
            created = rpc("surface.new", {"pane": pane, "focus": False, "command": f"{shlex.quote(str(binary))} agent --mode unattended codex"})
            wait_until(lambda: len(records()) == 1)
            time.sleep(0.5)
            close(window)
            state_path = next((folder / "data").rglob("state.json"))
            saved = json.loads(state_path.read_text())
            terminal = next(item for item in saved["terminals"] if item.get("managed_codex"))
            thread = terminal["managed_codex"]["session_id"]
            assert terminal["managed_codex"]["mode"] == "unattended"
            assert records()[0]["reviewer"] == "auto_review"

            for index, (mode, policy, reviewer, sandbox) in enumerate([
                ("unattended", "on-request", "auto_review", "workspaceWrite"),
                ("interactive", "on-request", "user", "workspaceWrite"),
                ("full-access", "never", "user", "dangerFullAccess"),
                ("full-access", "never", "user", "dangerFullAccess"),
            ], start=2):
                # Simulate a persisted GUI choice, including the prior full-access confirmation.
                saved = json.loads(state_path.read_text())
                terminal = next(item for item in saved["terminals"] if item.get("managed_codex"))
                if terminal["managed_codex"]["mode"] != mode:
                    terminal["managed_codex"]["mode"] = mode
                    terminal["managed_codex"].pop("permissions", None)
                state_path.write_text(json.dumps(saved))
                window = start()
                wait_until(lambda: len(records()) == index)
                actual = records()[-1]
                assert actual == {"threadId": thread, "policy": policy, "reviewer": reviewer, "sandbox": sandbox, "resumed": True}, actual
                surfaces = rpc("surface.list", {"workspace": created["workspace_id"]})
                surface = next(item for item in surfaces if item["id"] == created["surface_id"])
                assert surface["agent"]["run_mode"] == mode, surface
                time.sleep(0.5)
                close(window)
                saved = json.loads(state_path.read_text())
                terminal = next(item for item in saved["terminals"] if item.get("managed_codex"))
                assert terminal["managed_codex"]["mode"] == mode
                assert terminal["managed_codex"]["session_id"] == thread
                print(f"PASS {mode}: GUI restart, same thread, /resume, mode label and save")
        except Exception:
            log.flush()
            print((folder / "gui.log").read_text())
            raise
        finally:
            if app and app.poll() is None:
                app.terminate()
                try:
                    app.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait()
            log.close()


if __name__ == "__main__":
    main()
