use super::*;
use std::net::{TcpListener, TcpStream};
use std::thread::JoinHandle;
use wmcterm_core::AgentRunMode;

/// Permissions belong to the server-side thread, never the remote TUI flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexPermissions {
    pub approval_policy: Value,
    pub approvals_reviewer: String,
    pub sandbox: String,
}

impl Default for CodexPermissions {
    fn default() -> Self {
        Self {
            approval_policy: json!("on-request"),
            approvals_reviewer: "user".into(),
            sandbox: "workspace-write".into(),
        }
    }
}

impl CodexPermissions {
    pub fn from_thread_response(response: &Value) -> Option<Self> {
        let result = response.get("result")?;
        let sandbox = match result.pointer("/sandbox/type")?.as_str()? {
            "readOnly" => "read-only",
            "workspaceWrite" => "workspace-write",
            "dangerFullAccess" => "danger-full-access",
            _ => return None,
        };
        Some(Self {
            approval_policy: result.get("approvalPolicy")?.clone(),
            approvals_reviewer: result.get("approvalsReviewer")?.as_str()?.into(),
            sandbox: sandbox.into(),
        })
    }

    pub fn run_mode(&self) -> AgentRunMode {
        if self.sandbox == "danger-full-access" {
            AgentRunMode::FullAccess
        } else if matches!(
            self.approvals_reviewer.as_str(),
            "auto_review" | "guardian_subagent"
        ) {
            AgentRunMode::Unattended
        } else {
            AgentRunMode::Interactive
        }
    }

