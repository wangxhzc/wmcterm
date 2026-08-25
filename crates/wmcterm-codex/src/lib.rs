//! Experimental Codex app-server integration.
//!
//! Protocol parsing intentionally lives outside the terminal core because the
//! app-server command is experimental and may evolve independently.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;
use toml_edit::{DocumentMut, table, value};
use tungstenite::{Message, WebSocket, connect, stream::MaybeTlsStream};
use wmcterm_ipc::{Request, call};

static RPC_ID: AtomicU64 = AtomicU64::new(1);
pub const ABSOLUTE_MAX_AUTO_DEPTH: usize = 8;
pub const APPROVAL_BRIDGE_ENV: &str = "WMCTERM_CODEX_APPROVAL_BRIDGE";
pub const APPROVAL_BRIDGE_ENV_VALUE: &str = "1";
const CONNECT_ATTEMPTS: usize = 100;
const CONNECT_DELAY: Duration = Duration::from_millis(50);
const APPROVAL_POLL_DELAY: Duration = Duration::from_millis(100);
const THREAD_RECONCILE_DELAY: Duration = Duration::from_secs(1);
pub const APPROVAL_REGISTER_METHOD: &str = "agent.approval.register";
pub const APPROVAL_POLL_METHOD: &str = "agent.approval.poll";
pub const APPROVAL_CANCEL_METHOD: &str = "agent.request.cancel";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTrustState {
    NotApplicable,
    Trusted,
    Untrusted,
}

/// Return the user-level Codex configuration path used by managed launches.
pub fn user_config_path() -> Option<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| directories::BaseDirs::new().map(|dirs| dirs.home_dir().join(".codex")))
        .map(|directory| directory.join("config.toml"))
}

/// Project trust is relevant whenever Codex can discover a project-local
/// `.codex` directory. Codex requires an exact project-root entry; a trusted
/// ancestor is intentionally not treated as sufficient here.
pub fn project_trust_state(project_root: &Path) -> Result<ProjectTrustState, ProjectTrustError> {
    if !project_root.join(".codex").is_dir() {
        return Ok(ProjectTrustState::NotApplicable);
    }
    let Some(config_path) = user_config_path() else {
        return Err(ProjectTrustError::HomeUnavailable);
    };
    project_trust_state_at(project_root, &config_path)
}

fn project_trust_state_at(
    project_root: &Path,
    config_path: &Path,
) -> Result<ProjectTrustState, ProjectTrustError> {
    if !project_root.join(".codex").is_dir() {
        return Ok(ProjectTrustState::NotApplicable);
    }
    if !config_path.exists() {
        return Ok(ProjectTrustState::Untrusted);
    }
    let document = read_config(config_path)?;
    let key = project_root.to_string_lossy();
    let trusted = document
        .get("projects")
        .and_then(|projects| projects.get(key.as_ref()))
        .and_then(|project| project.get("trust_level"))
        .and_then(|trust| trust.as_str())
        == Some("trusted");
    Ok(if trusted {
        ProjectTrustState::Trusted
    } else {
        ProjectTrustState::Untrusted
    })
}

/// Persist an explicit human decision to trust one exact Codex project root.
pub fn trust_project(project_root: &Path) -> Result<(), ProjectTrustError> {
    let config_path = user_config_path().ok_or(ProjectTrustError::HomeUnavailable)?;
    trust_project_at(project_root, &config_path)
}

fn trust_project_at(project_root: &Path, config_path: &Path) -> Result<(), ProjectTrustError> {
    let mut document = if config_path.exists() {
        read_config(config_path)?
    } else {
        DocumentMut::new()
    };
    let key = project_root.to_string_lossy().into_owned();
    if !document.contains_key("projects") {
        document["projects"] = table();
    }
    if document["projects"].get(&key).is_none() {
        document["projects"][&key] = table();
    }
    document["projects"][&key]["trust_level"] = value("trusted");
    atomic_write_config(config_path, document.to_string().as_bytes())
}

#[derive(Debug, thiserror::Error)]
pub enum ProjectTrustError {
    #[error("Codex home directory is unavailable")]
    HomeUnavailable,
    #[error("could not read Codex configuration {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse Codex configuration {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml_edit::TomlError,
    },
    #[error("could not write Codex configuration {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

