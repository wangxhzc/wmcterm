#!/usr/bin/env python3
"""Isolated GUI + real loopback OpenSSH smoke. Run under xvfb-run -a."""
import json
import os
from pathlib import Path
import pwd
import shlex
import shutil
import socket
import subprocess
import sys
import tempfile
import time


def main():
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else "target/debug/wmcterm").resolve())
    bash = str(Path(os.environ.get("WMCTERM_TEST_BASH", "/bin/bash")).resolve())
    for tool in ("ssh", "sshd", "ssh-keygen"):
        if not shutil.which(tool):
            raise RuntimeError(f"missing dependency: {tool}")
    with tempfile.TemporaryDirectory(prefix="wmcterm-ssh-smoke-") as tmp:
        root = Path(tmp)
        help_env = dict(os.environ, WMCTERM_CONTROL_SOCKET=str(root / "absent.sock"))
        for args in (["ssh", "--help"], ["ssh", "exec", "--help"]):
            result = subprocess.run([binary, *args], env=help_env, capture_output=True, text=True, check=True)
            assert "wmcterm ssh HOST" in result.stdout
        for name in ("home", "remote", "bin", "runtime", "config", "data"):
            (root / name).mkdir(mode=0o700)
        (root / "bin/bash").symlink_to(bash)
        for name in ("host_key", "client_key"):
            subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(root / name)], check=True)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        (root / "authorized_keys").write_text((root / "client_key.pub").read_text())
        remote = root / "remote.sh"
        remote.write_text(f"#!/bin/bash\nexport HOME={shlex.quote(str(root / 'remote'))}\nexport PATH={shlex.quote(str(root / 'bin'))}:/usr/bin:/bin\nexec bash -c \"$SSH_ORIGINAL_COMMAND\"\n")
        remote.chmod(0o700)
        config = root / "sshd_config"
        config.write_text(f"""ListenAddress 127.0.0.1
Port {port}
HostKey {root}/host_key
PidFile {root}/sshd.pid
AuthorizedKeysFile {root}/authorized_keys
StrictModes no
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
AuthenticationMethods publickey
ForceCommand /bin/bash {remote}
""")
        (root / "known_hosts").write_text(f"[127.0.0.1]:{port} " + (root / "host_key.pub").read_text())
        client_config = root / "ssh_config"
        client_config.write_text(f"""Host smoke
  HostName 127.0.0.1
  Port {port}
  User {pwd.getpwuid(os.getuid()).pw_name}
  IdentityFile {root}/client_key
  IdentitiesOnly yes
  UserKnownHostsFile {root}/known_hosts
  StrictHostKeyChecking yes
  BatchMode yes
""")
        shim = root / "bin/ssh"
        shim.write_text(f"#!/bin/sh\nexec /usr/bin/ssh -F {shlex.quote(str(client_config))} \"$@\"\n")
        shim.chmod(0o700)
        env = {k: v for k, v in os.environ.items() if not k.startswith("WMCTERM_")}
        env.update(HOME=str(root / "home"), SHELL=str(root / "bin/bash"), PATH=f"{root}/bin:/usr/bin:/bin",
                   XDG_RUNTIME_DIR=str(root / "runtime"), XDG_CONFIG_HOME=str(root / "config"),
                   XDG_DATA_HOME=str(root / "data"), WGPU_BACKEND="gl")
        processes = []
        with (root / "sshd.log").open("w") as ssh_log, (root / "gui.log").open("w") as gui_log:
            try:
                processes.append(subprocess.Popen([shutil.which("sshd"), "-D", "-e", "-f", str(config)], stdout=ssh_log, stderr=ssh_log))
                processes.append(subprocess.Popen([binary, "gui"], cwd=root, env=env, stdout=gui_log, stderr=gui_log))

                def cli(*args, fail=False, input=None):
                    result = subprocess.run([binary, *args], env=env, text=True, input=input, capture_output=True, timeout=40)
                    if fail:
                        assert result.returncode != 0, result.stdout
                        return result.stderr
                    if result.returncode:
                        raise RuntimeError(result.stderr)
                    return json.loads(result.stdout)

                def wait(test):
                    deadline = time.monotonic() + 20
                    last = None
                    while time.monotonic() < deadline:
                        try:
                            last = test()
                            if last:
                                return last
                        except (RuntimeError, KeyError):
                            pass
                        time.sleep(0.05)
                    raise AssertionError(f"wait timed out: {last}")

                initial = wait(lambda: cli("status"))
                env["WMCTERM_WORKSPACE_ID"] = initial["active_workspace"]
                surfaces = cli("surface", "list")
                surface = surfaces[0]["id"]
                target = ("--surface", surface)
                status = lambda: cli("ssh", "status", *target)
                wait(lambda: status()["local_ready"])
                cli("workspace", "new", "--name", "foreground", "--cwd", str(root))
                before = cli("status")
                cli("rpc", "ssh.connect", json.dumps({"host": "smoke"}), fail=True)
                cli("rpc", "ssh.connect", json.dumps({"surface_id": surface, "workspace": before["active_workspace"], "host": "smoke"}), fail=True)
                pane = cli("pane", "list")[0]["id"]
                env["WMCTERM_PANE_ID"] = pane
                panes_before = len(cli("pane", "list"))
                count = len(cli("surface", "list"))
                cli("ssh", "smoke", "--new", "--pane", pane, "--command", "true", fail=True)
                cli("ssh", "smoke", "--new", "--pane", pane, "--surface", surface, fail=True)
                assert len(cli("surface", "list")) == count
                connected = cli("ssh", "smoke")
                assert connected["state"] == "ready", connected
                assert connected["pane_id"] != pane
                assert len(cli("pane", "list")) == panes_before + 1
                assert connected["workspace_id"] == env["WMCTERM_WORKSPACE_ID"]
                assert connected["surface_id"] != surface
                assert len(cli("surface", "list")) == count + 1
                surface = connected["surface_id"]
                target = ("--surface", surface)
                connection = connected["connection_id"]
                saved = (*target, "--connection", connection, "--host", "smoke")
                def manual_input(label):
                    command = f"printf 'manual-%s-ok\\n' {label}"
                    cli("send", *target, "--", command)
                    wait(lambda: command in cli("screen", "read", *target)["text"])
                    cli("send-key", "Enter", *target)
                    wait(lambda: f"manual-{label}-ok" in cli("screen", "read", *target)["text"])
                    wait(lambda: status()["state"] == "ready")

                manual_input("connected")
                result = cli("ssh", "exec", *saved, "--command", "stty -a", "--wait")
                modes = result["command"]["output"].replace(";", " ").split()
                assert "echo" in modes and "icanon" in modes and "isig" in modes, result
                script = root / "diagnostic script.sh"
                script.write_text("printf '%s\\n' \"a'b\" '$HOME' '\u4e2d\u6587 \U0001f600 e\u0301'\nprintf '%s' \"$((1 + 2))\"\n")
                for source, stdin in [(("--command-file", str(script)), None), (("--command-stdin",), script.read_text())]:
                    result = cli("ssh", "exec", *saved, *source, "--wait", input=stdin)
                    assert result["command"]["output"] == "a'b\r\n$HOME\r\n\u4e2d\u6587 \U0001f600 e\u0301\r\n3", result
                    assert result["command"]["exit_code"] == 0, result
                previous = status()["command"]["command_id"]
                cli("ssh", "exec", *target, "--connection", connection, "--host", "wrong", "--command", "touch /bad", fail=True)
                cli("ssh", "exec", *target, "--host", "smoke", "--command", "true", fail=True)
                cli("ssh", "exec", *saved, "--command", "true", "--command-stdin", fail=True)
                cli("ssh", "exec", *saved, "--command-file", str(root / "missing"), fail=True)
                cli("ssh", "exec", *saved, "--command-stdin", input="x" * 16385, fail=True)
                assert status()["command"]["command_id"] == previous
                result = cli("ssh", "exec", *target, "--command", "printf %s \"$BASH_VERSION\"", "--wait")
                actual_version = result["command"]["output"]
                expected_version = subprocess.check_output([bash, "-c", "printf %s \"$BASH_VERSION\""], text=True)
                assert actual_version == expected_version, result
                assert cli("status")["active_workspace"] == before["active_workspace"]
                assert list((root / "remote/.wmcterm/tmp").iterdir()) == []
                assert (root / "remote/.wmcterm").stat().st_mode & 0o777 == 0o700
                command = "cd /tmp; printf '\u4e2d\u6587 \U0001f600 e\u0301\\n'; false"
                result = cli("ssh", "exec", *target, "--command", command, "--wait")
                assert result["command"]["exit_code"] == 1, result
                assert result["command"]["output"] == "\u4e2d\u6587 \U0001f600 e\u0301\r\n", result
                assert result["cwd"] == "/tmp", result
                manual_input("completed")
                cli("rpc", "ssh.exec", json.dumps({"surface_id": surface, "connection_id": "stale", "command": "touch /bad"}), fail=True)
                result = cli("ssh", "exec", *target, "--command", "printf 123456789", "--max-output-bytes", "4", "--wait")
                assert result["command"]["output"] == "1234" and result["command"]["truncated"], result
                result = cli("ssh", "exec", *target, "--command", "printf '\u4e2d\u6587'", "--max-output-bytes", "4", "--wait")
                assert result["command"]["output"] == "\u4e2d" and result["command"]["truncated"], result
                result = cli("ssh", "exec", *target, "--command", "sleep 1; printf later", "--wait", "--timeout-ms", "10")
                assert result["wait_timed_out"] and result["command"]["state"] in ("submitted", "running"), result
                cli("ssh", "exec", *target, "--command", "true", fail=True)
                wait(lambda: status()["command"]["state"] == "completed")
                result = cli("ssh", "exec", *target, "--command", "sleep 30")
                wait(lambda: status()["command"]["state"] == "running")
                cancelled = cli("ssh", "cancel", *target, "--connection", connection, "--command-id", result["command"]["command_id"], "--wait")
                assert cancelled["command"]["state"] == "cancelled", cancelled
                wait(lambda: status()["state"] == "ready")
                assert status()["command"]["state"] == "cancelled", status()
                manual_input("cancelled")
                cli("ssh", "exec", *target, "--command", "exit")
                wait(lambda: status()["state"] == "disconnected")
                assert status()["command"]["state"] == "outcome_unknown"
                assert status()["local_ready"]
                cli("ssh", "smoke", *target, "--wait")
                manual_input("reconnected")
                cli("rpc", "ssh.exec", json.dumps({"surface_id": surface, "connection_id": connection, "command": "false"}), fail=True)
                (root / "home/.bashrc").write_text("sleep 1\n")
                timed = cli("ssh", "smoke", "--new", "--pane", pane, "--timeout-ms", "0")
                assert timed["wait_timed_out"] and timed["wait_phase"] == "local_ready", timed
                assert timed["requested_host"] == "smoke" and timed["connection_id"] is None, timed
                assert timed["surface_id"] != surface
                count = len(cli("surface", "list"))
                panes_before = len(cli("pane", "list"))
                resumed = cli("ssh", "smoke", "--surface", timed["surface_id"], "--wait")
                assert resumed["state"] == "ready" and resumed["surface_id"] == timed["surface_id"], resumed
                assert len(cli("pane", "list")) == panes_before
                assert len(cli("surface", "list")) == count
                cli("ssh", "smoke", "--surface", timed["surface_id"], "--wait", fail=True)
                assert cli("status")["active_workspace"] == before["active_workspace"]
                print(f"PASS: Bash {actual_version}, real SSH, isolated GUI, manual input/echo, terminal modes, one-step connection, file/stdin quoting, host checks, UTF-8, exit/cwd, output limit, timeout, cancellation, reconnect, stale IDs, remote cleanup")
            except Exception:
                if "target" in locals():
                    print(cli("ssh", "status", *target), file=sys.stderr)
                    print(cli("screen", "read", *target), file=sys.stderr)
                for name in ("sshd.log", "gui.log"):
                    print(f"{name}:\n{(root / name).read_text()[-6000:]}", file=sys.stderr)
                raise
            finally:
                for process in reversed(processes):
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait()


if __name__ == "__main__":
    main()