    pub fn extract(args: &[String]) -> Result<(Self, Vec<String>), BridgeError> {
        let mut permissions = Self::default();
        let mut client = Vec::new();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            if arg == "--" {
                client.push(arg.clone());
                client.extend(args.cloned());
                break;
            }
            match arg.as_str() {
                "--approve-for-me" | "--full-auto" => {
                    permissions = Self::default();
                    if arg == "--approve-for-me" {
                        permissions.approvals_reviewer = "auto_review".into();
                    }
                }
                "--dangerously-bypass-approvals-and-sandbox" | "--yolo" => {
                    permissions = Self {
                        approval_policy: json!("never"),
                        approvals_reviewer: "user".into(),
                        sandbox: "danger-full-access".into(),
                    };
                }
                "-a" | "--ask-for-approval" | "-s" | "--sandbox" => {
                    let value = args
                        .next()
                        .ok_or_else(|| BridgeError::Protocol(format!("{arg} requires a value")))?;
                    if matches!(arg.as_str(), "-a" | "--ask-for-approval") {
                        permissions.approval_policy = json!(value);
                    } else {
                        permissions.sandbox.clone_from(value);
                    }
                }
                "-c" | "--config" => {
                    let value = args
                        .next()
                        .ok_or_else(|| BridgeError::Protocol(format!("{arg} requires a value")))?;
                    if !permissions.config(value)? {
                        client.extend([arg.clone(), value.clone()]);
                    }
                }
                "--remote" | "--remote-auth-token-env" | "--add-dir" => {
                    return Err(BridgeError::Protocol(format!(
                        "{arg} is not supported by the managed Codex permission bridge"
                    )));
                }
                _ => {
                    if let Some(value) = arg.strip_prefix("--ask-for-approval=") {
                        permissions.approval_policy = json!(value);
                    } else if let Some(value) = arg.strip_prefix("--sandbox=") {
                        permissions.sandbox = value.into();
                    } else if let Some(value) =
                        arg.strip_prefix("-a").filter(|value| !value.is_empty())
                    {
                        permissions.approval_policy = json!(value.trim_start_matches('='));
                    } else if let Some(value) =
                        arg.strip_prefix("-s").filter(|value| !value.is_empty())
                    {
                        permissions.sandbox = value.trim_start_matches('=').into();
                    } else if let Some(value) = arg.strip_prefix("--config=").or_else(|| {
                        arg.strip_prefix("-c")
                            .map(|value| value.trim_start_matches('='))
                    }) {
                        if !permissions.config(value)? {
                            client.push(arg.clone());
                        }
                    } else if arg.starts_with("--remote=") || arg.starts_with("--add-dir=") {
                        return Err(BridgeError::Protocol(format!(
                            "{arg} is not supported by the managed Codex permission bridge"
                        )));
                    } else {
                        client.push(arg.clone());
                    }
                }
            }
        }
        if !matches!(
            permissions.sandbox.as_str(),
            "read-only" | "workspace-write" | "danger-full-access"
        ) {
            return Err(BridgeError::Protocol("unsupported Codex sandbox".into()));
        }
        if !matches!(
            permissions.approvals_reviewer.as_str(),
            "user" | "auto_review" | "guardian_subagent"
        ) {
            return Err(BridgeError::Protocol(
                "unsupported Codex approvals reviewer".into(),
            ));
        }
        Ok((permissions, client))
    }

    fn config(&mut self, config: &str) -> Result<bool, BridgeError> {
        let Some((key, raw)) = config.split_once('=') else {
            return Ok(false);
        };
        let key = key.trim();
        if !matches!(
            key,
            "approval_policy" | "approvals_reviewer" | "sandbox_mode"
        ) {
            if key == "permissions"
                || key.starts_with("permissions.")
                || key.starts_with("sandbox_workspace_write.")
            {
                return Err(BridgeError::Protocol(format!(
                    "managed Codex does not support permission override {key}"
                )));
            }
            return Ok(false);
        }
        let value = toml::from_str::<toml::Table>(&format!("value={raw}"))
            .ok()
            .and_then(|mut table| table.remove("value"))
            .map(|value| serde_json::to_value(value).expect("TOML values serialize to JSON"))
            .unwrap_or_else(|| json!(raw.trim()));
        match key {
            "approval_policy" => self.approval_policy = value,
            "approvals_reviewer" => {
                self.approvals_reviewer = value
                    .as_str()
                    .ok_or_else(|| BridgeError::Protocol("invalid approvals_reviewer".into()))?
                    .into()
            }
            "sandbox_mode" => {
                self.sandbox = value
                    .as_str()
                    .ok_or_else(|| BridgeError::Protocol("invalid sandbox_mode".into()))?
                    .into()
            }
            _ => unreachable!(),
        }
        Ok(true)
    }

    fn apply(&self, request: &mut Value) -> Result<(), BridgeError> {
        let params = request
            .get_mut("params")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| BridgeError::Protocol("thread request requires object params".into()))?;
        params.remove("permissions");
        if let Some(config) = params.get_mut("config").and_then(Value::as_object_mut) {
            config.retain(|key, _| {
                !matches!(
                    key.as_str(),
                    "approval_policy" | "approvals_reviewer" | "sandbox_mode" | "permissions"
                ) && !key.starts_with("permissions.")
                    && !key.starts_with("sandbox_workspace_write.")
            });
        }
        params.insert("approvalPolicy".into(), self.approval_policy.clone());
        params.insert("approvalsReviewer".into(), json!(self.approvals_reviewer));
        params.insert("sandbox".into(), json!(self.sandbox));
        Ok(())
    }

    fn verify(&self, response: &Value) -> Result<(), BridgeError> {
        let result = &response["result"];
        let sandbox = match self.sandbox.as_str() {
            "read-only" => "readOnly",
            "workspace-write" => "workspaceWrite",
            "danger-full-access" => "dangerFullAccess",
            _ => return Err(BridgeError::Protocol("unsupported sandbox".into())),
        };
        let reviewer = result["approvalsReviewer"].as_str();
        let reviewer_matches = reviewer == Some(self.approvals_reviewer.as_str())
            || (matches!(reviewer, Some("auto_review" | "guardian_subagent"))
                && matches!(
                    self.approvals_reviewer.as_str(),
                    "auto_review" | "guardian_subagent"
                ));
        if result["approvalPolicy"] != self.approval_policy
            || !reviewer_matches
            || result.pointer("/sandbox/type").and_then(Value::as_str) != Some(sandbox)
        {
            return Err(BridgeError::Protocol("Codex did not apply the requested permissions; managed restore stopped (no automatic fallback)".into()));
        }
        Ok(())
    }
}

pub(super) fn control(
    config: &BridgeConfig,
    method: &str,
    mut params: Value,
) -> Result<Value, BridgeError> {
    params["surface_id"] = json!(config.root_surface_id);
    params["lifecycle_id"] =
        json!(std::env::var("WMCTERM_TERMINAL_LIFECYCLE_ID").unwrap_or_default());
    let response = call(
        &config.control_socket,
        &Request {
            id: format!("codex-managed-{}", next_rpc_id()),
            method: method.into(),
            params,
        },
    )
    .map_err(|error| BridgeError::Control(error.to_string()))?;
    if let Some(error) = response.error {
        return Err(BridgeError::Control(error.message));
    }
    response
        .result
        .ok_or_else(|| BridgeError::Control("missing managed Codex response".into()))
}