fn read_config(path: &Path) -> Result<DocumentMut, ProjectTrustError> {
    let contents = fs::read_to_string(path).map_err(|source| ProjectTrustError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    contents.parse().map_err(|source| ProjectTrustError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

static CONFIG_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn atomic_write_config(path: &Path, contents: &[u8]) -> Result<(), ProjectTrustError> {
    let effective_path = if path.exists() {
        fs::canonicalize(path).map_err(|source| ProjectTrustError::Write {
            path: path.to_path_buf(),
            source,
        })?
    } else {
        path.to_path_buf()
    };
    let parent = effective_path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|source| ProjectTrustError::Write {
        path: effective_path.clone(),
        source,
    })?;
    let permissions = fs::metadata(&effective_path)
        .ok()
        .map(|metadata| metadata.permissions());
    let file_name = effective_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();
    let sequence = CONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary_path = parent.join(format!(
        ".{file_name}.wmcterm-{}-{sequence}.tmp",
        std::process::id()
    ));
    let result = (|| -> std::io::Result<()> {
        let mut temporary = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)?;
        if let Some(permissions) = permissions {
            temporary.set_permissions(permissions)?;
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temporary.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
        }
        temporary.write_all(contents)?;
        temporary.sync_all()?;
        drop(temporary);
        fs::rename(&temporary_path, &effective_path)?;
        #[cfg(unix)]
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if let Err(source) = result {
        let _ = fs::remove_file(&temporary_path);
        return Err(ProjectTrustError::Write {
            path: effective_path,
            source,
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalKind {
    CommandExecution,
    FileChange,
    Permissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    Deny,
    Once,
    Always,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub jsonrpc_id: Value,
    pub method: String,
    pub kind: ApprovalKind,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub item_id: String,
    pub params: Value,
}

impl ApprovalRequest {
    pub fn parse(value: &Value) -> Option<Self> {
        let method = value.get("method")?.as_str()?;
        let kind = match method {
            "item/commandExecution/requestApproval" => ApprovalKind::CommandExecution,
            "item/fileChange/requestApproval" => ApprovalKind::FileChange,
            "item/permissions/requestApproval" => ApprovalKind::Permissions,
            _ => return None,
        };
        let jsonrpc_id = value.get("id")?.clone();
        if !jsonrpc_id.is_string() && !jsonrpc_id.is_number() {
            return None;
        }
        let params = value.get("params")?.clone();
        let thread_id = text_field(&params, &["threadId", "thread_id"])?;
        let turn_id = text_field(&params, &["turnId", "turn_id"]);
        let item_id = text_field(&params, &["approvalId", "approval_id", "itemId", "item_id"])
            .unwrap_or_else(|| request_id_string(&jsonrpc_id));
        Some(Self {
            jsonrpc_id,
            method: method.to_owned(),
            kind,
            thread_id,
            turn_id,
            item_id,
            params,
        })
    }

    pub fn registration_params(&self, config: &BridgeConfig) -> Value {
        let decisions = supported_modes(self)
            .into_iter()
            .map(|mode| serde_json::to_value(mode).expect("approval decision serializes"))
            .collect::<Vec<_>>();
        json!({
            "source": "codex_app_server",
            "workspace_id": config.workspace_id,
            "surface_id": config.root_surface_id,
            "request": {
                "jsonrpc_id": self.jsonrpc_id,
                "method": self.method,
                "kind": self.kind,
                "thread_id": self.thread_id,
                "turn_id": self.turn_id,
                "item_id": self.item_id,
                "params": bounded_json(&self.params, 0),
            },
            "decisions": decisions,
        })
    }

    pub fn response(&self, decision: ApprovalDecision) -> Value {
        let result = match self.kind {
            ApprovalKind::CommandExecution => json!({
                "decision": command_decision(&self.params, decision)
            }),
            ApprovalKind::FileChange => json!({
                "decision": file_decision(&self.params, decision)
            }),
            ApprovalKind::Permissions => permission_decision(&self.params, decision),
        };
        json!({"id": self.jsonrpc_id, "result": result})
    }

    fn controlled_response(&self, decision: ApprovalDecision) -> Value {
        if supported_modes(self).contains(&decision) {
            self.response(decision)
        } else {
            self.response(ApprovalDecision::Deny)
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalControlStatus {
    Pending { approval_id: String },
    Resolved { decision: ApprovalDecision },
}

pub trait ApprovalControl {
    fn register(
        &mut self,
        request: &ApprovalRequest,
        config: &BridgeConfig,
    ) -> Result<ApprovalControlStatus, BridgeError>;
    fn poll(&mut self, approval_id: &str) -> Result<ApprovalControlStatus, BridgeError>;
    fn cancel(&mut self, approval_id: &str, reason: &str) -> Result<(), BridgeError>;
}

#[derive(Debug, Default)]
pub struct ApprovalBridge {
    pending: HashMap<String, ApprovalRequest>,
}

impl ApprovalBridge {
    pub fn begin(
        &mut self,
        request: ApprovalRequest,
        config: &BridgeConfig,
        control: &mut impl ApprovalControl,
    ) -> Result<Option<Value>, BridgeError> {
        match control.register(&request, config)? {
            ApprovalControlStatus::Pending { approval_id } => {
                if self.pending.insert(approval_id.clone(), request).is_some() {
                    return Err(BridgeError::Protocol(format!(
                        "duplicate pending approval id {approval_id}"
                    )));
                }
                Ok(None)
            }
            ApprovalControlStatus::Resolved { decision } => {
                Ok(Some(request.controlled_response(decision)))
            }
        }
    }

    pub fn pending_ids(&self) -> Vec<String> {
        self.pending.keys().cloned().collect()
    }

    pub fn poll(
        &mut self,
        approval_id: &str,
        control: &mut impl ApprovalControl,
    ) -> Result<Option<Value>, BridgeError> {
        let request = self
            .pending
            .get(approval_id)
            .ok_or_else(|| BridgeError::Protocol("approval bridge id is not pending".into()))?;
        match control.poll(approval_id)? {
            ApprovalControlStatus::Pending {
                approval_id: returned,
            } => {
                if returned != approval_id {
                    return Err(BridgeError::Protocol("approval poll id changed".into()));
                }
                Ok(None)
            }
            ApprovalControlStatus::Resolved { decision } => {
                let response = request.controlled_response(decision);
                self.pending.remove(approval_id);
                Ok(Some(response))
            }
        }
    }

    pub fn fail_closed(&mut self, approval_id: &str) -> Option<Value> {
        self.pending
            .remove(approval_id)
            .map(|request| request.response(ApprovalDecision::Deny))
    }

    /// Reconcile a request that another app-server client (normally the root
    /// Codex TUI) has already answered. The app-server broadcasts an explicit
    /// `serverRequest/resolved` notification, so this does not infer approval
    /// from terminal input or item activity.
    pub fn acknowledge_external_resolution(
        &mut self,
        thread_id: &str,
        request_id: &Value,
        control: &mut impl ApprovalControl,
    ) -> Result<bool, BridgeError> {
        let Some(approval_id) = self.pending.iter().find_map(|(approval_id, request)| {
            (request.thread_id == thread_id && request.jsonrpc_id == *request_id)
                .then(|| approval_id.clone())
        }) else {
            return Ok(false);
        };
        control.cancel(&approval_id, "resolved_by_codex_client")?;
        self.pending.remove(&approval_id);
        Ok(true)
    }

    /// Reconcile an approval after Codex starts or completes the exact item.
    /// Some app-server versions deliver `serverRequest/resolved` only to the
    /// client that answered the request, while item lifecycle notifications
    /// are visible to the watcher connection as well.
    pub fn acknowledge_item_progress(
        &mut self,
        thread_id: &str,
        turn_id: &str,
        item_id: &str,
        control: &mut impl ApprovalControl,
    ) -> Result<bool, BridgeError> {
        let Some(approval_id) = self.pending.iter().find_map(|(approval_id, request)| {
            (request.thread_id == thread_id
                && request.turn_id.as_deref() == Some(turn_id)
                && request.item_id == item_id)
                .then(|| approval_id.clone())
        }) else {
            return Ok(false);
        };
        control.cancel(&approval_id, "item_progressed_in_codex")?;
        self.pending.remove(&approval_id);
        Ok(true)
    }

    pub fn cancel_all(&mut self, control: &mut impl ApprovalControl, reason: &str) {
        let pending = std::mem::take(&mut self.pending);
        for (approval_id, _) in pending {
            if let Err(error) = control.cancel(&approval_id, reason) {
                eprintln!("wmcterm: could not cancel approval {approval_id}: {error}");
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentThread {
    pub thread_id: String,
    pub parent_thread_id: String,
    pub depth: usize,
    pub cwd: Option<PathBuf>,
    pub nickname: Option<String>,
    pub role: Option<String>,
}

impl SubagentThread {
    pub fn from_thread_value(thread: &Value) -> Option<Self> {
        let thread_id = thread.get("id")?.as_str()?.to_owned();
        let spawn = thread
            .pointer("/source/subAgent/thread_spawn")
            .or_else(|| thread.pointer("/source/subAgent/threadSpawn"))
            .or_else(|| thread.pointer("/source/subagent/thread_spawn"))
            .or_else(|| thread.pointer("/source/subagent/threadSpawn"))?;
        let parent_thread_id = spawn
            .get("parent_thread_id")
            .or_else(|| spawn.get("parentThreadId"))?
            .as_str()?
            .to_owned();
        let depth = spawn.get("depth").and_then(Value::as_u64).unwrap_or(1) as usize;
        let text = |snake: &str, camel: &str| {
            spawn
                .get(snake)
                .or_else(|| spawn.get(camel))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        Some(Self {
            thread_id,
            parent_thread_id,
            depth,
            cwd: thread.get("cwd").and_then(Value::as_str).map(PathBuf::from),
            nickname: text("agent_nickname", "agentNickname").or_else(|| {
                thread
                    .get("agentNickname")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }),
            role: text("agent_role", "agentRole").or_else(|| {
                thread
                    .get("agentRole")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }),
        })
    }

    pub fn title(&self) -> String {
        self.nickname
            .as_deref()
            .or(self.role.as_deref())
            .map(|name| format!("Codex · {name}"))
            .unwrap_or_else(|| {
                format!(
                    "Codex · agent {}",
                    &self.thread_id[..self.thread_id.len().min(8)]
                )
            })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeConfig {
    pub endpoint: String,
    pub control_socket: PathBuf,
    pub workspace_id: String,
    pub root_surface_id: String,
    pub codex_program: PathBuf,
    pub workspace_cwd: PathBuf,
    #[serde(default = "default_depth")]
    pub max_auto_depth: usize,
}

/// The subprocess arguments used by [`launch_codex_teams`]. Keeping this
/// construction separate makes the integration testable without starting a
/// real Codex process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodexLaunchPlan {
    pub app_server_args: Vec<String>,
    pub root_args: Vec<String>,
}

pub fn codex_launch_plan(config: &BridgeConfig, codex_args: &[String]) -> CodexLaunchPlan {
    codex_launch_plan_for_endpoint(config, &config.endpoint, codex_args)
}

fn codex_launch_plan_for_endpoint(
    config: &BridgeConfig,
    client_endpoint: &str,
    codex_args: &[String],
) -> CodexLaunchPlan {
    let mut root_args = vec!["--remote".to_owned(), client_endpoint.to_owned()];
    // A remote Codex TUI cannot infer the server-side workspace cwd. Give it
    // an explicit override so `/resume` sends a cwd filter to `thread/list`,
    // matching the behavior of a local `codex resume` picker. Preserve a cwd
    // supplied by the caller as the more specific choice.
    if !has_explicit_cwd(codex_args) {
        root_args.extend([
            "--cd".to_owned(),
            config.workspace_cwd.to_string_lossy().into_owned(),
        ]);
    }
    root_args.extend_from_slice(codex_args);
    CodexLaunchPlan {
        app_server_args: vec![
            "app-server".to_owned(),
            "--listen".to_owned(),
            config.endpoint.clone(),
        ],
        root_args,
    }
}

fn has_explicit_cwd(args: &[String]) -> bool {
    args.iter()
        .take_while(|argument| argument.as_str() != "--")
        .any(|argument| {
            argument == "-C"
                || argument == "--cd"
                || argument.starts_with("--cd=")
                || argument
                    .strip_prefix("-C")
                    .is_some_and(|value| !value.is_empty())
        })
}

const fn default_depth() -> usize {
    2
}

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("Codex app-server websocket error: {0}")]
    WebSocket(#[from] tungstenite::Error),
    #[error("invalid Codex app-server JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("wmcterm control error: {0}")]
    Control(String),
    #[error("could not start Codex process: {0}")]
    Process(#[from] std::io::Error),
    #[error("Codex app-server protocol error: {0}")]
    Protocol(String),
}

struct SocketApprovalControl {
    path: PathBuf,
}

impl SocketApprovalControl {
    fn request(&self, method: &str, params: Value) -> Result<ApprovalControlStatus, BridgeError> {
        let response = call(
            &self.path,
            &Request {
                id: format!("codex-approval-{}", next_rpc_id()),
                method: method.into(),
                params,
            },
        )
        .map_err(|error| BridgeError::Control(error.to_string()))?;
        if let Some(error) = response.error {
            return Err(BridgeError::Control(error.message));
        }
        parse_approval_control_status(
            response
                .result
                .as_ref()
                .ok_or_else(|| BridgeError::Control("approval response has no result".into()))?,
        )
    }

    fn cancel_request(&self, approval_id: &str, reason: &str) -> Result<(), BridgeError> {
        let response = call(
            &self.path,
            &Request {
                id: format!("codex-approval-{}", next_rpc_id()),
                method: APPROVAL_CANCEL_METHOD.into(),
                params: json!({"request_id": approval_id, "reason": reason}),
            },
        )
        .map_err(|error| BridgeError::Control(error.to_string()))?;
        if let Some(error) = response.error {
            return Err(BridgeError::Control(error.message));
        }
        Ok(())
    }
}

impl ApprovalControl for SocketApprovalControl {
    fn register(
        &mut self,
        request: &ApprovalRequest,
        config: &BridgeConfig,
    ) -> Result<ApprovalControlStatus, BridgeError> {
        self.request(
            APPROVAL_REGISTER_METHOD,
            request.registration_params(config),
        )
    }

    fn poll(&mut self, approval_id: &str) -> Result<ApprovalControlStatus, BridgeError> {
        self.request(APPROVAL_POLL_METHOD, json!({"approval_id": approval_id}))
    }

    fn cancel(&mut self, approval_id: &str, reason: &str) -> Result<(), BridgeError> {
        self.cancel_request(approval_id, reason)
    }
}

fn parse_approval_control_status(value: &Value) -> Result<ApprovalControlStatus, BridgeError> {
    match value.get("status").and_then(Value::as_str) {
        Some("pending") => value
            .get("approval_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(|approval_id| ApprovalControlStatus::Pending {
                approval_id: approval_id.to_owned(),
            })
            .ok_or_else(|| BridgeError::Control("pending approval has no approval_id".into())),
        Some("resolved") => {
            let decision = value
                .pointer("/decision/mode")
                .or_else(|| value.get("decision"))
                .cloned()
                .ok_or_else(|| BridgeError::Control("resolved approval has no decision".into()))?;
            serde_json::from_value(decision)
                .map(|decision| ApprovalControlStatus::Resolved { decision })
                .map_err(|error| {
                    BridgeError::Control(format!("invalid approval decision: {error}"))
                })
        }
        Some(status) => Err(BridgeError::Control(format!(
            "unknown approval status {status}"
        ))),
        None => Err(BridgeError::Control(
            "approval response has no status".into(),
        )),
    }
}

pub fn allocate_loopback_endpoint() -> Result<String, std::io::Error> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(format!("ws://127.0.0.1:{port}"))
}

type Socket = WebSocket<MaybeTlsStream<std::net::TcpStream>>;

pub struct CodexWatcher {
    config: BridgeConfig,
    known: HashSet<String>,
    depths: HashMap<String, usize>,
    pending: HashMap<String, Vec<SubagentThread>>,
    pending_ids: HashSet<String>,
    subscriptions: HashSet<String>,
    missing_rollouts: HashSet<String>,
    opened: HashSet<String>,
    open_attempts: HashMap<String, usize>,
    last_surface: Option<String>,
    approvals: ApprovalBridge,
    client_endpoint: Option<String>,
}

impl CodexWatcher {
    pub fn new(config: BridgeConfig) -> Self {
        Self {
            config,
            known: HashSet::new(),
            depths: HashMap::new(),
            pending: HashMap::new(),
            pending_ids: HashSet::new(),
            subscriptions: HashSet::new(),
            missing_rollouts: HashSet::new(),
            opened: HashSet::new(),
            open_attempts: HashMap::new(),
            last_surface: None,
            approvals: ApprovalBridge::default(),
            client_endpoint: None,
        }
    }

    fn with_client_endpoint(mut self, endpoint: String) -> Self {
        self.client_endpoint = Some(endpoint);
        self
    }

    pub fn run(mut self) -> Result<(), BridgeError> {
        let stop = AtomicBool::new(false);
        self.run_until(&stop)
    }

    fn run_until(&mut self, stop: &AtomicBool) -> Result<(), BridgeError> {
        let mut disconnects = 0;
        while !stop.load(Ordering::Acquire) {
            let mut socket = connect_with_retry(&self.config.endpoint, stop)?;
            match self.run_session(&mut socket, stop) {
                Ok(()) if stop.load(Ordering::Acquire) => return Ok(()),
                Ok(()) | Err(BridgeError::WebSocket(_)) | Err(BridgeError::Json(_)) => {
                    disconnects += 1;
                    if disconnects >= CONNECT_ATTEMPTS {
                        return Err(BridgeError::Protocol(
                            "Codex app-server repeatedly disconnected".into(),
                        ));
                    }
                    std::thread::sleep(CONNECT_DELAY);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn run_session(&mut self, socket: &mut Socket, stop: &AtomicBool) -> Result<(), BridgeError> {
        let result = self.run_session_inner(socket, stop);
        let reason = if stop.load(Ordering::Acquire) {
            "watcher_stopped"
        } else {
            "app_server_disconnected"
        };
        let mut control = SocketApprovalControl {
            path: self.config.control_socket.clone(),
        };
        self.approvals.cancel_all(&mut control, reason);
        result
    }

    fn run_session_inner(
        &mut self,
        socket: &mut Socket,
        stop: &AtomicBool,
    ) -> Result<(), BridgeError> {
        let initialize_id = next_rpc_id();
        send_json(
            socket,
            &json!({
                "id": initialize_id,
                "method": "initialize",
                "params": {
                    "clientInfo": {"name": "wmcterm", "title": "wmcterm", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {"experimentalApi": true}
                }
            }),
        )?;
        loop {
            let Some(value) = read_json(socket)? else {
                return Ok(());
            };
            if response_matches(&value, initialize_id) {
                ensure_success_response(&value, "initialize")?;
                break;
            }
            self.handle_server_value(socket, &value)?;
        }
        send_json(socket, &json!({"method": "initialized"}))?;
        self.subscriptions.clear();
        self.missing_rollouts.clear();
        self.load_existing_threads(socket)?;
        configure_socket_polling(socket)?;
        let mut last_thread_reconcile = Instant::now();

        while !stop.load(Ordering::Acquire) {
            self.poll_approvals(socket)?;
            if last_thread_reconcile.elapsed() >= THREAD_RECONCILE_DELAY {
                self.load_existing_threads(socket)?;
                last_thread_reconcile = Instant::now();
            }
            match read_json_poll(socket)? {
                SocketRead::Message(value) => self.handle_server_value(socket, &value)?,
                SocketRead::Idle => {}
                SocketRead::Closed => return Ok(()),
            }
        }
        Ok(())
    }

    fn load_existing_threads(&mut self, socket: &mut Socket) -> Result<(), BridgeError> {
        let mut cursor: Option<String> = None;
        loop {
            let request_id = next_rpc_id();
            send_json(
                socket,
                &json!({
                    "id": request_id,
                    "method": "thread/loaded/list",
                    "params": {"limit": 200, "cursor": cursor}
                }),
            )?;
            let response = loop {
                let Some(value) = read_json(socket)? else {
                    return Ok(());
                };
                if response_matches(&value, request_id) {
                    break value;
                }
                self.handle_server_value(socket, &value)?;
            };
            ensure_success_response(&response, "thread/loaded/list")?;
            for id in loaded_thread_ids(&response) {
                if let Err(error) = self.subscribe_thread(socket, id) {
                    eprintln!("wmcterm: Codex watcher could not resume thread {id}: {error}");
                }
            }
            cursor = response
                .pointer("/result/nextCursor")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        Ok(())
    }

    fn observe_value(&mut self, value: &Value) {
        if let Some(thread) = thread_value(value)
            && let Err(error) = self.observe(thread)
        {
            eprintln!("wmcterm: ignoring Codex thread event: {error}");
        }
    }

    fn handle_server_value(
        &mut self,
        socket: &mut Socket,
        value: &Value,
    ) -> Result<(), BridgeError> {
        if let Some(thread_id) = announced_thread_id(value) {
            self.missing_rollouts.remove(thread_id);
            if let Err(error) = self.subscribe_thread(socket, thread_id) {
                eprintln!("wmcterm: Codex watcher could not resume thread {thread_id}: {error}");
            }
        }
        if let Some((thread_id, request_id)) = resolved_server_request(value) {
            let mut control = SocketApprovalControl {
                path: self.config.control_socket.clone(),
            };
            self.approvals
                .acknowledge_external_resolution(thread_id, request_id, &mut control)?;
            return Ok(());
        }
        if let Some((thread_id, turn_id, item_id)) = progressed_item(value) {
            let mut control = SocketApprovalControl {
                path: self.config.control_socket.clone(),
            };
            self.approvals
                .acknowledge_item_progress(thread_id, turn_id, item_id, &mut control)?;
        }
        let Some(request) = ApprovalRequest::parse(value) else {
            self.observe_value(value);
            return Ok(());
        };
        let mut control = SocketApprovalControl {
            path: self.config.control_socket.clone(),
        };
        let first = self.approvals.begin(request, &self.config, &mut control);
        let response = match first {
            Ok(response) => response,
            Err(error) => {
                eprintln!("wmcterm: approval registration failed; denying request: {error}");
                ApprovalRequest::parse(value)
                    .map(|request| request.response(ApprovalDecision::Deny))
            }
        };
        if let Some(response) = response {
            send_json(socket, &response)?;
        }
        Ok(())
    }

    fn subscribe_thread(
        &mut self,
        socket: &mut Socket,
        thread_id: &str,
    ) -> Result<(), BridgeError> {
        if self.missing_rollouts.contains(thread_id) {
            return Ok(());
        }
        if !self.subscriptions.insert(thread_id.to_owned()) {
            return Ok(());
        }
        let resume_id = next_rpc_id();
        let result = (|| {
            send_json(
                socket,
                &json!({
                    "id": resume_id,
                    "method": "thread/resume",
                    "params": {"threadId": thread_id, "excludeTurns": true}
                }),
            )?;
            loop {
                let Some(value) = read_json(socket)? else {
                    return Err(BridgeError::Protocol(
                        "Codex app-server closed while resuming thread".into(),
                    ));
                };
                if response_matches(&value, resume_id) {
                    if response_has_missing_rollout(&value) {
                        self.subscriptions.remove(thread_id);
                        self.missing_rollouts.insert(thread_id.to_owned());
                        return Ok(());
                    }
                    ensure_success_response(&value, "thread/resume")?;
                    self.observe_value(&value);
                    return Ok(());
                }
                self.handle_server_value(socket, &value)?;
            }
        })();
        if result.is_err() {
            self.subscriptions.remove(thread_id);
        }
        result
    }

    fn poll_approvals(&mut self, socket: &mut Socket) -> Result<(), BridgeError> {
        let mut control = SocketApprovalControl {
            path: self.config.control_socket.clone(),
        };
        for approval_id in self.approvals.pending_ids() {
            match self.approvals.poll(&approval_id, &mut control) {
                Ok(Some(response)) => send_json(socket, &response)?,
                Ok(None) => {}
                Err(error) => {
                    eprintln!("wmcterm: approval polling failed; denying request: {error}");
                    let _ = control.cancel(&approval_id, "poll_failed");
                    if let Some(response) = self.approvals.fail_closed(&approval_id) {
                        send_json(socket, &response)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn observe(&mut self, thread: &Value) -> Result<(), BridgeError> {
        let id = match thread.get("id").and_then(Value::as_str) {
            Some(id) => id.to_owned(),
            None => return Ok(()),
        };
        if self.opened.contains(&id) {
            return Ok(());
        }
        self.known.insert(id.clone());
        let Some(child) = SubagentThread::from_thread_value(thread) else {
            self.depths.entry(id.clone()).or_insert(0);
            return self.drain(&id);
        };
        if !self.depths.contains_key(&child.parent_thread_id) {
            if !self.pending_ids.insert(child.thread_id.clone()) {
                return Ok(());
            }
            self.pending
                .entry(child.parent_thread_id.clone())
                .or_default()
                .push(child);
            return Ok(());
        }
        let child_id = child.thread_id.clone();
        let result = self.open(child);
        let drain_result = self.drain(&child_id);
        result.and(drain_result)
    }

    fn drain(&mut self, parent: &str) -> Result<(), BridgeError> {
        let Some(children) = self.pending.remove(parent) else {
            return Ok(());
        };
        for child in children {
            let id = child.thread_id.clone();
            self.pending_ids.remove(&id);
            if let Err(error) = self.open(child) {
                eprintln!("wmcterm: could not create pane for Codex thread {id}: {error}");
            }
            if let Err(error) = self.drain(&id) {
                eprintln!("wmcterm: could not drain Codex children for {id}: {error}");
            }
        }
        Ok(())
    }

    fn open(&mut self, child: SubagentThread) -> Result<(), BridgeError> {
        if self.opened.contains(&child.thread_id) {
            return Ok(());
        }
        let parent_depth = self
            .depths
            .get(&child.parent_thread_id)
            .copied()
            .unwrap_or(0);
        let depth = parent_depth.saturating_add(1).max(child.depth);
        self.depths.insert(child.thread_id.clone(), depth);
        let maximum = self.config.max_auto_depth.min(ABSOLUTE_MAX_AUTO_DEPTH);
        if depth > maximum {
            self.opened.insert(child.thread_id);
            return Ok(());
        }
        let target = self
            .last_surface
            .as_deref()
            .unwrap_or(&self.config.root_surface_id);
        let direction = if self.last_surface.is_some() {
            "down"
        } else {
            "right"
        };
        let command = resume_command(
            &self.config.codex_program,
            self.client_endpoint
                .as_deref()
                .unwrap_or(&self.config.endpoint),
            &child.thread_id,
        );
        let mut params = json!({
            "workspace": self.config.workspace_id,
            "surface": target,
            "direction": direction,
            "focus": false,
            "command": command,
            "title": child.title()
        });
        if let Some(cwd) = child.cwd.as_ref().and_then(|path| path.to_str()) {
            params["cwd"] = cwd.into();
        }
        let attempts = self
            .open_attempts
            .entry(child.thread_id.clone())
            .or_default();
        *attempts += 1;
        if *attempts > 3 {
            self.opened.insert(child.thread_id);
            return Ok(());
        }
        let response = call(
            &self.config.control_socket,
            &Request {
                id: format!("codex-team-{}", RPC_ID.fetch_add(1, Ordering::Relaxed)),
                method: "pane.split".into(),
                params,
            },
        )
        .map_err(|error| BridgeError::Control(error.to_string()))?;
        if let Some(error) = response.error {
            return Err(BridgeError::Control(error.message));
        }
        let surface = response
            .result
            .as_ref()
            .and_then(|result| result.get("surface_id"))
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::Control("pane.split did not return surface_id".into()))?;
        self.last_surface = Some(surface.to_owned());
        self.open_attempts.remove(&child.thread_id);
        self.opened.insert(child.thread_id);
        Ok(())
    }
}

fn text_field(value: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        value.get(*name).and_then(|value| match value {
            Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_owned()),
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        })
    })
}

fn request_id_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn decision_names(params: &Value) -> Option<HashSet<String>> {
    let raw = params
        .get("availableDecisions")
        .or_else(|| params.get("available_decisions"))?;
    Some(
        raw.as_array()
            .into_iter()
            .flatten()
            .filter_map(|decision| match decision {
                Value::String(name) => Some(name.clone()),
                Value::Object(object) => object.keys().next().cloned(),
                _ => None,
            })
            .collect(),
    )
}

fn available(params: &Value, name: &str) -> bool {
    decision_names(params).is_none_or(|decisions| decisions.contains(name))
}

fn command_exact_rule(params: &Value) -> Option<Value> {
    let exec = params
        .get("proposedExecpolicyAmendment")
        .or_else(|| params.get("proposed_execpolicy_amendment"));
    if available(params, "acceptWithExecpolicyAmendment")
        && let Some(amendment) = exec
        && !amendment.is_null()
    {
        return Some(json!({
            "acceptWithExecpolicyAmendment": {"execpolicy_amendment": amendment}
        }));
    }
    let network = params
        .get("proposedNetworkPolicyAmendments")
        .or_else(|| params.get("proposed_network_policy_amendments"))
        .and_then(Value::as_array)
        .and_then(|items| items.first());
    if available(params, "applyNetworkPolicyAmendment")
        && let Some(amendment) = network
        && !amendment.is_null()
    {
        return Some(json!({
            "applyNetworkPolicyAmendment": {"network_policy_amendment": amendment}
        }));
    }
    None
}

fn reject_decision(params: &Value) -> Value {
    let decisions = decision_names(params).unwrap_or_default();
    if decisions.is_empty() || decisions.contains("decline") {
        json!("decline")
    } else if decisions.contains("cancel") {
        json!("cancel")
    } else {
        json!("decline")
    }
}

fn command_decision(params: &Value, mode: ApprovalDecision) -> Value {
    match mode {
        ApprovalDecision::Deny => reject_decision(params),
        ApprovalDecision::Once => {
            if available(params, "accept") {
                json!("accept")
            } else {
                reject_decision(params)
            }
        }
        ApprovalDecision::Always => {
            if let Some(amendment) = command_exact_rule(params) {
                amendment
            } else if available(params, "acceptForSession") {
                json!("acceptForSession")
            } else {
                reject_decision(params)
            }
        }
    }
}

fn supported_modes(request: &ApprovalRequest) -> Vec<ApprovalDecision> {
    let mut modes = vec![ApprovalDecision::Deny];
    match request.kind {
        ApprovalKind::Permissions => {
            if available(&request.params, "accept") {
                modes.push(ApprovalDecision::Once);
            }
            if available(&request.params, "acceptForSession") {
                modes.push(ApprovalDecision::Always);
            }
        }
        ApprovalKind::CommandExecution => {
            if available(&request.params, "accept") {
                modes.push(ApprovalDecision::Once);
            }
            if available(&request.params, "acceptForSession")
                || command_exact_rule(&request.params).is_some()
            {
                modes.push(ApprovalDecision::Always);
            }
        }
        ApprovalKind::FileChange => {
            if available(&request.params, "accept") {
                modes.push(ApprovalDecision::Once);
            }
            if available(&request.params, "acceptForSession") {
                modes.push(ApprovalDecision::Always);
            }
        }
    }
    modes
}

fn file_decision(params: &Value, mode: ApprovalDecision) -> Value {
    match mode {
        ApprovalDecision::Deny => reject_decision(params),
        ApprovalDecision::Once => {
            if available(params, "accept") {
                json!("accept")
            } else {
                reject_decision(params)
            }
        }
        ApprovalDecision::Always => {
            if available(params, "acceptForSession") {
                json!("acceptForSession")
            } else {
                reject_decision(params)
            }
        }
    }
}

fn permission_decision(params: &Value, mode: ApprovalDecision) -> Value {
    let persistent = matches!(mode, ApprovalDecision::Always);
    let permissions = if mode == ApprovalDecision::Deny {
        json!({})
    } else {
        params
            .get("permissions")
            .cloned()
            .unwrap_or_else(|| json!({}))
    };
    json!({
        "permissions": permissions,
        "scope": if persistent { "session" } else { "turn" },
    })
}

fn bounded_json(value: &Value, depth: usize) -> Value {
    const MAX_DEPTH: usize = 5;
    const MAX_ITEMS: usize = 50;
    const MAX_CHARS: usize = 4096;
    if depth > MAX_DEPTH {
        return Value::Null;
    }
    match value {
        Value::String(text) => Value::String(text.chars().take(MAX_CHARS).collect()),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(MAX_ITEMS)
                .map(|item| bounded_json(item, depth + 1))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .take(MAX_ITEMS)
                .map(|(key, value)| (key.clone(), bounded_json(value, depth + 1)))
                .collect(),
        ),
        scalar => scalar.clone(),
    }
}

fn connect_with_retry(endpoint: &str, stop: &AtomicBool) -> Result<Socket, BridgeError> {
    let mut last_error = None;
    for _ in 0..CONNECT_ATTEMPTS {
        if stop.load(Ordering::Acquire) {
            return Err(BridgeError::Protocol("watcher stopped".into()));
        }
        match connect(endpoint) {
            Ok((socket, _)) => return Ok(socket),
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(CONNECT_DELAY);
    }
    Err(BridgeError::WebSocket(
        last_error.unwrap_or_else(|| tungstenite::Error::ConnectionClosed),
    ))
}

fn wait_for_app_server(endpoint: &str, process: &mut Child) -> Result<(), BridgeError> {
    let mut last_error = None;
    for _ in 0..CONNECT_ATTEMPTS {
        if let Some(status) = process.try_wait()? {
            return Err(BridgeError::Protocol(format!(
                "Codex app-server exited before becoming ready: {status}"
            )));
        }
        match connect(endpoint) {
            Ok((mut socket, _)) => {
                let _ = socket.close(None);
                return Ok(());
            }
            Err(error) => last_error = Some(error),
        }
        std::thread::sleep(CONNECT_DELAY);
    }
    Err(BridgeError::WebSocket(
        last_error.unwrap_or(tungstenite::Error::ConnectionClosed),
    ))
}

fn next_rpc_id() -> u64 {
    RPC_ID.fetch_add(1, Ordering::Relaxed)
}

fn response_matches(value: &Value, id: u64) -> bool {
    value.get("id").and_then(Value::as_u64) == Some(id)
}

fn ensure_success_response(value: &Value, operation: &str) -> Result<(), BridgeError> {
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| error.to_string());
        return Err(BridgeError::Protocol(format!(
            "{operation} failed: {message}"
        )));
    }
    if value.get("result").is_none() {
        return Err(BridgeError::Protocol(format!(
            "{operation} response is missing result"
        )));
    }
    Ok(())
}

fn response_has_missing_rollout(value: &Value) -> bool {
    value
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| {
            message
                .to_ascii_lowercase()
                .contains("no rollout found for thread id")
        })
}

/// Launch a private Codex app-server, attach the root TUI and watch subagent
/// threads until the root process exits.
pub fn launch_codex_teams(
    config: BridgeConfig,
    codex_args: &[String],
) -> Result<ExitStatus, BridgeError> {
    let plan = codex_launch_plan(&config, codex_args);
    let mut app_server_command = managed_codex_command(&config.codex_program);
    let mut app_server = app_server_command
        .args(&plan.app_server_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // This auxiliary server must not inject delayed diagnostics into the
        // interactive Codex PTY and overwrite its input area.
        .stderr(Stdio::null())
        .spawn()?;
    if let Err(error) = wait_for_app_server(&config.endpoint, &mut app_server) {
        let _ = app_server.kill();
        let _ = app_server.wait();
        return Err(error);
    }
    let watcher_config = config.clone();
    let watcher_client_endpoint = config.endpoint.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let watcher_stop = Arc::clone(&stop);
    let watcher = match std::thread::Builder::new()
        .name("wmcterm-codex-watcher".into())
        .spawn(move || {
            CodexWatcher::new(watcher_config)
                .with_client_endpoint(watcher_client_endpoint)
                .run_until(&watcher_stop)
        }) {
        Ok(watcher) => watcher,
        Err(error) => {
            let _ = app_server.kill();
            let _ = app_server.wait();
            return Err(BridgeError::Process(error));
        }
    };

    let status = managed_codex_command(&config.codex_program)
        .args(&plan.root_args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status();
    stop.store(true, Ordering::Release);
    let _ = app_server.kill();
    let _ = app_server.wait();
    if let Ok(Err(error)) = watcher.join() {
        eprintln!("wmcterm: Codex subagent watcher stopped: {error}");
    }
    Ok(status?)
}

fn managed_codex_command(program: &Path) -> Command {
    let mut command = Command::new(program);
    command.env(APPROVAL_BRIDGE_ENV, APPROVAL_BRIDGE_ENV_VALUE);
    command
}

fn send_json(socket: &mut Socket, value: &Value) -> Result<(), BridgeError> {
    socket.send(Message::Text(serde_json::to_string(value)?.into()))?;
    Ok(())
}

fn read_json(socket: &mut Socket) -> Result<Option<Value>, BridgeError> {
    loop {
        match socket.read()? {
            Message::Text(text) => return Ok(Some(serde_json::from_str(&text)?)),
            Message::Binary(bytes) => return Ok(Some(serde_json::from_slice(&bytes)?)),
            Message::Close(_) => return Ok(None),
            Message::Ping(payload) => socket.send(Message::Pong(payload))?,
            _ => {}
        }
    }
}

enum SocketRead {
    Message(Value),
    Idle,
    Closed,
}

fn configure_socket_polling(socket: &mut Socket) -> Result<(), BridgeError> {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream
            .set_read_timeout(Some(APPROVAL_POLL_DELAY))
            .map_err(BridgeError::Process),
        _ => Err(BridgeError::Protocol(
            "Codex approval polling requires a local ws:// endpoint".into(),
        )),
    }
}

fn read_json_poll(socket: &mut Socket) -> Result<SocketRead, BridgeError> {
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                return Ok(SocketRead::Message(serde_json::from_str(&text)?));
            }
            Ok(Message::Binary(bytes)) => {
                return Ok(SocketRead::Message(serde_json::from_slice(&bytes)?));
            }
            Ok(Message::Close(_)) => return Ok(SocketRead::Closed),
            Ok(Message::Ping(payload)) => socket.send(Message::Pong(payload))?,
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return Ok(SocketRead::Idle);
            }
            Err(error) => return Err(BridgeError::WebSocket(error)),
        }
    }
}

fn loaded_thread_ids(value: &Value) -> Vec<&str> {
    value
        .pointer("/result/data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn thread_value(value: &Value) -> Option<&Value> {
    value
        .pointer("/params/thread")
        .or_else(|| value.pointer("/result/thread"))
}

fn announced_thread_id(value: &Value) -> Option<&str> {
    value
        .get("method")?
        .as_str()?
        .starts_with("thread/")
        .then(|| {
            value
                .pointer("/params/thread/id")
                .or_else(|| value.pointer("/params/threadId"))?
                .as_str()
        })?
}

fn resolved_server_request(value: &Value) -> Option<(&str, &Value)> {
    (value.get("method")?.as_str()? == "serverRequest/resolved").then_some((
        value.pointer("/params/threadId")?.as_str()?,
        value.pointer("/params/requestId")?,
    ))
}

fn progressed_item(value: &Value) -> Option<(&str, &str, &str)> {
    matches!(
        value.get("method")?.as_str()?,
        "item/started" | "item/completed"
    )
    .then_some((
        value.pointer("/params/threadId")?.as_str()?,
        value.pointer("/params/turnId")?.as_str()?,
        value.pointer("/params/item/id")?.as_str()?,
    ))
}

pub fn resume_command(program: &Path, endpoint: &str, thread_id: &str) -> String {
    [
        shell_arg(program.to_string_lossy().as_ref()),
        "resume".into(),
        "--remote".into(),
        shell_arg(endpoint),
        shell_arg(thread_id),
    ]
    .join(" ")
}

fn shell_arg(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_./:=+".contains(character))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct FakeApprovalControl {
        statuses: VecDeque<ApprovalControlStatus>,
        registrations: Vec<Value>,
        polls: Vec<String>,
        cancellations: Vec<(String, String)>,
    }

    impl ApprovalControl for FakeApprovalControl {
        fn register(
            &mut self,
            request: &ApprovalRequest,
            config: &BridgeConfig,
        ) -> Result<ApprovalControlStatus, BridgeError> {
            self.registrations.push(request.registration_params(config));
            self.statuses
                .pop_front()
                .ok_or_else(|| BridgeError::Control("fake status exhausted".into()))
        }

        fn poll(&mut self, approval_id: &str) -> Result<ApprovalControlStatus, BridgeError> {
            self.polls.push(approval_id.to_owned());
            self.statuses
                .pop_front()
                .ok_or_else(|| BridgeError::Control("fake status exhausted".into()))
        }

        fn cancel(&mut self, approval_id: &str, reason: &str) -> Result<(), BridgeError> {
            self.cancellations
                .push((approval_id.to_owned(), reason.to_owned()));
            Ok(())
        }
    }

    fn test_config(endpoint: String) -> BridgeConfig {
        BridgeConfig {
            endpoint,
            control_socket: std::env::temp_dir().join("wmcterm-codex-missing.sock"),
            workspace_id: "workspace".into(),
            root_surface_id: "surface".into(),
            codex_program: "codex".into(),
            workspace_cwd: PathBuf::from("/tmp/project"),
            max_auto_depth: default_depth(),
        }
    }

    fn read_server_json(socket: &mut WebSocket<std::net::TcpStream>) -> serde_json::Value {
        loop {
            match socket.read().unwrap() {
                Message::Text(text) => return serde_json::from_str(&text).unwrap(),
                Message::Binary(bytes) => return serde_json::from_slice(&bytes).unwrap(),
                Message::Ping(bytes) => socket.send(Message::Pong(bytes)).unwrap(),
                _ => {}
            }
        }
    }

    fn serve_initialized_session(
        stream: std::net::TcpStream,
        notification: Option<Value>,
        stop: Option<&AtomicBool>,
    ) {
        let mut socket = tungstenite::accept(stream).unwrap();
        let initialize = read_server_json(&mut socket);
        let initialize_id = initialize["id"].as_u64().unwrap();
        socket
            .send(Message::Text(
                json!({"id": initialize_id, "result": {}})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        assert_eq!(read_server_json(&mut socket)["method"], "initialized");
        let loaded = read_server_json(&mut socket);
        assert_eq!(loaded["method"], "thread/loaded/list");
        if let Some(notification) = notification {
            socket
                .send(Message::Text(notification.to_string().into()))
                .unwrap();
            let resume = read_server_json(&mut socket);
            assert_eq!(resume["method"], "thread/resume");
            socket
                .send(Message::Text(
                    json!({"id": resume["id"], "result": {"thread": {"id": "root-from-notification"}}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
        }
        socket
            .send(Message::Text(
                json!({"id": loaded["id"], "result": {"data": [], "nextCursor": null}})
                    .to_string()
                    .into(),
            ))
            .unwrap();
        if let Some(stop) = stop {
            stop.store(true, Ordering::Release);
        }
        socket.close(None).unwrap();
    }

    #[test]
    fn parses_subagent_spawn_from_generated_protocol_shape() {
        let value = json!({
            "id": "child-1",
            "cwd": "/tmp/project",
            "source": {"subAgent": {"thread_spawn": {
                "parent_thread_id": "root-1", "depth": 1,
                "agent_nickname": "researcher", "agent_role": "explorer"
            }}}
        });
        let child = SubagentThread::from_thread_value(&value).unwrap();
        assert_eq!(child.thread_id, "child-1");
        assert_eq!(child.parent_thread_id, "root-1");
        assert_eq!(child.title(), "Codex · researcher");
    }

    #[test]
    fn recognizes_camel_case_compatibility_shape() {
        let value = json!({
            "id": "child",
            "source": {"subagent": {"threadSpawn": {
                "parentThreadId": "root", "agentRole": "worker"
            }}}
        });
        assert_eq!(
            SubagentThread::from_thread_value(&value)
                .unwrap()
                .role
                .as_deref(),
            Some("worker")
        );
    }

    #[test]
    fn resume_command_quotes_untrusted_parts() {
        let command = resume_command(
            Path::new("/opt/Codex CLI/codex"),
            "ws://127.0.0.1:1234",
            "id'1",
        );
        assert!(command.contains("'/opt/Codex CLI/codex'"));
        assert!(command.contains("'id'\\''1'"));
    }

    #[test]
    fn allocates_a_loopback_websocket_endpoint() {
        let endpoint = allocate_loopback_endpoint().unwrap();
        assert!(endpoint.starts_with("ws://127.0.0.1:"));
        assert!(endpoint.rsplit(':').next().unwrap().parse::<u16>().unwrap() > 0);
    }

    #[test]
    fn launch_plan_connects_both_processes_to_the_private_endpoint() {
        let config = test_config("ws://127.0.0.1:43123".into());
        let plan = codex_launch_plan(
            &config,
            &["--model".into(), "gpt-test".into(), "prompt text".into()],
        );
        assert_eq!(
            plan.app_server_args,
            ["app-server", "--listen", "ws://127.0.0.1:43123"]
        );
        assert_eq!(
            plan.root_args,
            [
                "--remote",
                "ws://127.0.0.1:43123",
                "--cd",
                "/tmp/project",
                "--model",
                "gpt-test",
                "prompt text"
            ]
        );
        assert!(!plan.root_args.iter().any(|arg| arg == "app-server"));
    }

    #[test]
    fn launch_plan_preserves_an_explicit_workspace_directory() {
        let config = test_config("ws://127.0.0.1:43123".into());
        for args in [
            vec!["--cd".into(), "/work/override".into()],
            vec!["--cd=/work/override".into()],
            vec!["-C".into(), "/work/override".into()],
            vec!["-C/work/override".into()],
        ] {
            let plan = codex_launch_plan(&config, &args);
            assert_eq!(
                plan.root_args
                    .iter()
                    .filter(|argument| matches!(argument.as_str(), "--cd" | "-C"))
                    .count(),
                usize::from(matches!(args[0].as_str(), "--cd" | "-C"))
            );
            assert!(plan.root_args.ends_with(&args));
            assert!(
                !plan
                    .root_args
                    .windows(2)
                    .any(|pair| pair == ["--cd", "/tmp/project"])
            );
        }
    }

    #[test]
    fn cwd_like_prompt_after_option_terminator_does_not_disable_filtering() {
        let config = test_config("ws://127.0.0.1:43123".into());
        let plan = codex_launch_plan(&config, &["--".into(), "--cd=prompt".into()]);
        assert!(
            plan.root_args
                .windows(2)
                .any(|pair| pair == ["--cd", "/tmp/project"])
        );
    }

    #[test]
    fn managed_codex_processes_mark_hooks_as_bridge_owned() {
        let command = managed_codex_command(Path::new("codex"));
        assert!(command.get_envs().any(|(name, value)| {
            name == APPROVAL_BRIDGE_ENV
                && value.is_some_and(|value| value == APPROVAL_BRIDGE_ENV_VALUE)
        }));
    }

    #[cfg(unix)]
    #[test]
    fn waits_for_delayed_app_server_websocket_readiness() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let _ = socket.read();
        });
        let mut process = Command::new("sh").args(["-c", "sleep 2"]).spawn().unwrap();

        wait_for_app_server(&endpoint, &mut process).unwrap();

        let _ = process.kill();
        let _ = process.wait();
        server.join().unwrap();
    }

    #[test]
    fn initialization_error_is_not_treated_as_success() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let request = read_server_json(&mut socket);
            socket
                .send(Message::Text(
                    json!({"id": request["id"], "error": {"code": -1, "message": "denied"}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
        });
        let mut watcher = CodexWatcher::new(test_config(endpoint));
        let error = watcher.run_until(&AtomicBool::new(false)).unwrap_err();
        assert!(error.to_string().contains("initialize failed: denied"));
        server.join().unwrap();
    }

    #[test]
    fn websocket_reconnects_and_replays_loaded_list_subscription() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let (first, _) = listener.accept().unwrap();
            serve_initialized_session(first, None, None);
            let (second, _) = listener.accept().unwrap();
            serve_initialized_session(
                second,
                Some(json!({
                    "method": "thread/started",
                    "params": {"thread": {"id": "root-from-notification"}}
                })),
                Some(&server_stop),
            );
        });
        let mut watcher = CodexWatcher::new(test_config(endpoint));
        watcher.run_until(&stop).unwrap();
        assert_eq!(watcher.depths.get("root-from-notification"), Some(&0));
        server.join().unwrap();
    }

    #[test]
    fn loaded_threads_are_resumed_for_approval_delivery() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let initialize = read_server_json(&mut socket);
            socket
                .send(Message::Text(
                    json!({"id": initialize["id"], "result": {}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            assert_eq!(read_server_json(&mut socket)["method"], "initialized");
            let loaded = read_server_json(&mut socket);
            assert_eq!(loaded["method"], "thread/loaded/list");
            socket
                .send(Message::Text(
                    json!({
                        "id": loaded["id"],
                        "result": {"data": ["existing-thread"], "nextCursor": null}
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();

            let resume = read_server_json(&mut socket);
            assert_eq!(resume["method"], "thread/resume");
            assert_eq!(resume["params"]["threadId"], "existing-thread");
            assert_eq!(resume["params"]["excludeTurns"], true);
            socket
                .send(Message::Text(
                    json!({
                        "id": resume["id"],
                        "result": {"thread": {"id": "existing-thread"}}
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();
            server_stop.store(true, Ordering::Release);
        });

        let mut watcher = CodexWatcher::new(test_config(endpoint));
        watcher.run_until(&stop).unwrap();
        assert_eq!(watcher.depths.get("existing-thread"), Some(&0));
        server.join().unwrap();
    }

    #[test]
    fn missing_rollout_is_not_retried_until_thread_is_announced() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let initialize = read_server_json(&mut socket);
            socket
                .send(Message::Text(
                    json!({"id": initialize["id"], "result": {}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            assert_eq!(read_server_json(&mut socket)["method"], "initialized");

            let loaded = read_server_json(&mut socket);
            socket
                .send(Message::Text(
                    json!({
                        "id": loaded["id"],
                        "result": {"data": ["missing-rollout"], "nextCursor": null}
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();
            let resume = read_server_json(&mut socket);
            assert_eq!(resume["method"], "thread/resume");
            socket
                .send(Message::Text(
                    json!({
                        "id": resume["id"],
                        "error": {
                            "code": -32602,
                            "message": "no rollout found for thread id missing-rollout"
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();

            let reconciled = read_server_json(&mut socket);
            assert_eq!(reconciled["method"], "thread/loaded/list");
            socket
                .send(Message::Text(
                    json!({
                        "id": reconciled["id"],
                        "result": {"data": ["missing-rollout"], "nextCursor": null}
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();

            socket
                .get_mut()
                .set_read_timeout(Some(Duration::from_millis(150)))
                .unwrap();
            assert!(matches!(
                socket.read(),
                Err(tungstenite::Error::Io(error))
                    if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)
            ));
            socket.get_mut().set_read_timeout(None).unwrap();

            socket
                .send(Message::Text(
                    json!({
                        "method": "thread/status/changed",
                        "params": {"threadId": "missing-rollout", "status": {"type": "idle"}}
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();
            let resumed_after_announcement = read_server_json(&mut socket);
            assert_eq!(resumed_after_announcement["method"], "thread/resume");
            assert_eq!(
                resumed_after_announcement["params"]["threadId"],
                "missing-rollout"
            );
            socket
                .send(Message::Text(
                    json!({
                        "id": resumed_after_announcement["id"],
                        "result": {"thread": {"id": "missing-rollout"}}
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();
            server_stop.store(true, Ordering::Release);
        });

        let mut watcher = CodexWatcher::new(test_config(endpoint));
        watcher.run_until(&stop).unwrap();
        assert_eq!(watcher.depths.get("missing-rollout"), Some(&0));
        server.join().unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn resume_selected_thread_is_resumed_and_approval_is_bridged() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let control_path = std::env::temp_dir().join(format!(
            "wmcterm-codex-resume-approval-{}-{}.sock",
            std::process::id(),
            next_rpc_id()
        ));
        let _control_server = wmcterm_ipc::UnixServer::spawn(&control_path, |request| {
            assert_eq!(request.method, APPROVAL_REGISTER_METHOD);
            wmcterm_ipc::Response::success(
                request.id,
                json!({"status": "resolved", "decision": {"mode": "once"}}),
            )
        })
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let initialize = read_server_json(&mut socket);
            socket
                .send(Message::Text(
                    json!({"id": initialize["id"], "result": {}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            assert_eq!(read_server_json(&mut socket)["method"], "initialized");
            let loaded = read_server_json(&mut socket);
            socket
                .send(Message::Text(
                    json!({"id": loaded["id"], "result": {"data": [], "nextCursor": null}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "method": "thread/status/changed",
                        "params": {"threadId": "new-root", "status": {"type": "idle"}}
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();

            let resume = read_server_json(&mut socket);
            assert_eq!(resume["method"], "thread/resume");
            assert_eq!(resume["params"]["threadId"], "new-root");
            assert_eq!(resume["params"]["excludeTurns"], true);
            socket
                .send(Message::Text(
                    json!({"id": resume["id"], "result": {"thread": {"id": "new-root"}}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "id": 77,
                        "method": "item/commandExecution/requestApproval",
                        "params": {
                            "threadId": "new-root",
                            "turnId": "resumed-turn",
                            "itemId": "command-after-resume",
                            "availableDecisions": ["accept", "decline"]
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();
            assert_eq!(
                read_server_json(&mut socket),
                json!({"id": 77, "result": {"decision": "accept"}})
            );
            server_stop.store(true, Ordering::Release);
        });

        let mut config = test_config(endpoint);
        config.control_socket = control_path;
        let mut watcher = CodexWatcher::new(config);
        watcher.run_until(&stop).unwrap();
        assert!(watcher.subscriptions.contains("new-root"));
        server.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn loaded_thread_reconciliation_recovers_a_missed_resume_notification() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let control_path = std::env::temp_dir().join(format!(
            "wmcterm-codex-reconcile-approval-{}-{}.sock",
            std::process::id(),
            next_rpc_id()
        ));
        let _control_server = wmcterm_ipc::UnixServer::spawn(&control_path, |request| {
            wmcterm_ipc::Response::success(
                request.id,
                json!({"status": "resolved", "decision": {"mode": "once"}}),
            )
        })
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            let initialize = read_server_json(&mut socket);
            socket
                .send(Message::Text(
                    json!({"id": initialize["id"], "result": {}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            assert_eq!(read_server_json(&mut socket)["method"], "initialized");
            let initial_list = read_server_json(&mut socket);
            socket
                .send(Message::Text(
                    json!({"id": initial_list["id"], "result": {"data": [], "nextCursor": null}})
                        .to_string()
                        .into(),
                ))
                .unwrap();

            let reconciled_list = read_server_json(&mut socket);
            assert_eq!(reconciled_list["method"], "thread/loaded/list");
            socket
                .send(Message::Text(
                    json!({
                        "id": reconciled_list["id"],
                        "result": {"data": ["missed-resume-root"], "nextCursor": null}
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();
            let resume = read_server_json(&mut socket);
            assert_eq!(resume["method"], "thread/resume");
            assert_eq!(resume["params"]["threadId"], "missed-resume-root");
            socket
                .send(Message::Text(
                    json!({"id": resume["id"], "result": {"thread": {"id": "missed-resume-root"}}})
                        .to_string()
                        .into(),
                ))
                .unwrap();
            socket
                .send(Message::Text(
                    json!({
                        "id": 78,
                        "method": "item/commandExecution/requestApproval",
                        "params": {
                            "threadId": "missed-resume-root",
                            "itemId": "reconciled-command",
                            "availableDecisions": ["accept", "decline"]
                        }
                    })
                    .to_string()
                    .into(),
                ))
                .unwrap();
            assert_eq!(
                read_server_json(&mut socket),
                json!({"id": 78, "result": {"decision": "accept"}})
            );
            server_stop.store(true, Ordering::Release);
        });

        let mut config = test_config(endpoint);
        config.control_socket = control_path;
        CodexWatcher::new(config).run_until(&stop).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn duplicate_out_of_order_children_are_queued_once() {
        let mut watcher = CodexWatcher::new(test_config("ws://127.0.0.1:1".into()));
        let child = json!({
            "id": "child",
            "source": {"subAgent": {"thread_spawn": {"parent_thread_id": "root"}}}
        });
        watcher.observe(&child).unwrap();
        watcher.observe(&child).unwrap();
        assert_eq!(watcher.pending["root"].len(), 1);
        assert_eq!(watcher.pending_ids.len(), 1);
    }

    #[test]
    fn pane_failure_does_not_prevent_sibling_depth_processing() {
        let mut watcher = CodexWatcher::new(test_config("ws://127.0.0.1:1".into()));
        for id in ["one", "two"] {
            watcher
                .observe(&json!({
                    "id": id,
                    "source": {"subAgent": {"thread_spawn": {"parent_thread_id": "root"}}}
                }))
                .unwrap();
        }
        watcher.observe(&json!({"id": "root"})).unwrap();
        assert_eq!(watcher.depths.get("one"), Some(&1));
        assert_eq!(watcher.depths.get("two"), Some(&1));
        assert_eq!(watcher.open_attempts.get("one"), Some(&1));
        assert_eq!(watcher.open_attempts.get("two"), Some(&1));
    }

    #[test]
    fn configured_depth_is_capped_by_explicit_absolute_maximum() {
        let mut config = test_config("ws://127.0.0.1:1".into());
        config.max_auto_depth = usize::MAX;
        let mut watcher = CodexWatcher::new(config);
        watcher.observe(&json!({"id": "root"})).unwrap();
        watcher
            .observe(&json!({
                "id": "too-deep",
                "source": {"subAgent": {"thread_spawn": {
                    "parent_thread_id": "root", "depth": ABSOLUTE_MAX_AUTO_DEPTH + 1
                }}}
            }))
            .unwrap();
        assert!(watcher.opened.contains("too-deep"));
        assert!(!watcher.open_attempts.contains_key("too-deep"));
    }

    #[test]
    fn recognizes_all_codex_approval_requests_and_preserves_routing_ids() {
        for (method, kind) in [
            (
                "item/commandExecution/requestApproval",
                ApprovalKind::CommandExecution,
            ),
            ("item/fileChange/requestApproval", ApprovalKind::FileChange),
            (
                "item/permissions/requestApproval",
                ApprovalKind::Permissions,
            ),
        ] {
            let request = ApprovalRequest::parse(&json!({
                "id": 91,
                "method": method,
                "params": {
                    "threadId": "thread-a", "turnId": "turn-a",
                    "itemId": "item-a", "approvalId": "approval-a"
                }
            }))
            .unwrap();
            assert_eq!(request.kind, kind);
            assert_eq!(request.jsonrpc_id, json!(91));
            assert_eq!(request.item_id, "approval-a");
            assert_eq!(request.response(ApprovalDecision::Deny)["id"], 91);
        }
    }

    #[test]
    fn approval_state_machine_registers_polls_and_returns_codex_response() {
        let request = ApprovalRequest::parse(&json!({
            "id": "rpc-a",
            "method": "item/commandExecution/requestApproval",
            "params": {
                "threadId": "thread-a", "itemId": "item-a",
                "command": "cargo test",
                "availableDecisions": ["accept", "acceptForSession", "decline"]
            }
        }))
        .unwrap();
        let mut fake = FakeApprovalControl {
            statuses: VecDeque::from([
                ApprovalControlStatus::Pending {
                    approval_id: "wmc-a".into(),
                },
                ApprovalControlStatus::Resolved {
                    decision: ApprovalDecision::Always,
                },
            ]),
            registrations: Vec::new(),
            polls: Vec::new(),
            cancellations: Vec::new(),
        };
        let mut bridge = ApprovalBridge::default();
        assert!(
            bridge
                .begin(request, &test_config("ws://localhost".into()), &mut fake)
                .unwrap()
                .is_none()
        );
        let response = bridge.poll("wmc-a", &mut fake).unwrap().unwrap();
        assert_eq!(
            response,
            json!({"id": "rpc-a", "result": {"decision": "acceptForSession"}})
        );
        assert_eq!(fake.polls, ["wmc-a"]);
        assert_eq!(fake.registrations[0]["source"], "codex_app_server");
        assert_eq!(fake.registrations[0]["request"]["item_id"], "item-a");
        assert_eq!(
            fake.registrations[0]["decisions"],
            json!(["deny", "once", "always"])
        );
    }

    #[test]
    fn concurrent_approvals_do_not_block_thread_events_and_disconnect_cancels_all() {
        let make_request = |id: &str| {
            ApprovalRequest::parse(&json!({
                "id": id,
                "method": "item/fileChange/requestApproval",
                "params": {"threadId": "thread-a", "itemId": id}
            }))
            .unwrap()
        };
        let mut fake = FakeApprovalControl {
            statuses: VecDeque::from([
                ApprovalControlStatus::Pending {
                    approval_id: "approval:one".into(),
                },
                ApprovalControlStatus::Pending {
                    approval_id: "approval:two".into(),
                },
            ]),
            registrations: Vec::new(),
            polls: Vec::new(),
            cancellations: Vec::new(),
        };
        let config = test_config("ws://localhost".into());
        let mut bridge = ApprovalBridge::default();
        assert!(
            bridge
                .begin(make_request("one"), &config, &mut fake)
                .unwrap()
                .is_none()
        );
        assert!(
            bridge
                .begin(make_request("two"), &config, &mut fake)
                .unwrap()
                .is_none()
        );
        assert_eq!(bridge.pending_ids().len(), 2);

        let mut watcher = CodexWatcher::new(config);
        watcher.approvals = bridge;
        watcher
            .observe(&json!({"id": "thread-during-approval"}))
            .unwrap();
        assert_eq!(watcher.depths.get("thread-during-approval"), Some(&0));

        watcher
            .approvals
            .cancel_all(&mut fake, "app_server_disconnected");
        fake.cancellations.sort();
        assert_eq!(
            fake.cancellations,
            [
                ("approval:one".into(), "app_server_disconnected".into()),
                ("approval:two".into(), "app_server_disconnected".into()),
            ]
        );
        assert!(watcher.approvals.pending_ids().is_empty());
    }

    #[test]
    fn codex_tui_resolution_cancels_the_matching_notification_request() {
        let request = ApprovalRequest::parse(&json!({
            "id": 91,
            "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "thread-a", "turnId": "turn-a", "itemId": "item-a"}
        }))
        .unwrap();
        let mut fake = FakeApprovalControl {
            statuses: VecDeque::from([ApprovalControlStatus::Pending {
                approval_id: "approval:wmcterm".into(),
            }]),
            registrations: Vec::new(),
            polls: Vec::new(),
            cancellations: Vec::new(),
        };
        let mut bridge = ApprovalBridge::default();
        bridge
            .begin(request, &test_config("ws://localhost".into()), &mut fake)
            .unwrap();

        let notification = json!({
            "method": "serverRequest/resolved",
            "params": {"threadId": "thread-a", "requestId": 91}
        });
        let (thread_id, request_id) = resolved_server_request(&notification).unwrap();
        assert!(
            bridge
                .acknowledge_external_resolution(thread_id, request_id, &mut fake)
                .unwrap()
        );
        assert!(bridge.pending_ids().is_empty());
        assert_eq!(
            fake.cancellations,
            [("approval:wmcterm".into(), "resolved_by_codex_client".into())]
        );
    }

    #[test]
    fn external_resolution_does_not_cancel_a_different_request() {
        let request = ApprovalRequest::parse(&json!({
            "id": "rpc-a",
            "method": "item/fileChange/requestApproval",
            "params": {"threadId": "thread-a", "turnId": "turn-a", "itemId": "item-a"}
        }))
        .unwrap();
        let mut fake = FakeApprovalControl {
            statuses: VecDeque::from([ApprovalControlStatus::Pending {
                approval_id: "approval:wmcterm".into(),
            }]),
            registrations: Vec::new(),
            polls: Vec::new(),
            cancellations: Vec::new(),
        };
        let mut bridge = ApprovalBridge::default();
        bridge
            .begin(request, &test_config("ws://localhost".into()), &mut fake)
            .unwrap();

        assert!(
            !bridge
                .acknowledge_external_resolution("thread-a", &json!("rpc-b"), &mut fake)
                .unwrap()
        );
        assert_eq!(bridge.pending_ids(), ["approval:wmcterm"]);
        assert!(fake.cancellations.is_empty());
    }

    #[test]
    fn item_progress_cancels_the_matching_notification_request() {
        let request = ApprovalRequest::parse(&json!({
            "id": 92,
            "method": "item/commandExecution/requestApproval",
            "params": {"threadId": "thread-a", "turnId": "turn-a", "itemId": "item-a"}
        }))
        .unwrap();
        let mut fake = FakeApprovalControl {
            statuses: VecDeque::from([ApprovalControlStatus::Pending {
                approval_id: "approval:wmcterm".into(),
            }]),
            registrations: Vec::new(),
            polls: Vec::new(),
            cancellations: Vec::new(),
        };
        let mut bridge = ApprovalBridge::default();
        bridge
            .begin(request, &test_config("ws://localhost".into()), &mut fake)
            .unwrap();

        let notification = json!({
            "method": "item/started",
            "params": {
                "threadId": "thread-a",
                "turnId": "turn-a",
                "item": {"type": "commandExecution", "id": "item-a"}
            }
        });
        let (thread_id, turn_id, item_id) = progressed_item(&notification).unwrap();
        assert!(
            bridge
                .acknowledge_item_progress(thread_id, turn_id, item_id, &mut fake)
                .unwrap()
        );
        assert!(bridge.pending_ids().is_empty());
        assert_eq!(
            fake.cancellations,
            [("approval:wmcterm".into(), "item_progressed_in_codex".into())]
        );
    }

    #[test]
    fn item_progress_requires_the_exact_thread_turn_and_item() {
        let notification = json!({
            "method": "item/completed",
            "params": {
                "threadId": "thread-a",
                "turnId": "turn-a",
                "item": {"type": "fileChange", "id": "item-a"}
            }
        });
        assert_eq!(
            progressed_item(&notification),
            Some(("thread-a", "turn-a", "item-a"))
        );
        assert!(
            progressed_item(&json!({
                "method": "item/updated",
                "params": notification["params"].clone()
            }))
            .is_none()
        );
    }

    #[test]
    fn decision_mapping_never_treats_unknown_or_bypass_as_approval() {
        let command = json!({"availableDecisions": ["accept", "decline"]});
        assert_eq!(command_decision(&command, ApprovalDecision::Once), "accept");
        assert_eq!(
            command_decision(&command, ApprovalDecision::Deny),
            "decline"
        );
        let amendable_command = json!({
            "availableDecisions": ["accept", "acceptWithExecpolicyAmendment", "decline"],
            "proposedExecpolicyAmendment": {"path":"/tmp/a"}
        });
        assert_eq!(
            command_decision(&amendable_command, ApprovalDecision::Always),
            json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":{"path":"/tmp/a"}}})
        );
        let network_rule_command = json!({
            "availableDecisions": ["accept", "applyNetworkPolicyAmendment", "decline"],
            "proposedNetworkPolicyAmendments": [
                {"destination":"8.8.8.8","protocol":"tcp"}
            ]
        });
        assert_eq!(
            command_decision(&network_rule_command, ApprovalDecision::Always),
            json!({"applyNetworkPolicyAmendment":{"network_policy_amendment":{"destination":"8.8.8.8","protocol":"tcp"}}})
        );
        assert!(serde_json::from_value::<ApprovalDecision>(json!("bypass")).is_err());

        let file = json!({"availableDecisions": ["accept", "acceptForSession", "decline"]});
        assert_eq!(
            file_decision(&file, ApprovalDecision::Always),
            "acceptForSession"
        );

        let permissions = json!({"permissions": {"network": {"enabled": true}}});
        assert_eq!(
            permission_decision(&permissions, ApprovalDecision::Once),
            json!({"permissions": {"network": {"enabled": true}}, "scope": "turn"})
        );
        assert_eq!(
            permission_decision(&permissions, ApprovalDecision::Deny),
            json!({"permissions": {}, "scope": "turn"})
        );

        let file_request = ApprovalRequest::parse(&json!({
            "id": 7,
            "method": "item/fileChange/requestApproval",
            "params": {"threadId": "t", "itemId": "i", "availableDecisions": ["accept", "decline"]}
        }))
        .unwrap();
        assert_eq!(
            file_request.controlled_response(ApprovalDecision::Always),
            json!({"id": 7, "result": {"decision": "decline"}})
        );
    }

    #[test]
    fn command_mode_always_exposes_only_exact_session_decision() {
        let request = ApprovalRequest::parse(&json!({
            "id": 8,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "threadId": "thread-a",
                "itemId": "item-a",
                "availableDecisions": ["accept", "decline"]
            }
        }))
        .unwrap();
        assert_eq!(
            supported_modes(&request),
            vec![ApprovalDecision::Deny, ApprovalDecision::Once]
        );

        assert!(
            parse_approval_control_status(&json!({"status":"resolved","decision":{"mode":"all"}}))
                .is_err()
        );
    }

    #[test]
    fn command_mode_always_prefers_exact_rule_when_present() {
        let request = ApprovalRequest::parse(&json!({
            "id": 9,
            "method": "item/commandExecution/requestApproval",
            "params": {
                "threadId": "thread-a",
                "itemId": "item-a",
                "availableDecisions": [
                    "accept",
                    "acceptForSession",
                    "acceptWithExecpolicyAmendment",
                    "decline"
                ],
                "proposedExecpolicyAmendment": {"path":"/tmp/allowlist-rule"},
                "acceptForSession": true
            }
        }))
        .unwrap();
        assert_eq!(
            command_decision(&request.params, ApprovalDecision::Always),
            json!({"acceptWithExecpolicyAmendment":{"execpolicy_amendment":{"path":"/tmp/allowlist-rule"}}})
        );
        assert_eq!(
            supported_modes(&request),
            vec![
                ApprovalDecision::Deny,
                ApprovalDecision::Once,
                ApprovalDecision::Always
            ]
        );
    }

    fn trust_test_directory(name: &str) -> PathBuf {
        let sequence = CONFIG_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "wmcterm-codex-trust-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn project_trust_requires_an_exact_project_entry() {
        let root = trust_test_directory("exact");
        let project = root.join("parent/project");
        fs::create_dir_all(project.join(".codex")).unwrap();
        let config = root.join("home/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(
            &config,
            format!(
                "[projects.\"{}\"]\ntrust_level = \"trusted\"\n",
                project.parent().unwrap().display()
            ),
        )
        .unwrap();

        assert_eq!(
            project_trust_state_at(&project, &config).unwrap(),
            ProjectTrustState::Untrusted
        );
        trust_project_at(&project, &config).unwrap();
        assert_eq!(
            project_trust_state_at(&project, &config).unwrap(),
            ProjectTrustState::Trusted
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_trust_update_preserves_existing_toml_and_permissions() {
        let root = trust_test_directory("preserve");
        let project = root.join("project with spaces");
        fs::create_dir_all(project.join(".codex")).unwrap();
        let config = root.join("home/config.toml");
        fs::create_dir_all(config.parent().unwrap()).unwrap();
        fs::write(&config, "# keep this comment\nmodel = \"gpt-test\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
        }

        trust_project_at(&project, &config).unwrap();
        let updated = fs::read_to_string(&config).unwrap();
        assert!(updated.starts_with("# keep this comment\nmodel = \"gpt-test\"\n"));
        assert!(updated.contains(&format!("[projects.\"{}\"]", project.display())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&config).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_codex_config_is_never_overwritten() {
        let root = trust_test_directory("malformed");
        let project = root.join("project");
        fs::create_dir_all(project.join(".codex")).unwrap();
        let config = root.join("config.toml");
        let invalid = "[projects.\n";
        fs::write(&config, invalid).unwrap();

        assert!(trust_project_at(&project, &config).is_err());
        assert_eq!(fs::read_to_string(&config).unwrap(), invalid);
        fs::remove_dir_all(root).unwrap();
    }
}
