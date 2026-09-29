//! Session-scoped Bash coordination. These events are not authorization signals.
use serde_json::{Value, json};
use uuid::Uuid;

const PREFIX: &[u8] = b"\x1b]777;wmcterm;";
// Bash 4.2 bind -x only resolves one level of keymap indirection.
pub const DISPATCH_PROBE: &[u8] = b"\x18\x1d";
const MAX_FRAME: usize = 32768;
pub const MAX_OUTPUT: usize = 1024 * 1024;

pub fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub fn bash_rc(token: &str, remote: bool) -> String {
    let cleanup = if remote {
        "command rm -f -- \"${BASH_SOURCE[0]}\"\n"
    } else {
        ""
    };
    format!(
        r#"{cleanup}[[ -f ~/.bashrc ]] && source ~/.bashrc
if (( BASH_VERSINFO[0] < 4 || (BASH_VERSINFO[0] == 4 && (BASH_VERSINFO[1] < 2 || (BASH_VERSINFO[1] == 2 && BASH_VERSINFO[2] < 46))) )); then
    builtin printf 'wmcterm: structured SSH requires Bash 4.2.46 or newer\n' >&2
    return
fi
__wmcterm_token={token}
__wmcterm_command=
# bind -x runs with Readline's no-echo terminal modes, not shell command modes.
__wmcterm_shell_tty=$(command stty -g)
__wmcterm_readline_tty=
__wmcterm_emit() {{
    builtin printf '\033]777;wmcterm;%s;%s;%s;%s;%s\007' "$__wmcterm_token" "$1" "$2" "$3" "$(builtin printf %s "$PWD" | od -An -v -tx1 | tr -d ' \n')"
}}
__wmcterm_prompt() {{
    local status=$?
    # Ctrl+C can unwind the callback; PROMPT_COMMAND must restore Readline too.
    if [[ -n $__wmcterm_readline_tty ]]; then
        __wmcterm_shell_tty=$(command stty -g)
        command stty "$__wmcterm_readline_tty"
        __wmcterm_readline_tty=
    fi
    if [[ -n $__wmcterm_command ]]; then
        __wmcterm_emit done "$__wmcterm_command" "$status"
        __wmcterm_command=
    fi
    __wmcterm_emit ready '' "$status"
    return "$status"
}}
__wmcterm_run() {{
    __wmcterm_command=$1
    __wmcterm_emit start "$1" 0
    builtin eval -- "$2"
}}
__wmcterm_dispatch() {{
    local __wmcterm_payload __wmcterm_line
    if [[ -n $READLINE_LINE ]]; then
        __wmcterm_emit rejected '' 1
        return
    fi
    __wmcterm_emit accept '' 0
    if ! IFS= builtin read -r __wmcterm_payload; then
        __wmcterm_emit rejected '' 1
        return
    fi
    if [[ $__wmcterm_payload != \\x* ]]; then
        READLINE_LINE=$__wmcterm_payload
        READLINE_POINT=${{#READLINE_LINE}}
        __wmcterm_emit rejected '' 1
        return
    fi
    builtin printf -v __wmcterm_line %b "$__wmcterm_payload"
    __wmcterm_readline_tty=$(command stty -g)
    if ! command stty "$__wmcterm_shell_tty"; then
        __wmcterm_emit rejected '' 1
        return
    fi
    builtin eval -- "$__wmcterm_line"
    __wmcterm_prompt
}}
bind -m emacs-standard -x '"\C-x\C-]": __wmcterm_dispatch'
bind -m vi-insert -x '"\C-x\C-]": __wmcterm_dispatch'
bind -m vi-command -x '"\C-x\C-]": __wmcterm_dispatch'
if (( BASH_VERSINFO[0] > 5 || (BASH_VERSINFO[0] == 5 && BASH_VERSINFO[1] >= 1) )); then
    PROMPT_COMMAND=(__wmcterm_prompt "${{PROMPT_COMMAND[@]}}")
else
    PROMPT_COMMAND=$'__wmcterm_prompt\n'${{PROMPT_COMMAND-}}
fi
"#
    )
}

fn dispatch_bytes(line: &str) -> Vec<u8> {
    use std::fmt::Write;
    let mut encoded = String::new();
    for byte in line.bytes() {
        let _ = write!(encoded, "\\x{byte:02x}");
    }
    encoded.push('\n');
    encoded.into_bytes()
}

fn bootstrap(token: &str) -> String {
    // One SSH connection stages and loads the rc file; no stdin pipe consumes authentication input.
    use std::fmt::Write;
    let mut script = String::new();
    for byte in bash_rc(token, true).bytes() {
        let _ = write!(script, "\\{byte:03o}");
    }
    format!(
        "umask 077; test ! -L \"$HOME/.wmcterm\" && test ! -L \"$HOME/.wmcterm/tmp\" || exit 1; mkdir -p \"$HOME/.wmcterm/tmp\" && chmod 700 \"$HOME/.wmcterm\" \"$HOME/.wmcterm/tmp\" || exit 1; f=$(mktemp \"$HOME/.wmcterm/tmp/bash.XXXXXXXXXX\") || exit 1; trap 'rm -f -- \"$f\"' EXIT; printf %b {} > \"$f\" || exit 1; bash --rcfile \"$f\" -i",
        quote(&script)
    )
}

#[derive(Debug)]
struct Execution {
    id: String,
    state: &'static str,
    output: Vec<u8>,
    limit: usize,
    truncated: bool,
    exit_code: Option<i32>,
}

pub struct SshState {
    pub final_status: std::sync::Arc<std::sync::Mutex<Option<Value>>>,
    local_token: String,
    token: Option<String>,
    host: Option<String>,
    state: &'static str,
    local_ready: bool,
    cwd: String,
    frame: Vec<u8>,
    execution: Option<Execution>,
    connection_exit_code: Option<i32>,
    pending_dispatch: Option<(String, String)>,
    dispatch_output: Option<Vec<u8>>,
}

impl Default for SshState {
    fn default() -> Self {
        Self {
            final_status: Default::default(),
            local_token: Uuid::new_v4().simple().to_string(),
            token: None,
            host: None,
            state: "local_unknown",
            local_ready: false,
            cwd: String::new(),
            frame: Vec::new(),
            execution: None,
            connection_exit_code: None,
            pending_dispatch: None,
            dispatch_output: None,
        }
    }
}

impl Drop for SshState {
    fn drop(&mut self) {
        self.disconnected();
        if let Ok(mut status) = self.final_status.lock() {
            *status = Some(self.status());
        }
    }
}

impl SshState {
    pub fn prepare_dispatch(&mut self, line: String) {
        let token = if self.state == "connecting" {
            &self.local_token
        } else {
            self.token.as_ref().expect("remote connection")
        };
        self.pending_dispatch = Some((token.clone(), line));
    }

    pub fn take_dispatch_output(&mut self) -> Option<Vec<u8>> {
        self.dispatch_output.take()
    }

    pub fn local_rc(&self) -> String {
        bash_rc(&self.local_token, false)
    }

    pub fn status(&self) -> Value {
        json!({"state": self.state, "connection_id": self.token, "host": self.host,
            "connection_exit_code": self.connection_exit_code,
            "cwd": self.cwd, "local_ready": self.local_ready,
            "command": self.execution.as_ref().map(|e| json!({"command_id": e.id,
                "state": e.state, "output": output_text(&e.output),
                "truncated": e.truncated, "exit_code": e.exit_code}))})
    }

    pub fn request(&mut self, method: &str, params: &Value) -> Result<Option<String>, String> {
        match method {
            "ssh.status" => Ok(None),
            "ssh.cancel" => {
                if params["connection_id"].as_str() != self.token.as_deref() || self.token.is_none()
                {
                    return Err("stale_connection".into());
                }
                let execution = self.execution.as_mut().ok_or("no active command")?;
                if params["command_id"].as_str() != Some(execution.id.as_str()) {
                    return Err("stale_command: command_id must match".into());
                }
                if execution.state != "running" {
                    return Err("command is not running".into());
                }
                execution.state = "cancelling";
                self.state = "cancelling";
                Ok(Some(String::new()))
            }
            "ssh.connect" => {
                if !self.local_ready {
                    return Err("shell_state_unknown: an untouched integrated local Bash prompt is required".into());
                }
                let host = params["host"].as_str().ok_or("missing host")?;
                if host.is_empty()
                    || host.starts_with('-')
                    || host.len() > 1024
                    || host.chars().any(|c| c.is_control() || c.is_whitespace())
                {
                    return Err("invalid SSH host; use an SSH config alias or user@host".into());
                }
                let token = Uuid::new_v4().simple().to_string();
                let line = format!("ssh -tt -- {} {}", quote(host), quote(&bootstrap(&token)));
                self.token = Some(token);
                self.host = Some(host.into());
                self.local_ready = false;
                self.state = "connecting";
                self.cwd.clear();
                self.execution = None;
                self.connection_exit_code = None;
                Ok(Some(line))
            }
            "ssh.exec" => {
                if params["connection_id"].as_str() != self.token.as_deref() || self.token.is_none()
                {
                    return Err("stale_connection: connection_id must match ssh.status".into());
                }
                if let Some(host) = params.get("expected_host")
                    && (host.as_str().is_none() || host.as_str() != self.host.as_deref())
                {
                    return Err("stale_host: expected_host must match the saved SSH host".into());
                }
                if self.state != "ready" {
                    return Err("terminal_busy: remote Bash is not ready".into());
                }
                let command = params["command"].as_str().ok_or("missing command")?;
                if command.len() > 16384 || command.contains('\0') {
                    return Err("command exceeds 16384 bytes or contains NUL".into());
                }
                let limit = match params.get("max_output_bytes") {
                    None => 65536,
                    Some(v) => v
                        .as_u64()
                        .filter(|n| *n > 0 && *n <= MAX_OUTPUT as u64)
                        .ok_or("max_output_bytes must be 1..1048576")?
                        as usize,
                };
                let id = Uuid::new_v4().simple().to_string();
                let line = format!("__wmcterm_run {} {}", quote(&id), quote(command));
                self.execution = Some(Execution {
                    id,
                    state: "submitted",
                    output: Vec::new(),
                    limit,
                    truncated: false,
                    exit_code: None,
                });
                self.state = "running";
                Ok(Some(line))
            }
            _ => Err("unknown SSH operation".into()),
        }
    }

    pub fn user_input(&mut self) {
        self.pending_dispatch = None;
        self.dispatch_output = None;
        self.local_ready = false;
        if self.token.is_none() {
            self.state = "local_unknown";
        }
        if self.token.is_some() {
            self.state = "intervened";
            if let Some(e) = &mut self.execution
                && matches!(e.state, "submitted" | "running" | "cancelling")
            {
                e.state = "outcome_unknown";
            }
        }
    }

    pub fn disconnected(&mut self) {
        self.user_input();
        self.state = "disconnected";
    }

    fn output(&mut self, byte: u8) {
        if let Some(e) = &mut self.execution
            && e.state == "running"
        {
            if e.output.len() < e.limit {
                e.output.push(byte);
            } else {
                e.truncated = true;
            }
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if self.frame.is_empty() && byte != 0x1b {
                self.output(byte);
                continue;
            }
            self.frame.push(byte);
            if self.frame.len() <= PREFIX.len() {
                if !PREFIX.starts_with(&self.frame) {
                    self.flush_frame();
                }
            } else if byte == 7 {
                let frame = std::mem::take(&mut self.frame);
                let body = String::from_utf8_lossy(&frame[PREFIX.len()..frame.len() - 1]);
                self.event(&body);
            } else if self.frame.len() >= MAX_FRAME {
                self.flush_frame();
            }
        }
    }

    fn flush_frame(&mut self) {
        let mut frame = std::mem::take(&mut self.frame);
        if frame.len() > 1 && frame.last() == Some(&0x1b) {
            frame.pop();
            self.frame.push(0x1b);
        }
        for byte in frame {
            self.output(byte);
        }
    }

    fn event(&mut self, body: &str) {
        let fields: Vec<_> = body.split(';').collect();
        if fields.len() != 5 {
            return;
        }
        let local = fields[0] == self.local_token;
        if !local && self.token.as_deref() != Some(fields[0]) {
            return;
        }
        let Ok(code) = fields[3].parse::<i32>() else {
            return;
        };
        let Some(cwd) = decode_hex(fields[4]) else {
            return;
        };
        if fields[1] == "accept" {
            if self
                .pending_dispatch
                .as_ref()
                .is_some_and(|(token, _)| token == fields[0])
            {
                let (_, line) = self.pending_dispatch.take().expect("checked above");
                self.dispatch_output = Some(dispatch_bytes(&line));
            }
            return;
        }
        if fields[1] == "rejected" {
            self.user_input();
            self.state = "input_rejected";
            return;
        }
        if local {
            if fields[1] == "ready" {
                if self.token.is_some() {
                    let failed = self.state == "connecting";
                    self.disconnected();
                    self.connection_exit_code = Some(code);
                    if failed {
                        self.state = "failed";
                    }
                } else {
                    self.state = "local_ready";
                }
                self.local_ready = true;
            }
            return;
        }
        if self.local_ready || self.state == "disconnected" {
            return;
        }
        self.cwd = cwd;
        match fields[1] {
            "ready"
                if self
                    .execution
                    .as_ref()
                    .is_none_or(|e| e.state != "submitted" && e.state != "running") =>
            {
                if let Some(e) = &mut self.execution
                    && e.state == "cancelling"
                {
                    e.state = "cancelled";
                }
                self.state = "ready"
            }
            "start" => {
                if let Some(e) = &mut self.execution
                    && e.id == fields[2]
                    && e.state == "submitted"
                {
                    e.state = "running";
                }
            }
            "done" => {
                if let Some(e) = &mut self.execution
                    && e.id == fields[2]
                    && matches!(e.state, "running" | "cancelling")
                {
                    e.state = if e.state == "cancelling" {
                        "cancelled"
                    } else {
                        "completed"
                    };
                    e.exit_code = Some(code);
                }
            }
            _ => {}
        }
    }
}

fn decode_hex(value: &str) -> Option<String> {
    if !value.len().is_multiple_of(2) {
        return None;
    }
    let bytes: Option<Vec<u8>> = value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).ok()?;
            u8::from_str_radix(text, 16).ok()
        })
        .collect();
    String::from_utf8(bytes?).ok()
}