pub(super) struct PermissionProxy {
    pub endpoint: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    config: BridgeConfig,
}

impl PermissionProxy {
    pub fn start(config: BridgeConfig, permissions: CodexPermissions) -> Result<Self, BridgeError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let token = format!("/{}", uuid::Uuid::new_v4());
        let endpoint = format!("ws://{}{token}", listener.local_addr()?);
        control(
            &config,
            "agent.codex.register",
            json!({
                "mode": permissions.run_mode(), "endpoint": config.endpoint,
                "program": config.codex_program, "cwd": config.workspace_cwd, "permissions": permissions,
            }),
        )?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_config = config.clone();
        let worker = std::thread::Builder::new()
            .name("wmcterm-codex-permissions".into())
            .spawn(move || {
                let mut clients: Vec<JoinHandle<()>> = Vec::new();
                while !worker_stop.load(Ordering::Acquire) {
                    let mut index = 0;
                    while index < clients.len() {
                        if clients[index].is_finished() {
                            let _ = clients.swap_remove(index).join();
                        } else {
                            index += 1;
                        }
                    }
                    match listener.accept() {
                        Ok((stream, _)) if clients.len() < 16 => {
                            let config = worker_config.clone();
                            let permissions = permissions.clone();
                            let token = token.clone();
                            let stop = Arc::clone(&worker_stop);
                            match std::thread::Builder::new()
                                .name("wmcterm-codex-client".into())
                                .spawn(move || {
                                    if let Err(error) =
                                        relay(stream, &token, &config, &permissions, &stop)
                                    {
                                        eprintln!("wmcterm: Codex permission bridge: {error}");
                                    }
                                }) {
                                Ok(client) => clients.push(client),
                                Err(error) => eprintln!("wmcterm: Codex client thread: {error}"),
                            }
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10))
                        }
                        Err(_) => break,
                    }
                }
                for client in clients {
                    let _ = client.join();
                }
            })?;
        Ok(Self {
            endpoint,
            stop,
            worker: Some(worker),
            config,
        })
    }
}

impl Drop for PermissionProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let _ = control(
            &self.config,
            "agent.codex.stopped",
            json!({"endpoint": self.config.endpoint}),
        );
    }
}

