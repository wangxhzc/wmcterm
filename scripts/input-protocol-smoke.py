#!/usr/bin/env python3
"""Isolated GUI to PTY input regression; run under xvfb-run -a."""
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time


def wait_for(check):
    for _ in range(100):
        try:
            result = check()
            if result:
                return result
        except (subprocess.SubprocessError, FileNotFoundError):
            pass
        time.sleep(0.05)
    raise AssertionError("timed out waiting for isolated input fixture")


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/release/wmcterm").resolve())
    with tempfile.TemporaryDirectory(prefix="wmcterm-input-") as directory:
        root = Path(directory)
        env = {key: value for key, value in os.environ.items() if not key.startswith("WMCTERM_")}
        for name in ("runtime", "config", "data", "workspace"):
            (root / name).mkdir(mode=0o700)
        env.update(XDG_RUNTIME_DIR=str(root / "runtime"), XDG_CONFIG_HOME=str(root / "config"), XDG_DATA_HOME=str(root / "data"), WGPU_BACKEND="gl")
        received = root / "input.bin"
        reader = """import os, sys, tty
tty.setraw(0)
os.write(1, b'line\\r\\n' * 200 + b'\\x1b[?1003h\\x1b[?1006h\\x1b[?1004h\\x1b[>7uREADY')
with open(sys.argv[1], 'ab', buffering=0) as output:
    while True:
        output.write(os.read(0, 4096))
"""
        def rpc(method, params=None):
            return json.loads(subprocess.check_output([binary, "rpc", method, json.dumps(params or {})], env=env, stderr=subprocess.DEVNULL, timeout=5))

        def xdo(*args):
            return subprocess.check_output(["xdotool", *args], env=env, stderr=subprocess.DEVNULL, text=True).strip()

        def data():
            return received.read_bytes() if received.exists() else b""

        with (root / "gui.log").open("wb") as log:
            app = subprocess.Popen([binary, "gui"], env=env, cwd=root / "workspace", stdout=log, stderr=log)
            try:
                workspace = wait_for(lambda: rpc("workspace.list"))[0]
                window = wait_for(lambda: xdo("search", "--pid", str(app.pid))).splitlines()[0]
                xdo("windowsize", window, "1000", "700")
                xdo("windowfocus", "--sync", window)
                initial = rpc("surface.list", {"workspace": workspace["id"]})[0]["id"]
                target = rpc("surface.new", {"workspace": workspace["id"], "command": shlex.join([sys.executable, "-u", "-c", reader, str(received)])})["surface_id"]
                wait_for(lambda: "READY" in rpc("screen.read", {"surface": target})["text"])
                time.sleep(0.3)
                xdo("mousemove", "--window", window, "600", "300")
                wait_for(lambda: b"\x1b[<35;" in data())
                xdo("key", "ctrl+f", "ctrl+g", "F2", "F3", "F4")
                wait_for(lambda: b"\x1b[102;5u" in data() and b"\x1b[103;5:3u" in data())
                assert b"\x1b[1;1:1Q" in data() and b"\x1b[1;1:3Q" in data(), data()
                xdo("key", "shift+Return")
                wait_for(lambda: b"\x1b[13;2u" in data())
                before = len(data())
                xdo("key", "ctrl+alt+f")
                wait_for(lambda: b"\x1b[O" in data()[before:])
                assert not next(item for item in rpc("app.diagnostics")["terminals"] if item["surface_id"] == target)["input_focused"]
                xdo("key", "Escape")
                wait_for(lambda: b"\x1b[I" in data()[before:])
                before = len(data())
                xdo("mousedown", "3")
                wait_for(lambda: b"\x1b[<2;" in data()[before:])
                assert not data()[before:].endswith(b"m"), data()[before:]
                xdo("mousemove", "--window", window, "620", "320")
                xdo("mouseup", "3")
                wait_for(lambda: data()[before:].endswith(b"m"))
                before = len(data())
                xdo("click", "2")
                wait_for(lambda: b"\x1b[<1;" in data()[before:])
                before = len(data())
                xdo("click", "4")
                wait_for(lambda: data()[before:].count(b"\x1b[<64;") >= 3)
                before = len(data())
                xdo("keydown", "Shift_L")
                xdo("click", "3")
                xdo("keyup", "Shift_L")
                time.sleep(0.1)
                assert b"\x1b[<" not in data()[before:], data()[before:]
                xdo("key", "Escape")
                before = len(data())
                rpc("surface.activate", {"id": initial})
                wait_for(lambda: b"\x1b[O" in data()[before:])
                rpc("surface.activate", {"id": target})
                wait_for(lambda: b"\x1b[I" in data()[before:])
                print("Input protocol smoke passed: shortcuts, hover, buttons, wheel, focus")
            except AssertionError as error:
                raise AssertionError(f"{error}; PTY received: {data()!r}") from error
            finally:
                app.terminate()
                try:
                    app.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait(timeout=5)


if __name__ == "__main__":
    main()