fn output_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    let bytes = match std::str::from_utf8(bytes) {
        Err(error) if error.error_len().is_none() => &bytes[..error.valid_up_to()],
        _ => bytes,
    };
    String::from_utf8_lossy(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_payload_requires_a_matching_readline_acknowledgement() {
        let mut state = SshState::default();
        let local = state.local_token.clone();
        event(&mut state, &local, "ready", "", 0);
        let line = state
            .request("ssh.connect", &json!({"host":"host"}))
            .unwrap()
            .unwrap();
        state.prepare_dispatch(line);
        assert!(state.take_dispatch_output().is_none());
        event(&mut state, "stale", "accept", "", 0);
        assert!(state.take_dispatch_output().is_none());
        event(&mut state, &local, "accept", "", 0);
        assert!(state.take_dispatch_output().is_some());
        event(&mut state, &local, "accept", "", 0);
        assert!(state.take_dispatch_output().is_none());
        state.prepare_dispatch("never execute".into());
        state.user_input();
        event(&mut state, &local, "accept", "", 0);
        assert!(state.take_dispatch_output().is_none());
    }

    #[test]
    fn output_utf8_and_malformed_frames_are_bounded() {
        let text = "\u{4e2d}\u{6587}\u{1f600}e\u{301}";
        for end in 0..=text.len() {
            let output = output_text(&text.as_bytes()[..end]);
            assert!(text.starts_with(output.as_ref()));
        }
        let mut state = SshState::default();
        state.feed(PREFIX);
        state.feed(&vec![b'x'; MAX_FRAME * 2]);
        assert!(state.frame.len() < MAX_FRAME);
        state.feed(b"\x1b");
        let local = state.local_token.clone();
        event(&mut state, &local, "ready", "", 0);
        assert!(state.local_ready);
    }

    fn event(state: &mut SshState, token: &str, kind: &str, id: &str, code: i32) {
        let bytes = format!("\x1b]777;wmcterm;{token};{kind};{id};{code};2f746d70\x07");
        for chunk in bytes.as_bytes().chunks(3) {
            state.feed(chunk);
        }
    }

    #[test]
    fn protocol_rejects_stale_connections_and_bounds_output() {
        let mut state = SshState::default();
        assert!(
            state
                .request("ssh.connect", &json!({"host":"host"}))
                .is_err()
        );
        let local = state.local_token.clone();
        event(&mut state, &local, "ready", "", 0);
        state
            .request("ssh.connect", &json!({"host":"host"}))
            .unwrap();
        let token = state.token.clone().unwrap();
        event(&mut state, "stale", "ready", "", 0);
        assert_eq!(state.state, "connecting");
        event(&mut state, &token, "ready", "", 0);
        for host in [json!("other-host"), json!(null), json!(42)] {
            assert!(state.request("ssh.exec", &json!({"connection_id":token, "expected_host":host, "command":"touch /bad"})).is_err());
            assert_eq!(state.state, "ready");
            assert!(state.execution.is_none());
        }
        assert!(
            state
                .request(
                    "ssh.exec",
                    &json!({"connection_id":"stale", "command":"true"})
                )
                .is_err()
        );
        state
            .request(
                "ssh.exec",
                &json!({"connection_id":token, "expected_host":"host", "command":"false", "max_output_bytes":4}),
            )
            .unwrap();
        let id = state.execution.as_ref().unwrap().id.clone();
        state.feed(b"echo excluded");
        event(&mut state, &token, "start", &id, 0);
        state.feed(b"abcdef");
        event(&mut state, &token, "done", &id, 1);
        event(&mut state, &token, "ready", "", 1);
        assert_eq!(state.status()["command"]["output"], "abcd");
        assert_eq!(state.status()["command"]["exit_code"], 1);
        assert_eq!(state.status()["command"]["truncated"], true);
        assert_eq!(state.state, "ready");
    }

    #[test]
    fn intervention_and_disconnect_do_not_claim_success() {
        let mut state = SshState::default();
        let local = state.local_token.clone();
        event(&mut state, &local, "ready", "", 0);
        state
            .request("ssh.connect", &json!({"host":"host"}))
            .unwrap();
        let token = state.token.clone().unwrap();
        event(&mut state, &token, "ready", "", 0);
        state
            .request(
                "ssh.exec",
                &json!({"connection_id":token,"command":"sleep 10"}),
            )
            .unwrap();
        let id = state.execution.as_ref().unwrap().id.clone();
        state.user_input();
        event(&mut state, &token, "done", &id, 0);
        assert_eq!(state.status()["command"]["state"], "outcome_unknown");
        event(&mut state, &local, "ready", "", 0);
        event(&mut state, &token, "ready", "", 0);
        assert_eq!(state.state, "disconnected");
    }

    #[cfg(unix)]
    #[test]
    fn bash_pty_connect_exec_and_cleanup() {
        for vi in [false, true] {
            for array_prompt in [false, true] {
                bash_pty_for_mode(vi, array_prompt);
            }
        }
    }

    #[cfg(unix)]
    fn bash_pty_for_mode(vi: bool, array_prompt: bool) {
        use crate::{SessionCommand, TerminalSession, TerminalSize};
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        use wmcterm_pty::CommandSpec;
        let dir = std::env::temp_dir().join(format!("wmcterm-ssh-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let bash = std::env::var_os("WMCTERM_TEST_BASH").unwrap_or_else(|| "/bin/bash".into());
        std::os::unix::fs::symlink(bash, dir.join("bash")).unwrap();
        let hook = "'printf \"%s\\n\" \"$?\" >> \"$HOME/hooks\"'";
        let prompt = if array_prompt {
            format!("({hook})")
        } else {
            hook.into()
        };
        let rc = format!(
            "PROMPT_COMMAND={prompt}\nset -o {}\n",
            if vi { "vi" } else { "emacs" }
        );
        std::fs::write(dir.join(".bashrc"), &rc).unwrap();
        let shim = dir.join("ssh");
        std::fs::write(&shim, "#!/bin/bash\nexec bash -c \"${@: -1}\"\n").unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).unwrap();
        let session = TerminalSession::spawn(
            CommandSpec::new(dir.join("bash"))
                .env("HOME", &dir)
                .env("PATH", format!("{}:/usr/bin:/bin", dir.display())),
            TerminalSize::new(120, 30).unwrap(),
        )
        .unwrap();
        let wait = |predicate: &dyn Fn(&Value) -> bool| {
            let deadline = Instant::now() + Duration::from_secs(12);
            loop {
                let status = session.ssh_request("ssh.status", json!({})).unwrap();
                if predicate(&status) {
                    return status;
                }
                if Instant::now() > deadline {
                    panic!(
                        "timed out: {status}; screen: {:?}",
                        session.export_history()
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        wait(&|s| s["local_ready"] == true);
        let status = session
            .ssh_request("ssh.connect", json!({"host":"test-host"}))
            .unwrap();
        let connection = status["connection_id"].clone();
        wait(&|s| s["state"] == "ready");
        assert_eq!(
            std::fs::read_dir(dir.join(".wmcterm/tmp")).unwrap().count(),
            0
        );
        session
            .ssh_request(
                "ssh.exec",
                json!({"connection_id":connection,"command":"cd /tmp; printf 'hello\\n'; false"}),
            )
            .unwrap();
        let result = wait(&|s| s["command"]["state"] == "completed" && s["state"] == "ready");
        assert_eq!(result["command"]["exit_code"], 1);
        assert_eq!(result["command"]["output"], "hello\r\n");
        assert_eq!(result["cwd"], "/tmp");
        session
            .ssh_request(
                "ssh.exec",
                json!({"connection_id":connection,"command":"printf '%s' \"$PWD\""}),
            )
            .unwrap();
        let result = wait(&|s| s["command"]["state"] == "completed" && s["state"] == "ready");
        assert_eq!(result["command"]["output"], "/tmp");
        let input_path = dir.join("input-preserved");
        session
            .send(SessionCommand::Paste(format!(
                "printf preserved > {}",
                quote(input_path.to_str().unwrap())
            )))
            .unwrap();
        assert!(
            session
                .ssh_request(
                    "ssh.exec",
                    json!({"connection_id":connection,"command":"true"})
                )
                .is_err()
        );
        session
            .send(SessionCommand::Input(DISPATCH_PROBE.to_vec()))
            .unwrap();
        wait(&|s| s["state"] == "input_rejected");
        let history = session.export_history().unwrap();
        assert!(!history.contains("SHOULD_NOT_RUN"));
        assert!(!input_path.exists());
        session.send(SessionCommand::Input(vec![b'\r'])).unwrap();
        wait(&|_| input_path.exists());
        assert_eq!(std::fs::read_to_string(input_path).unwrap(), "preserved");
        assert_eq!(std::fs::read_to_string(dir.join(".bashrc")).unwrap(), rc);
        assert!(
            !std::fs::read_to_string(dir.join("hooks"))
                .unwrap()
                .is_empty()
        );
        session
            .send(SessionCommand::PasteLine("false".into()))
            .unwrap();
        wait(&|_| {
            std::fs::read_to_string(dir.join("hooks")).is_ok_and(|text| text.ends_with("1\n"))
        });
        drop(session);
    }
}