// tungstenite's handshake callback requires an unboxed HTTP error response.
#[allow(clippy::result_large_err)]
fn relay(
    stream: TcpStream,
    token: &str,
    config: &BridgeConfig,
    permissions: &CodexPermissions,
    stop: &AtomicBool,
) -> Result<(), BridgeError> {
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut client = tungstenite::accept_hdr(
        stream,
        |request: &tungstenite::handshake::server::Request, response| {
            if request.uri().path() == token {
                Ok(response)
            } else {
                Err(tungstenite::http::Response::builder()
                    .status(403)
                    .body(Some("invalid managed endpoint".into()))
                    .expect("valid HTTP response"))
            }
        },
    )
    .map_err(|error| BridgeError::Protocol(error.to_string()))?;
    client
        .get_mut()
        .set_read_timeout(Some(Duration::from_millis(10)))?;
    let mut server = connect_with_retry(&config.endpoint, stop)?;
    if let MaybeTlsStream::Plain(stream) = server.get_mut() {
        stream.set_read_timeout(Some(Duration::from_millis(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    }
    let mut pending = HashSet::new();
    let mut verified: HashMap<String, Value> = HashMap::new();
    while !stop.load(Ordering::Acquire) {
        match client.read() {
            Ok(Message::Text(text)) => {
                let mut request: Value = serde_json::from_str(&text)?;
                match request["method"].as_str() {
                    Some("thread/start" | "thread/resume" | "thread/fork") => {
                        if pending.len() >= 32 {
                            return Err(BridgeError::Protocol(
                                "too many pending thread requests".into(),
                            ));
                        }
                        permissions.apply(&mut request)?;
                        pending.insert(request["id"].to_string());
                    }
                    Some("turn/start" | "turn/steer")
                        if !verified.contains_key(
                            request["params"]["threadId"].as_str().unwrap_or_default(),
                        ) =>
                    {
                        client.send(Message::Text(json!({"id": request["id"], "error": {"code": -32000, "message": "Thread permissions have not been verified; resume the thread first"}}).to_string().into()))?;
                        continue;
                    }
                    Some("turn/start") => {
                        let sandbox = verified
                            .get(request["params"]["threadId"].as_str().unwrap_or_default())
                            .expect("verified thread")
                            .clone();
                        request["params"]["approvalPolicy"] = permissions.approval_policy.clone();
                        request["params"]["approvalsReviewer"] =
                            json!(permissions.approvals_reviewer);
                        request["params"]["sandboxPolicy"] = sandbox;
                        if let Some(params) = request["params"].as_object_mut() {
                            params.remove("permissions");
                        }
                    }
                    _ => {}
                }
                send_json(&mut server, &request)?;
            }
            Ok(Message::Close(_)) => break,
            Ok(Message::Ping(_)) => {
                client.flush()?;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(tungstenite::Error::ConnectionClosed | tungstenite::Error::AlreadyClosed) => break,
            Err(error) => return Err(error.into()),
        }
        match read_json_poll(&mut server)? {
            SocketRead::Message(response) => {
                if response.get("method").is_none()
                    && response.get("id").is_some()
                    && pending.remove(&response["id"].to_string())
                    && response.get("error").is_none()
                {
                    let result = permissions.verify(&response).and_then(|()| {
                        let thread = response.pointer("/result/thread/id").and_then(Value::as_str)
                            .ok_or_else(|| BridgeError::Protocol("missing resumed thread ID".into()))?;
                        control(config, "agent.codex.ready", json!({"session_id": thread, "mode": permissions.run_mode(), "cwd": response["result"]["cwd"]}))?;
                        verified.insert(thread.to_owned(), response["result"]["sandbox"].clone());
                        Ok(())
                    });
                    if let Err(error) = result {
                        client.send(Message::Text(json!({"id": response["id"], "error": {"code": -32000, "message": error.to_string()}}).to_string().into()))?;
                        return Err(error);
                    }
                }
                client.send(Message::Text(response.to_string().into()))?;
            }
            SocketRead::Closed => break,
            SocketRead::Idle => {}
        }
    }
    let _ = server.close(None);
    let _ = client.close(None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_verifies_resumes_and_blocks_turns_until_permissions_match() {
        for mismatch in [false, true] {
            let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("ws://{}/test-token", listener.local_addr().unwrap());
            let socket_path = std::env::temp_dir()
                .join(format!("wmcterm-permission-{}.sock", uuid::Uuid::new_v4()));
            let ready_count = Arc::new(AtomicU64::new(0));
            let observed = Arc::clone(&ready_count);
            let _control = wmcterm_ipc::UnixServer::spawn(&socket_path, move |request| {
                assert_eq!(request.method, "agent.codex.ready");
                observed.fetch_add(1, Ordering::Relaxed);
                wmcterm_ipc::Response::success(request.id, json!({"ok": true}))
            })
            .unwrap();
            let config = BridgeConfig {
                endpoint: format!("ws://{}", upstream.local_addr().unwrap()),
                control_socket: socket_path,
                workspace_id: "workspace".into(),
                root_surface_id: "surface".into(),
                codex_program: "codex".into(),
                workspace_cwd: "/tmp".into(),
                max_auto_depth: 2,
            };
            let provider =
                std::thread::spawn(move || {
                    let mut socket = tungstenite::accept(upstream.accept().unwrap().0).unwrap();
                    let mut read = || -> Value {
                        loop {
                            if let Message::Text(text) = socket.read().unwrap() {
                                return serde_json::from_str(&text).unwrap();
                            }
                        }
                    };
                    let request = read();
                    assert_eq!(request["method"], "thread/resume");
                    assert_eq!(request["params"]["approvalsReviewer"], "auto_review");
                    assert_eq!(request["params"]["sandbox"], "workspace-write");
                    assert_eq!(request["params"]["cwd"], "/worktree");
                    socket.send(Message::Text(json!({"id": request["id"], "result": {
                    "thread": {"id": "thread"}, "approvalPolicy": "on-request",
                    "approvalsReviewer": if mismatch { "user" } else { "auto_review" },
                    "sandbox": {"type": "workspaceWrite", "writableRoots": ["/worktree"]}
                }}).to_string().into())).unwrap();
                    if !mismatch {
                        let Message::Text(text) = socket.read().unwrap() else {
                            panic!("expected turn")
                        };
                        let turn: Value = serde_json::from_str(&text).unwrap();
                        assert_eq!(turn["method"], "turn/start");
                        assert_eq!(turn["params"]["sandboxPolicy"]["type"], "workspaceWrite");
                        assert_eq!(turn["params"]["approvalsReviewer"], "auto_review");
                        socket
                            .send(Message::Text(
                                json!({"id": turn["id"], "result": {"ok": true}})
                                    .to_string()
                                    .into(),
                            ))
                            .unwrap();
                    }
                });
            let stop = Arc::new(AtomicBool::new(false));
            let relay_stop = Arc::clone(&stop);
            let worker = std::thread::spawn(move || {
                let permissions = CodexPermissions::extract(&["--approve-for-me".into()])
                    .unwrap()
                    .0;
                relay(
                    listener.accept().unwrap().0,
                    "/test-token",
                    &config,
                    &permissions,
                    &relay_stop,
                )
            });
            let (mut client, _) = connect(&endpoint).unwrap();
            send_json(
                &mut client,
                &json!({"id": 1, "method": "turn/start", "params": {"threadId": "thread"}}),
            )
            .unwrap();
            assert!(
                read_json(&mut client)
                    .unwrap()
                    .unwrap()
                    .get("error")
                    .is_some()
            );
            send_json(&mut client, &json!({"id": 2, "method": "thread/resume", "params": {"threadId": "thread", "cwd": "/worktree"}})).unwrap();
            let response = read_json(&mut client).unwrap().unwrap();
            assert_eq!(response.get("error").is_some(), mismatch);
            if !mismatch {
                send_json(&mut client, &json!({"id": 3, "method": "turn/start", "params": {"threadId": "thread", "sandboxPolicy": {"type": "dangerFullAccess"}}})).unwrap();
                assert_eq!(
                    read_json(&mut client).unwrap().unwrap()["result"]["ok"],
                    true
                );
            }
            stop.store(true, Ordering::Release);
            let _ = client.close(None);
            let result = worker.join().unwrap();
            if mismatch {
                assert!(result.is_err());
            }
            provider.join().unwrap();
            assert_eq!(ready_count.load(Ordering::Relaxed), u64::from(!mismatch));
        }
    }

    #[test]
    fn all_modes_move_permissions_off_the_remote_tui() {
        for (flag, mode, policy, reviewer, sandbox) in [
            (
                "--full-auto",
                AgentRunMode::Interactive,
                "on-request",
                "user",
                "workspaceWrite",
            ),
            (
                "--approve-for-me",
                AgentRunMode::Unattended,
                "on-request",
                "auto_review",
                "workspaceWrite",
            ),
            (
                "--dangerously-bypass-approvals-and-sandbox",
                AgentRunMode::FullAccess,
                "never",
                "user",
                "dangerFullAccess",
            ),
        ] {
            let (permissions, client) =
                CodexPermissions::extract(&[flag.into(), "resume".into(), "thread".into()])
                    .unwrap();
            assert_eq!(client, ["resume", "thread"]);
            assert_eq!(permissions.run_mode(), mode);
            let mut request = json!({"method": "thread/resume", "params": {"threadId": "thread", "cwd": "/work"}});
            permissions.apply(&mut request).unwrap();
            assert_eq!(request["params"]["cwd"], "/work");
            assert_eq!(request["params"]["approvalPolicy"], policy);
            assert_eq!(request["params"]["approvalsReviewer"], reviewer);
            permissions.verify(&json!({"result": {"approvalPolicy": policy, "approvalsReviewer": reviewer, "sandbox": {"type": sandbox}}})).unwrap();
            assert!(permissions.verify(&json!({"result": {}})).is_err());
        }
    }

    #[test]
    fn explicit_permissions_and_prompt_boundaries_are_preserved() {
        let (permissions, client) = CodexPermissions::extract(&[
            "-c".into(),
            "approval_policy=\"never\"".into(),
            "--sandbox=read-only".into(),
            "--config=approvals_reviewer=\"auto_review\"".into(),
            "--model".into(),
            "test".into(),
            "--".into(),
            "--yolo".into(),
        ])
        .unwrap();
        assert_eq!(permissions.approval_policy, "never");
        assert_eq!(permissions.sandbox, "read-only");
        assert_eq!(client, ["--model", "test", "--", "--yolo"]);
        assert!(CodexPermissions::extract(&["--sandbox".into()]).is_err());
        assert!(CodexPermissions::extract(&["--remote=ws://other".into()]).is_err());
    }
}
