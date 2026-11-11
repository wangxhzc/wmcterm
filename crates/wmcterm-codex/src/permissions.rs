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
                || key == "sandbox_workspace_write"
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
                    "approval_policy"
                        | "approvals_reviewer"
                        | "sandbox_mode"
                        | "permissions"
                        | "sandbox_workspace_write"
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
    pub token: String,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    config: BridgeConfig,
}

impl PermissionProxy {
    pub fn start(config: BridgeConfig, permissions: CodexPermissions) -> Result<Self, BridgeError> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let token = uuid::Uuid::new_v4().to_string();
        let endpoint = format!("ws://{}", listener.local_addr()?);
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
        let worker_token = token.clone();
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
                            let token = worker_token.clone();
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
            token,
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
            if request.uri().path() == "/"
                && request.uri().query().is_none()
                && request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    == Some(format!("Bearer {token}").as_str())
            {
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
    let mut verified: HashMap<String, VerifiedThread> = HashMap::new();
    let mut active_turns: HashMap<String, String> = HashMap::new();
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
                    Some(
                        "turn/start"
                        | "turn/steer"
                        | "thread/settings/update"
                        | "turn/settings/update",
                    ) if !verified.contains_key(
                        request["params"]["threadId"].as_str().unwrap_or_default(),
                    ) =>
                    {
                        client.send(Message::Text(json!({"id": request["id"], "error": {"code": -32000, "message": "Thread permissions have not been verified; resume the thread first"}}).to_string().into()))?;
                        continue;
                    }
                    Some("thread/settings/update" | "turn/settings/update") => {
                        if let Err(error) = validate_settings_update(&request) {
                            client.send(Message::Text(json!({"id": request["id"], "error": {"code": -32602, "message": error}}).to_string().into()))?;
                            continue;
                        }
                    }
                    Some("turn/start") => {
                        let thread = verified
                            .get(request["params"]["threadId"].as_str().unwrap_or_default())
                            .expect("verified thread");
                        if let Err(error) = thread.validate_turn(&request) {
                            client.send(Message::Text(json!({"id": request["id"], "error": {"code": -32602, "message": error}}).to_string().into()))?;
                            continue;
                        }
                        request["params"]["approvalPolicy"] = permissions.approval_policy.clone();
                        request["params"]["approvalsReviewer"] =
                            json!(permissions.approvals_reviewer);
                        request["params"]["sandboxPolicy"] = thread.sandbox.clone();
                        if let Some(params) = request["params"].as_object_mut() {
                            params.remove("permissions");
                            params.remove("cwd");
                            params.remove("runtimeWorkspaceRoots");
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
                if let Some(event) = managed_turn_event(&response, &verified, &mut active_turns)
                    && let Err(error) = control(config, "agent.codex.turn", event)
                {
                    eprintln!("wmcterm: Codex turn telemetry: {error}");
                }
                if response.get("method").is_none()
                    && response.get("id").is_some()
                    && pending.remove(&response["id"].to_string())
                    && response.get("error").is_none()
                {
                    let result = permissions.verify(&response).and_then(|()| {
                        let settings = VerifiedThread::from_response(&response)?;
                        let thread = response.pointer("/result/thread/id").and_then(Value::as_str)
                            .ok_or_else(|| BridgeError::Protocol("missing resumed thread ID".into()))?;
                        if !settings.ephemeral {
                            control(config, "agent.codex.ready", json!({"session_id": thread, "mode": permissions.run_mode(), "cwd": response["result"]["cwd"]}))?;
                        }
                        verified.insert(thread.to_owned(), settings);
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

fn managed_turn_event(
    notification: &Value,
    verified: &HashMap<String, VerifiedThread>,
    active: &mut HashMap<String, String>,
) -> Option<Value> {
    if notification.get("id").is_some() {
        return None;
    }
    let method = notification["method"].as_str()?;
    if !matches!(method, "turn/started" | "turn/completed") {
        return None;
    }
    let params = &notification["params"];
    let thread = params["threadId"].as_str()?;
    let turn = params["turn"]["id"].as_str()?.trim();
    if turn.is_empty()
        || verified
            .get(thread)
            .is_none_or(|settings| settings.ephemeral)
    {
        return None;
    }
    let event = if method == "turn/started" {
        active.insert(thread.to_owned(), turn.to_owned());
        json!({"event": "tool_use", "tool": "Codex turn"})
    } else {
        if active.get(thread).map(String::as_str) != Some(turn) {
            return None;
        }
        let event = match params["turn"]["status"].as_str()? {
            "completed" => json!({"event": "stop", "success": true, "message": null}),
            "interrupted" => {
                json!({"event": "turn_complete", "continuing": false, "message": null})
            }
            "failed" => json!({"event": "turn_failed", "message": null}),
            _ => return None,
        };
        active.remove(thread);
        event
    };
    Some(json!({"session_id": thread, "turn_id": turn, "agent_event": event}))
}

struct VerifiedThread {
    ephemeral: bool,
    sandbox: Value,
    cwd: String,
    runtime_workspace_roots: Value,
}

impl VerifiedThread {
    fn from_response(response: &Value) -> Result<Self, BridgeError> {
        let result = &response["result"];
        let cwd = result["cwd"]
            .as_str()
            .filter(|cwd| !cwd.is_empty())
            .ok_or_else(|| BridgeError::Protocol("missing verified thread cwd".into()))?;
        let roots = result
            .get("runtimeWorkspaceRoots")
            .cloned()
            .unwrap_or_else(|| json!([]));
        if !roots.as_array().is_some_and(|roots| {
            roots
                .iter()
                .all(|root| root.as_str().is_some_and(|root| !root.is_empty()))
        }) {
            return Err(BridgeError::Protocol(
                "invalid runtime workspace roots".into(),
            ));
        }
        Ok(Self {
            ephemeral: result
                .pointer("/thread/ephemeral")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            sandbox: result["sandbox"].clone(),
            cwd: cwd.to_owned(),
            runtime_workspace_roots: roots,
        })
    }

    fn validate_turn(&self, request: &Value) -> Result<(), &'static str> {
        let params = request["params"]
            .as_object()
            .ok_or("Turn start requires object params")?;
        for (field, expected) in [
            ("cwd", json!(self.cwd)),
            (
                "runtimeWorkspaceRoots",
                self.runtime_workspace_roots.clone(),
            ),
        ] {
            if params
                .get(field)
                .is_some_and(|value| !value.is_null() && *value != expected)
            {
                return Err(
                    "Managed Codex cwd and workspace roots are fixed after thread verification; restart through wmcterm to change them",
                );
            }
        }
        Ok(())
    }
}

fn validate_settings_update(request: &Value) -> Result<(), &'static str> {
    let params = request["params"]
        .as_object()
        .ok_or("Settings update requires object params")?;
    // The launch profile owns permissions for the lifetime of this proxy.
    // Changing cwd also invalidates its cached workspace sandbox roots.
    if [
        "approvalPolicy",
        "approvalsReviewer",
        "sandboxPolicy",
        "permissions",
        "cwd",
        "runtimeWorkspaceRoots",
    ]
    .iter()
    .any(|field| params.get(*field).is_some_and(|value| !value.is_null()))
    {
        return Err(
            "Managed Codex permissions and cwd are fixed at launch; restart through wmcterm with the desired mode and workspace",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn managed_turn_notifications_track_verified_threads_and_matching_turns() {
        let mut verified = HashMap::from([(
            "thread".into(),
            VerifiedThread {
                ephemeral: false,
                sandbox: json!({"type": "workspaceWrite"}),
                cwd: "/work".into(),
                runtime_workspace_roots: json!([]),
            },
        )]);
        let mut active = HashMap::new();
        verified.insert(
            "reviewer".into(),
            VerifiedThread {
                ephemeral: true,
                sandbox: json!({"type": "workspaceWrite"}),
                cwd: "/work".into(),
                runtime_workspace_roots: json!([]),
            },
        );
        assert!(managed_turn_event(&json!({"method": "turn/started", "params": {"threadId": "reviewer", "turn": {"id": "1"}}}), &verified, &mut active).is_none());
        let notification = |method: &str, thread: &str, turn: &str, status: &str| json!({"method": method, "params": {"threadId": thread, "turn": {"id": turn, "status": status}}});
        assert!(
            managed_turn_event(
                &notification("turn/started", "other", "1", "inProgress"),
                &verified,
                &mut active
            )
            .is_none()
        );
        let started = managed_turn_event(
            &notification("turn/started", "thread", "1", "inProgress"),
            &verified,
            &mut active,
        )
        .unwrap();
        assert_eq!(started["agent_event"]["event"], "tool_use");
        assert_eq!(started["session_id"], "thread");
        managed_turn_event(
            &notification("turn/started", "thread", "2", "inProgress"),
            &verified,
            &mut active,
        )
        .unwrap();
        assert!(
            managed_turn_event(
                &notification("turn/completed", "thread", "1", "completed"),
                &verified,
                &mut active
            )
            .is_none()
        );
        let completed = managed_turn_event(
            &notification("turn/completed", "thread", "2", "completed"),
            &verified,
            &mut active,
        )
        .unwrap();
        assert_eq!(completed["agent_event"]["event"], "stop");
        assert_eq!(completed["agent_event"]["success"], true);
        assert!(
            managed_turn_event(
                &notification("turn/completed", "thread", "2", "completed"),
                &verified,
                &mut active
            )
            .is_none()
        );
        for (status, expected) in [("interrupted", "turn_complete"), ("failed", "turn_failed")] {
            managed_turn_event(
                &notification("turn/started", "thread", "3", "inProgress"),
                &verified,
                &mut active,
            )
            .unwrap();
            assert_eq!(
                managed_turn_event(
                    &notification("turn/completed", "thread", "3", status),
                    &verified,
                    &mut active
                )
                .unwrap()["agent_event"]["event"],
                expected
            );
        }
    }

    use super::*;

    #[test]
    fn proxy_rejects_missing_wrong_and_path_tokens_before_upstream_connection() {
        use tungstenite::client::IntoClientRequest;
        for (path, authorization) in [
            ("/", None),
            ("/", Some("Bearer wrong-token")),
            ("/test-token", None),
            ("/?token=test-token", Some("Bearer test-token")),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("ws://{}{path}", listener.local_addr().unwrap());
            let config = BridgeConfig {
                endpoint: "ws://127.0.0.1:1".into(),
                control_socket: PathBuf::new(),
                workspace_id: "workspace".into(),
                root_surface_id: "surface".into(),
                codex_program: "codex".into(),
                workspace_cwd: std::env::temp_dir(),
                max_auto_depth: 2,
            };
            let worker = std::thread::spawn(move || {
                relay(
                    listener.accept().unwrap().0,
                    "test-token",
                    &config,
                    &CodexPermissions::extract(&[]).unwrap().0,
                    &AtomicBool::new(false),
                )
            });
            let mut request = endpoint.into_client_request().unwrap();
            if let Some(value) = authorization {
                request
                    .headers_mut()
                    .insert("authorization", value.parse().unwrap());
            }
            match connect(request).unwrap_err() {
                tungstenite::Error::Http(response) => assert_eq!(response.status(), 403),
                error => panic!("unexpected handshake error: {error}"),
            }
            assert!(worker.join().unwrap().is_err());
        }
    }

    #[test]
    fn proxy_verifies_resumes_and_blocks_turns_until_permissions_match() {
        for mismatch in [false, true] {
            let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("ws://{}", listener.local_addr().unwrap());
            let socket_path = std::env::temp_dir()
                .join(format!("wmcterm-permission-{}.sock", uuid::Uuid::new_v4()));
            let ready_count = Arc::new(AtomicU64::new(0));
            let observed = Arc::clone(&ready_count);
            let turn_count = Arc::new(AtomicU64::new(0));
            let observed_turns = Arc::clone(&turn_count);
            let _control = wmcterm_ipc::UnixServer::spawn(&socket_path, move |request| {
                if request.method == "agent.codex.turn" {
                    assert_eq!(request.params["surface_id"], "surface");
                    assert_eq!(request.params["session_id"], "thread");
                    assert_eq!(request.params["turn_id"], "turn");
                    let index = observed_turns.fetch_add(1, Ordering::Relaxed);
                    assert_eq!(
                        request.params["agent_event"]["event"],
                        if index == 0 { "tool_use" } else { "stop" }
                    );
                    return wmcterm_ipc::Response::success(request.id, json!({"ok": true}));
                }
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
            let provider = std::thread::spawn(move || {
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
                socket
                    .send(Message::Text(
                        json!({"id": request["id"], "result": {
                            "thread": {"id": "thread"}, "approvalPolicy": "on-request",
                            "cwd": "/worktree", "runtimeWorkspaceRoots": ["/worktree"],
                            "approvalsReviewer": if mismatch { "user" } else { "auto_review" },
                            "sandbox": {"type": "workspaceWrite", "writableRoots": ["/worktree"]}
                        }})
                        .to_string()
                        .into(),
                    ))
                    .unwrap();
                if !mismatch {
                    let Message::Text(text) = socket.read().unwrap() else {
                        panic!("expected reviewer resume")
                    };
                    let reviewer: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(reviewer["method"], "thread/resume");
                    socket.send(Message::Text(json!({"id": reviewer["id"], "result": {
                            "thread": {"id": "reviewer", "ephemeral": true},
                            "approvalPolicy": "on-request", "approvalsReviewer": "auto_review",
                            "cwd": "/worktree", "runtimeWorkspaceRoots": ["/worktree"],
                            "sandbox": {"type": "workspaceWrite", "writableRoots": ["/worktree"]}
                        }}).to_string().into())).unwrap();
                    let Message::Text(text) = socket.read().unwrap() else {
                        panic!("expected ordinary settings update")
                    };
                    let settings: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(settings["method"], "thread/settings/update");
                    assert_eq!(
                        settings["params"],
                        json!({"threadId": "thread", "model": "test-model"})
                    );
                    socket
                        .send(Message::Text(
                            json!({"id": settings["id"], "result": {}})
                                .to_string()
                                .into(),
                        ))
                        .unwrap();
                    let Message::Text(text) = socket.read().unwrap() else {
                        panic!("expected turn")
                    };
                    let turn: Value = serde_json::from_str(&text).unwrap();
                    assert_eq!(turn["method"], "turn/start");
                    assert_eq!(turn["params"]["sandboxPolicy"]["type"], "workspaceWrite");
                    assert_eq!(turn["params"]["approvalsReviewer"], "auto_review");
                    assert!(turn["params"].get("cwd").is_none());
                    assert!(turn["params"].get("runtimeWorkspaceRoots").is_none());
                    for (method, status) in [
                        ("turn/started", "inProgress"),
                        ("turn/completed", "completed"),
                    ] {
                        socket.send(Message::Text(json!({"method": method, "params": {"threadId": "thread", "turn": {"id": "turn", "status": status}}}).to_string().into())).unwrap();
                    }
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
                    "test-token",
                    &config,
                    &permissions,
                    &relay_stop,
                )
            });
            use tungstenite::client::IntoClientRequest;
            let mut request = endpoint.into_client_request().unwrap();
            request
                .headers_mut()
                .insert("authorization", "Bearer test-token".parse().unwrap());
            let (mut client, _) = connect(request).unwrap();
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
                send_json(&mut client, &json!({"id": 14, "method": "thread/resume", "params": {"threadId": "reviewer"}})).unwrap();
                assert_eq!(
                    read_json(&mut client).unwrap().unwrap()["result"]["thread"]["ephemeral"],
                    true
                );
                for method in ["thread/settings/update", "turn/settings/update"] {
                    for (field, value) in [
                        ("approvalPolicy", json!("never")),
                        ("approvalsReviewer", json!("user")),
                        ("sandboxPolicy", json!({"type": "dangerFullAccess"})),
                        ("permissions", json!("full-access")),
                        ("cwd", json!("/other-workspace")),
                        ("runtimeWorkspaceRoots", json!(["/other-workspace"])),
                    ] {
                        let mut request = json!({"id": 10, "method": method, "params": {"threadId": "thread", "turnId": "turn"}});
                        request["params"][field] = value;
                        send_json(&mut client, &request).unwrap();
                        let rejected = read_json(&mut client).unwrap().unwrap();
                        assert_eq!(rejected["id"], 10);
                        assert_eq!(rejected["error"]["code"], -32602);
                    }
                    send_json(&mut client, &json!({"id": 11, "method": method, "params": {"threadId": "unverified", "model": "test-model"}})).unwrap();
                    assert_eq!(
                        read_json(&mut client).unwrap().unwrap()["error"]["code"],
                        -32000
                    );
                }
                send_json(&mut client, &json!({"id": 12, "method": "thread/settings/update", "params": {"threadId": "thread", "model": "test-model"}})).unwrap();
                assert_eq!(
                    read_json(&mut client).unwrap().unwrap()["result"],
                    json!({})
                );
                for (field, value) in [
                    ("cwd", json!("/other-workspace")),
                    ("runtimeWorkspaceRoots", json!(["/other-workspace"])),
                ] {
                    let mut request =
                        json!({"id": 13, "method": "turn/start", "params": {"threadId": "thread"}});
                    request["params"][field] = value;
                    send_json(&mut client, &request).unwrap();
                    assert_eq!(
                        read_json(&mut client).unwrap().unwrap()["error"]["code"],
                        -32602
                    );
                }
                send_json(&mut client, &json!({"id": 3, "method": "turn/start", "params": {"threadId": "thread", "cwd": "/worktree", "runtimeWorkspaceRoots": ["/worktree"], "sandboxPolicy": {"type": "dangerFullAccess"}}})).unwrap();
                for method in ["turn/started", "turn/completed"] {
                    assert_eq!(read_json(&mut client).unwrap().unwrap()["method"], method);
                }
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
            assert_eq!(
                turn_count.load(Ordering::Relaxed),
                if mismatch { 0 } else { 2 }
            );
        }
    }

    #[test]
    fn settings_updates_allow_nullable_noops_and_ordinary_turn_settings() {
        for method in ["thread/settings/update", "turn/settings/update"] {
            assert!(
                validate_settings_update(&json!({"method": method, "params": {
                    "threadId": "thread", "turnId": "turn", "model": "test", "effort": "high",
                    "approvalPolicy": null, "approvalsReviewer": null,
                    "sandboxPolicy": null, "permissions": null, "cwd": null
                }}))
                .is_ok()
            );
            assert!(validate_settings_update(&json!({"method": method, "params": []})).is_err());
        }
    }

    #[test]
    fn turns_preserve_verified_workspace_and_reject_malformed_overrides() {
        let response = json!({"result": {
            "cwd": "/worktree", "sandbox": {"type": "workspaceWrite"},
            "runtimeWorkspaceRoots": ["/worktree", "/shared"]
        }});
        let thread = VerifiedThread::from_response(&response).unwrap();
        for params in [
            json!({}),
            json!({"cwd": null, "runtimeWorkspaceRoots": null}),
            json!({"cwd": "/worktree", "runtimeWorkspaceRoots": ["/worktree", "/shared"]}),
        ] {
            assert!(thread.validate_turn(&json!({"params": params})).is_ok());
        }
        for params in [
            json!({"cwd": "/other"}),
            json!({"cwd": 1}),
            json!({"runtimeWorkspaceRoots": []}),
            json!({"runtimeWorkspaceRoots": ["/worktree", "/shared", "/extra"]}),
            json!({"runtimeWorkspaceRoots": "invalid"}),
            json!([]),
        ] {
            assert!(thread.validate_turn(&json!({"params": params})).is_err());
        }
        for result in [
            json!({}),
            json!({"cwd": ""}),
            json!({"cwd": "/worktree", "runtimeWorkspaceRoots": [1]}),
        ] {
            assert!(VerifiedThread::from_response(&json!({"result": result})).is_err());
        }
        let legacy =
            VerifiedThread::from_response(&json!({"result": {"cwd": "/worktree"}})).unwrap();
        assert!(
            legacy
                .validate_turn(&json!({"params": {"runtimeWorkspaceRoots": []}}))
                .is_ok()
        );
        assert!(
            legacy
                .validate_turn(&json!({"params": {"runtimeWorkspaceRoots": ["/extra"]}}))
                .is_err()
        );
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
    fn launch_profile_rejects_network_table_overrides_and_scrubs_cold_resume_config() {
        for config in [
            "sandbox_workspace_write={network_access=true}",
            "sandbox_workspace_write.network_access=true",
            "permissions={default=\"full-access\"}",
        ] {
            assert!(CodexPermissions::extract(&["-c".into(), config.into()]).is_err());
        }
        let (permissions, _) = CodexPermissions::extract(&[
            "--yolo".into(),
            "--sandbox=read-only".into(),
            "--ask-for-approval=on-request".into(),
        ])
        .unwrap();
        let mut request = json!({"method": "thread/resume", "params": {
            "threadId": "saved-full-access-thread", "cwd": "/worktree", "permissions": "saved-profile",
            "config": {
                "sandbox_workspace_write": {"network_access": true, "writable_roots": ["/private"]},
                "sandbox_workspace_write.network_access": true,
                "permissions": {"default": "full-access"},
                "permissions.default": "full-access", "sandbox_mode": "danger-full-access",
                "approval_policy": "never", "approvals_reviewer": "auto_review", "model": "test-model"
            }
        }});
        permissions.apply(&mut request).unwrap();
        assert_eq!(request["params"]["config"], json!({"model": "test-model"}));
        assert_eq!(request["params"]["sandbox"], "read-only");
        assert_eq!(request["params"]["approvalPolicy"], "on-request");
        assert!(request["params"].get("permissions").is_none());
        // A persisted full-access or auto-review profile cannot silently replace
        // an explicitly selected managed launch profile during cold restore.
        for (policy, reviewer, sandbox) in [
            ("never", "user", "readOnly"),
            ("on-request", "auto_review", "readOnly"),
            ("on-request", "user", "dangerFullAccess"),
        ] {
            assert!(
                permissions
                    .verify(&json!({"result": {"approvalPolicy": policy,
                "approvalsReviewer": reviewer, "sandbox": {"type": sandbox}}}))
                    .is_err()
            );
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
