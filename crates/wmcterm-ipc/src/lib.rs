use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

mod input;
pub use input::{InputField, InputFormRequest};

pub const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_CONNECTIONS: usize = 16;

fn encode_request(request: &Request) -> Result<Vec<u8>, IpcError> {
    let mut bytes = serde_json::to_vec(request)?;
    if bytes.len() >= MAX_REQUEST_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "IPC request exceeds the 1 MiB frame limit",
        )
        .into());
    }
    bytes.push(b'\n');
    Ok(bytes)
}

fn read_frame(reader: impl std::io::BufRead, limit: usize) -> std::io::Result<String> {
    use std::io::BufRead;
    let mut line = String::new();
    reader.take(limit as u64 + 1).read_line(&mut line)?;
    if line.len() > limit || !line.ends_with('\n') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "IPC frame exceeds its limit or is incomplete",
        ));
    }
    Ok(line)
}

struct ConnectionPermit(Arc<std::sync::atomic::AtomicUsize>);

impl ConnectionPermit {
    fn acquire(active: &Arc<std::sync::atomic::AtomicUsize>) -> Option<Self> {
        use std::sync::atomic::Ordering;
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_CONNECTIONS).then_some(count + 1)
            })
            .ok()?;
        Some(Self(active.clone()))
    }
}

impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
}

impl Response {
    pub fn success(id: String, result: Value) -> Self {
        Self {
            id,
            result: Some(result),
            error: None,
        }
    }
    pub fn error(id: String, code: i32, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Stable wire value. Unknown request kinds can pass through older wmcterm
/// versions without a serde failure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentRequestKind(pub String);

impl AgentRequestKind {
    pub const PERMISSION: &'static str = "permission";
    pub const INPUT: &'static str = "input";
    pub const CONFIRMATION: &'static str = "confirmation";

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentChoice {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInteractionRequest {
    pub request_id: String,
    pub surface_id: String,
    pub lifecycle_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid_start_time: Option<u64>,
    pub kind: AgentRequestKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<AgentChoice>,
    /// Absolute Unix time in milliseconds. `None` means the producer did not
    /// impose a timeout; wmcterm may still apply its own policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_unix_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentResolution {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approved: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum AgentReplyStatus {
    Resolved { resolution: AgentResolution },
    Cancelled { reason: String },
    Expired,
    ProcessExited,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentInteractionReply {
    pub request_id: String,
    pub surface_id: String,
    pub lifecycle_id: String,
    pub resolved_at_unix_ms: u64,
    #[serde(flatten)]
    pub status: AgentReplyStatus,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterOutcome {
    Inserted,
    DuplicatePending,
    AlreadyCompleted(AgentInteractionReply),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompleteOutcome {
    Completed(AgentInteractionReply),
    AlreadyCompleted(AgentInteractionReply),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProcessScope {
    pub surface_id: String,
    pub lifecycle_id: String,
    pub agent_pid: Option<u32>,
    pub pid_start_time: Option<u64>,
}

impl AgentProcessScope {
    fn matches(&self, request: &AgentInteractionRequest) -> bool {
        self.surface_id == request.surface_id
            && self.lifecycle_id == request.lifecycle_id
            && self
                .agent_pid
                .is_none_or(|pid| request.agent_pid == Some(pid))
            && self
                .pid_start_time
                .is_none_or(|start| request.pid_start_time == Some(start))
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PendingAgentRequestError {
    #[error("pending agent request capacity must be greater than zero")]
    ZeroCapacity,
    #[error("pending agent request capacity {capacity} has been reached")]
    CapacityExceeded { capacity: usize },
    #[error("agent request id must not be empty")]
    EmptyRequestId,
    #[error("agent request surface and lifecycle must not be empty")]
    EmptyScope,
    #[error("agent request kind must not be empty")]
    EmptyKind,
    #[error("request id `{0}` was reused with different content")]
    ConflictingDuplicate(String),
    #[error("agent request `{0}` is not pending")]
    NotPending(String),
    #[error("pending agent request lock is poisoned")]
    Poisoned,
}

#[derive(Debug)]
struct PendingState {
    pending: HashMap<String, AgentInteractionRequest>,
    order: VecDeque<String>,
    completed: HashMap<String, CompletedRecord>,
    completed_order: VecDeque<String>,
}

#[derive(Debug)]
struct CompletedRecord {
    request: AgentInteractionRequest,
    reply: AgentInteractionReply,
}

/// Thread-safe in-process rendezvous for UI, IPC, and hook worker threads. It
/// never blocks waiting for a human response; callers poll/list or receive the
/// returned wire reply and deliver it through their chosen transport.
#[derive(Debug, Clone)]
pub struct PendingAgentRequests {
    state: Arc<Mutex<PendingState>>,
    capacity: usize,
    history_capacity: usize,
}

impl PendingAgentRequests {
    pub fn new(capacity: usize) -> Result<Self, PendingAgentRequestError> {
        if capacity == 0 {
            return Err(PendingAgentRequestError::ZeroCapacity);
        }
        Ok(Self {
            state: Arc::new(Mutex::new(PendingState {
                pending: HashMap::new(),
                order: VecDeque::new(),
                completed: HashMap::new(),
                completed_order: VecDeque::new(),
            })),
            capacity,
            history_capacity: capacity.saturating_mul(4).max(64),
        })
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> Result<usize, PendingAgentRequestError> {
        Ok(self.lock()?.pending.len())
    }

    pub fn is_empty(&self) -> Result<bool, PendingAgentRequestError> {
        Ok(self.len()? == 0)
    }

    pub fn register(
        &self,
        request: AgentInteractionRequest,
    ) -> Result<RegisterOutcome, PendingAgentRequestError> {
        validate_agent_request(&request)?;
        let mut state = self.lock()?;
        if let Some(existing) = state.pending.get(&request.request_id) {
            return if existing == &request {
                Ok(RegisterOutcome::DuplicatePending)
            } else {
                Err(PendingAgentRequestError::ConflictingDuplicate(
                    request.request_id,
                ))
            };
        }
        if let Some(completed) = state.completed.get(&request.request_id) {
            return if completed.request == request {
                Ok(RegisterOutcome::AlreadyCompleted(completed.reply.clone()))
            } else {
                Err(PendingAgentRequestError::ConflictingDuplicate(
                    request.request_id,
                ))
            };
        }
        if state.pending.len() >= self.capacity {
            return Err(PendingAgentRequestError::CapacityExceeded {
                capacity: self.capacity,
            });
        }
        state.order.push_back(request.request_id.clone());
        state.pending.insert(request.request_id.clone(), request);
        Ok(RegisterOutcome::Inserted)
    }

    pub fn get(
        &self,
        request_id: &str,
    ) -> Result<Option<AgentInteractionRequest>, PendingAgentRequestError> {
        Ok(self.lock()?.pending.get(request_id).cloned())
    }

    pub fn list(&self) -> Result<Vec<AgentInteractionRequest>, PendingAgentRequestError> {
        let state = self.lock()?;
        Ok(state
            .order
            .iter()
            .filter_map(|id| state.pending.get(id).cloned())
            .collect())
    }

    pub fn completed_reply(
        &self,
        request_id: &str,
    ) -> Result<Option<AgentInteractionReply>, PendingAgentRequestError> {
        Ok(self
            .lock()?
            .completed
            .get(request_id)
            .map(|record| record.reply.clone()))
    }

    pub fn resolve(
        &self,
        request_id: &str,
        resolution: AgentResolution,
        now_unix_ms: u64,
    ) -> Result<CompleteOutcome, PendingAgentRequestError> {
        self.complete(
            request_id,
            AgentReplyStatus::Resolved { resolution },
            now_unix_ms,
            true,
        )
    }

    pub fn cancel(
        &self,
        request_id: &str,
        reason: impl Into<String>,
        now_unix_ms: u64,
    ) -> Result<CompleteOutcome, PendingAgentRequestError> {
        self.complete(
            request_id,
            AgentReplyStatus::Cancelled {
                reason: reason.into(),
            },
            now_unix_ms,
            false,
        )
    }

    pub fn expire(
        &self,
        now_unix_ms: u64,
    ) -> Result<Vec<AgentInteractionReply>, PendingAgentRequestError> {
        self.complete_matching(now_unix_ms, AgentReplyStatus::Expired, |request| {
            request
                .deadline_unix_ms
                .is_some_and(|deadline| deadline <= now_unix_ms)
        })
    }

    pub fn process_exited(
        &self,
        scope: &AgentProcessScope,
        now_unix_ms: u64,
    ) -> Result<Vec<AgentInteractionReply>, PendingAgentRequestError> {
        self.complete_matching(now_unix_ms, AgentReplyStatus::ProcessExited, |request| {
            scope.matches(request)
        })
    }

    fn complete(
        &self,
        request_id: &str,
        status: AgentReplyStatus,
        now_unix_ms: u64,
        honor_deadline: bool,
    ) -> Result<CompleteOutcome, PendingAgentRequestError> {
        let mut state = self.lock()?;
        if let Some(completed) = state.completed.get(request_id) {
            return Ok(CompleteOutcome::AlreadyCompleted(completed.reply.clone()));
        }
        let request = state
            .pending
            .remove(request_id)
            .ok_or_else(|| PendingAgentRequestError::NotPending(request_id.to_owned()))?;
        state.order.retain(|id| id != request_id);
        let status = if honor_deadline
            && request
                .deadline_unix_ms
                .is_some_and(|deadline| deadline <= now_unix_ms)
        {
            AgentReplyStatus::Expired
        } else {
            status
        };
        let reply = reply_for(&request, status, now_unix_ms);
        self.remember_completion(&mut state, request, reply.clone());
        Ok(CompleteOutcome::Completed(reply))
    }

    fn complete_matching(
        &self,
        now_unix_ms: u64,
        status: AgentReplyStatus,
        matches: impl Fn(&AgentInteractionRequest) -> bool,
    ) -> Result<Vec<AgentInteractionReply>, PendingAgentRequestError> {
        let mut state = self.lock()?;
        let ids: Vec<_> = state
            .order
            .iter()
            .filter(|id| state.pending.get(*id).is_some_and(&matches))
            .cloned()
            .collect();
        let mut replies = Vec::with_capacity(ids.len());
        for id in &ids {
            if let Some(request) = state.pending.remove(id) {
                let reply = reply_for(&request, status.clone(), now_unix_ms);
                self.remember_completion(&mut state, request, reply.clone());
                replies.push(reply);
            }
        }
        state.order.retain(|id| !ids.contains(id));
        Ok(replies)
    }

    fn remember_completion(
        &self,
        state: &mut PendingState,
        request: AgentInteractionRequest,
        reply: AgentInteractionReply,
    ) {
        while state.completed_order.len() >= self.history_capacity {
            if let Some(id) = state.completed_order.pop_front() {
                state.completed.remove(&id);
            }
        }
        state.completed_order.push_back(reply.request_id.clone());
        state
            .completed
            .insert(reply.request_id.clone(), CompletedRecord { request, reply });
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, PendingState>, PendingAgentRequestError> {
        self.state
            .lock()
            .map_err(|_| PendingAgentRequestError::Poisoned)
    }
}

fn validate_agent_request(
    request: &AgentInteractionRequest,
) -> Result<(), PendingAgentRequestError> {
    if request.request_id.trim().is_empty() {
        return Err(PendingAgentRequestError::EmptyRequestId);
    }
    if request.surface_id.trim().is_empty() || request.lifecycle_id.trim().is_empty() {
        return Err(PendingAgentRequestError::EmptyScope);
    }
    if request.kind.as_str().trim().is_empty() {
        return Err(PendingAgentRequestError::EmptyKind);
    }
    Ok(())
}

fn reply_for(
    request: &AgentInteractionRequest,
    status: AgentReplyStatus,
    resolved_at_unix_ms: u64,
) -> AgentInteractionReply {
    AgentInteractionReply {
        request_id: request.request_id.clone(),
        surface_id: request.surface_id.clone(),
        lifecycle_id: request.lifecycle_id.clone(),
        resolved_at_unix_ms,
        status,
    }
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("IPC I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid JSON-RPC message: {0}")]
    Json(#[from] serde_json::Error),
    #[error("IPC is not supported on this platform")]
    Unsupported,
}

pub fn default_socket_path() -> Option<PathBuf> {
    let base = BaseDirs::new()?;
    Some(
        base.runtime_dir()
            .unwrap_or_else(|| base.home_dir())
            .join("wmcterm.sock"),
    )
}

#[cfg(unix)]
mod unix {
    use std::fs;
    use std::io::{BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, JoinHandle};

    use super::{IpcError, Request, Response};

    pub struct Server {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
    }

    impl Server {
        pub fn spawn<F>(path: &Path, handler: F) -> Result<Self, IpcError>
        where
            F: Fn(Request) -> Response + Send + Sync + 'static,
        {
            if path.exists() {
                fs::remove_file(path)?;
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            let listener = UnixListener::bind(path)?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            listener.set_nonblocking(true)?;
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let handler = Arc::new(handler);
            let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let worker = thread::Builder::new()
                .name("wmcterm-ipc".into())
                .spawn(move || {
                    while !thread_stop.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((stream, _)) => {
                                if let Some(permit) = super::ConnectionPermit::acquire(&active) {
                                    let handler = handler.clone();
                                    let _ = thread::Builder::new()
                                        .name("wmcterm-ipc-client".into())
                                        .spawn(move || {
                                            let _permit = permit;
                                            handle(stream, handler.as_ref());
                                        });
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(std::time::Duration::from_millis(10));
                            }
                            Err(_) => break,
                        }
                    }
                })?;
            Ok(Self {
                stop,
                worker: Some(worker),
            })
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            // Do not unlink the pathname here. A newer wmcterm may already
            // have replaced it during a fast restart, and checking file
            // identity before unlinking still has a TOCTOU race (including
            // immediate inode reuse). The next server removes the stale path
            // before binding, so leaving it after a clean shutdown is safe.
        }
    }

    fn handle<F>(mut stream: UnixStream, handler: &F)
    where
        F: Fn(Request) -> Response,
    {
        if stream.set_read_timeout(Some(super::IO_TIMEOUT)).is_err()
            || stream.set_write_timeout(Some(super::IO_TIMEOUT)).is_err()
        {
            return;
        }
        if let Ok(line) = super::read_frame(BufReader::new(&stream), super::MAX_REQUEST_BYTES) {
            let response = match serde_json::from_str::<Request>(&line) {
                Ok(request) => handler(request),
                Err(error) => Response::error(String::new(), -32700, error.to_string()),
            };
            if serde_json::to_writer(&mut stream, &response).is_ok() {
                let _ = stream.write_all(b"\n");
            }
        }
    }

    pub fn call(path: &Path, request: &Request) -> Result<Response, IpcError> {
        let request = super::encode_request(request)?;
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(super::IO_TIMEOUT))?;
        stream.set_write_timeout(Some(super::IO_TIMEOUT))?;
        stream.write_all(&request)?;
        let line = super::read_frame(BufReader::new(stream), super::MAX_RESPONSE_BYTES)?;
        Ok(serde_json::from_str(&line)?)
    }

    pub fn notify(path: &Path, request: &Request) -> Result<(), IpcError> {
        let request = super::encode_request(request)?;
        let mut stream = UnixStream::connect(path)?;
        stream.set_write_timeout(Some(super::IO_TIMEOUT))?;
        stream.write_all(&request)?;
        stream.flush()?;
        Ok(())
    }

    pub use Server as UnixServer;
}

#[cfg(unix)]
pub use unix::{UnixServer, call, notify};

#[cfg(windows)]
mod windows {
    use std::fs;
    use std::io::{BufReader, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::{self, JoinHandle};

    use super::{IpcError, Request, Response};

    pub struct Server {
        stop: Arc<AtomicBool>,
        worker: Option<JoinHandle<()>>,
        endpoint_path: PathBuf,
        discovery_path: Option<PathBuf>,
        address: String,
        token: String,
    }
    impl Server {
        pub fn spawn<F>(path: &Path, handler: F) -> Result<Self, IpcError>
        where
            F: Fn(Request) -> Response + Send + Sync + 'static,
        {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let address = listener.local_addr()?.to_string();
            let token = uuid::Uuid::new_v4().to_string();
            listener.set_nonblocking(true)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, format!("{address} {token}\n"))?;
            let discovery_path = super::default_socket_path().filter(|value| value != path);
            if let Some(discovery) = &discovery_path {
                if let Some(parent) = discovery.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(discovery, format!("{address} {token}\n"))?;
            }
            let stop = Arc::new(AtomicBool::new(false));
            let thread_stop = Arc::clone(&stop);
            let handler = Arc::new(handler);
            let expected_token = token.clone();
            let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let worker = thread::Builder::new()
                .name("wmcterm-ipc".into())
                .spawn(move || {
                    while !thread_stop.load(Ordering::Acquire) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                let Some(permit) = super::ConnectionPermit::acquire(&active) else {
                                    continue;
                                };
                                let expected_token = expected_token.clone();
                                let handler = handler.clone();
                                let _ = thread::Builder::new()
                                    .name("wmcterm-ipc-client".into())
                                    .spawn(move || {
                                        let _permit = permit;
                                        if stream.set_read_timeout(Some(super::IO_TIMEOUT)).is_err()
                                            || stream
                                                .set_write_timeout(Some(super::IO_TIMEOUT))
                                                .is_err()
                                        {
                                            return;
                                        }
                                        let mut reader = BufReader::new(&stream);
                                        if !super::read_frame(&mut reader, 128)
                                            .is_ok_and(|line| line.trim() == expected_token)
                                        {
                                            return;
                                        }
                                        if let Ok(line) =
                                            super::read_frame(&mut reader, super::MAX_REQUEST_BYTES)
                                        {
                                            let response = serde_json::from_str::<Request>(&line)
                                                .map(|request| handler(request))
                                                .unwrap_or_else(|error| {
                                                    Response::error(
                                                        String::new(),
                                                        -32700,
                                                        error.to_string(),
                                                    )
                                                });
                                            if serde_json::to_writer(&mut stream, &response).is_ok()
                                            {
                                                let _ = stream.write_all(b"\n");
                                            }
                                        }
                                    });
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                thread::sleep(std::time::Duration::from_millis(10))
                            }
                            Err(_) => break,
                        }
                    }
                })?;
            Ok(Self {
                stop,
                worker: Some(worker),
                endpoint_path: path.to_path_buf(),
                discovery_path,
                address,
                token,
            })
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            if let Some(worker) = self.worker.take() {
                let _ = worker.join();
            }
            if fs::read_to_string(&self.endpoint_path)
                .ok()
                .is_some_and(|value| value.trim() == format!("{} {}", self.address, self.token))
            {
                let _ = fs::remove_file(&self.endpoint_path);
            }
            if let Some(path) = &self.discovery_path
                && fs::read_to_string(path)
                    .ok()
                    .is_some_and(|value| value.trim() == format!("{} {}", self.address, self.token))
            {
                let _ = fs::remove_file(path);
            }
        }
    }
    fn connect(path: &Path) -> Result<(TcpStream, String), IpcError> {
        let endpoint = fs::read_to_string(path)?;
        let (address, token) = endpoint.trim().split_once(' ').ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid wmcterm endpoint")
        })?;
        let address = address
            .parse()
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        let stream = TcpStream::connect_timeout(&address, super::IO_TIMEOUT)?;
        stream.set_read_timeout(Some(super::IO_TIMEOUT))?;
        stream.set_write_timeout(Some(super::IO_TIMEOUT))?;
        Ok((stream, token.to_owned()))
    }
    pub fn call(path: &Path, request: &Request) -> Result<Response, IpcError> {
        let request = super::encode_request(request)?;
        let (mut stream, token) = connect(path)?;
        writeln!(stream, "{token}")?;
        stream.write_all(&request)?;
        let line = super::read_frame(BufReader::new(stream), super::MAX_RESPONSE_BYTES)?;
        Ok(serde_json::from_str(&line)?)
    }

    pub fn notify(path: &Path, request: &Request) -> Result<(), IpcError> {
        let request = super::encode_request(request)?;
        let (mut stream, token) = connect(path)?;
        writeln!(stream, "{token}")?;
        stream.write_all(&request)?;
        stream.flush()?;
        Ok(())
    }
    pub use Server as UnixServer;
}

#[cfg(windows)]
pub use windows::{UnixServer, call, notify};

#[cfg(test)]
mod agent_request_tests {
    use super::*;

    fn request(id: &str, lifecycle: &str, deadline: Option<u64>) -> AgentInteractionRequest {
        AgentInteractionRequest {
            request_id: id.into(),
            surface_id: "surface-1".into(),
            lifecycle_id: lifecycle.into(),
            agent_pid: Some(42),
            pid_start_time: Some(900),
            kind: AgentRequestKind::new(AgentRequestKind::PERMISSION),
            message: "Allow command?".into(),
            choices: vec![
                AgentChoice {
                    id: "allow".into(),
                    label: "Allow".into(),
                },
                AgentChoice {
                    id: "deny".into(),
                    label: "Deny".into(),
                },
            ],
            deadline_unix_ms: deadline,
        }
    }

    #[test]
    fn wire_types_round_trip_unknown_request_kinds() {
        let mut value = request("r1", "life-1", Some(500));
        value.kind = AgentRequestKind::new("future_interaction");
        let encoded = serde_json::to_string(&value).unwrap();
        let decoded: AgentInteractionRequest = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, value);

        let reply = AgentInteractionReply {
            request_id: "r1".into(),
            surface_id: "surface-1".into(),
            lifecycle_id: "life-1".into(),
            resolved_at_unix_ms: 123,
            status: AgentReplyStatus::Resolved {
                resolution: AgentResolution {
                    choice_id: Some("allow".into()),
                    approved: Some(true),
                    text: None,
                },
            },
        };
        assert_eq!(
            serde_json::from_str::<AgentInteractionReply>(&serde_json::to_string(&reply).unwrap())
                .unwrap(),
            reply
        );
    }

    #[test]
    fn concurrent_input_reply_and_disconnect_preserve_first_completion() {
        for iteration in 0..32 {
            let pending = PendingAgentRequests::new(1).unwrap();
            let mut input = request("r1", "life-1", None);
            input.kind = AgentRequestKind::new(AgentRequestKind::INPUT);
            pending.register(input).unwrap();
            let gate = Arc::new(std::sync::Barrier::new(3));
            let replying = pending.clone();
            let reply_gate = gate.clone();
            let answer = std::thread::spawn(move || {
                reply_gate.wait();
                replying
                    .resolve(
                        "r1",
                        AgentResolution {
                            text: Some(r#"{"target":"test"}"#.into()),
                            ..Default::default()
                        },
                        iteration,
                    )
                    .unwrap()
            });
            let disconnecting = pending.clone();
            let cancel_gate = gate.clone();
            let cancel = std::thread::spawn(move || {
                cancel_gate.wait();
                disconnecting
                    .cancel("r1", "codex_disconnected", iteration)
                    .unwrap()
            });
            gate.wait();
            let outcomes = [answer.join().unwrap(), cancel.join().unwrap()];
            assert_eq!(
                outcomes
                    .iter()
                    .filter(|outcome| matches!(outcome, CompleteOutcome::Completed(_)))
                    .count(),
                1
            );
            let replies = outcomes.map(|outcome| match outcome {
                CompleteOutcome::Completed(reply) | CompleteOutcome::AlreadyCompleted(reply) => {
                    reply
                }
            });
            assert_eq!(replies[0], replies[1]);
            assert_eq!(
                pending.completed_reply("r1").unwrap(),
                Some(replies[0].clone())
            );
            assert!(pending.is_empty().unwrap());
        }
    }

    #[test]
    fn registration_is_bounded_and_duplicate_safe() {
        let pending = PendingAgentRequests::new(1).unwrap();
        let first = request("r1", "life-1", None);
        assert_eq!(
            pending.register(first.clone()),
            Ok(RegisterOutcome::Inserted)
        );
        assert_eq!(
            pending.register(first.clone()),
            Ok(RegisterOutcome::DuplicatePending)
        );
        let mut conflict = first;
        conflict.message = "different".into();
        assert_eq!(
            pending.register(conflict),
            Err(PendingAgentRequestError::ConflictingDuplicate("r1".into()))
        );
        assert_eq!(
            pending.register(request("r2", "life-1", None)),
            Err(PendingAgentRequestError::CapacityExceeded { capacity: 1 })
        );
        assert_eq!(pending.list().unwrap()[0].request_id, "r1");
    }

    #[test]
    fn completion_is_idempotent_even_with_a_different_retry_payload() {
        let pending = PendingAgentRequests::new(4).unwrap();
        let original = request("r1", "life-1", None);
        pending.register(original.clone()).unwrap();
        let resolution = AgentResolution {
            choice_id: Some("allow".into()),
            approved: Some(true),
            text: None,
        };
        let completed = pending.resolve("r1", resolution, 20).unwrap();
        let CompleteOutcome::Completed(reply) = completed else {
            panic!("first resolution must complete")
        };
        let retry = pending
            .resolve(
                "r1",
                AgentResolution {
                    choice_id: Some("deny".into()),
                    approved: Some(false),
                    text: None,
                },
                30,
            )
            .unwrap();
        assert_eq!(retry, CompleteOutcome::AlreadyCompleted(reply.clone()));
        assert_eq!(
            pending.register(original.clone()),
            Ok(RegisterOutcome::AlreadyCompleted(reply))
        );
        let mut conflicting_retry = original;
        conflicting_retry.lifecycle_id = "new-life".into();
        assert_eq!(
            pending.register(conflicting_retry),
            Err(PendingAgentRequestError::ConflictingDuplicate("r1".into()))
        );
    }

    #[test]
    fn expiration_and_process_exit_only_complete_matching_requests() {
        let pending = PendingAgentRequests::new(8).unwrap();
        pending
            .register(request("expired", "life-1", Some(100)))
            .unwrap();
        pending
            .register(request("later", "life-1", Some(200)))
            .unwrap();
        pending.register(request("other", "life-2", None)).unwrap();
        let expired = pending.expire(100).unwrap();
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].request_id, "expired");
        assert_eq!(expired[0].status, AgentReplyStatus::Expired);

        let exited = pending
            .process_exited(
                &AgentProcessScope {
                    surface_id: "surface-1".into(),
                    lifecycle_id: "life-1".into(),
                    agent_pid: Some(42),
                    pid_start_time: Some(900),
                },
                150,
            )
            .unwrap();
        assert_eq!(exited.len(), 1);
        assert_eq!(exited[0].request_id, "later");
        assert_eq!(pending.list().unwrap()[0].request_id, "other");

        let deadline_race = PendingAgentRequests::new(1).unwrap();
        deadline_race
            .register(request("too-late", "life-3", Some(10)))
            .unwrap();
        let CompleteOutcome::Completed(reply) = deadline_race
            .resolve("too-late", AgentResolution::default(), 10)
            .unwrap()
        else {
            panic!("first late resolution must complete as expired")
        };
        assert_eq!(reply.status, AgentReplyStatus::Expired);
    }

    #[test]
    fn concurrent_resolution_has_exactly_one_winner() {
        use std::sync::Barrier;
        use std::thread;

        let pending = PendingAgentRequests::new(2).unwrap();
        pending.register(request("race", "life-1", None)).unwrap();
        let workers = 12;
        let barrier = Arc::new(Barrier::new(workers));
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let pending = pending.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    pending
                        .resolve("race", AgentResolution::default(), 50)
                        .unwrap()
                })
            })
            .collect();
        let outcomes: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, CompleteOutcome::Completed(_)))
                .count(),
            1
        );
        assert_eq!(pending.len(), Ok(0));
    }
}

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn frames_reject_oversize_and_incomplete_requests() {
        assert!(read_frame(std::io::Cursor::new(b"12345\n"), 5).is_err());
        assert!(read_frame(std::io::Cursor::new(b"1234"), 5).is_err());
        assert_eq!(
            read_frame(std::io::Cursor::new(b"1234\nextra"), 5).unwrap(),
            "1234\n"
        );
    }

    #[test]
    fn stalled_connection_does_not_block_other_clients_or_shutdown() {
        use std::os::unix::net::UnixStream;
        let path =
            std::env::temp_dir().join(format!("wmcterm-stalled-{}.sock", uuid::Uuid::new_v4()));
        let server =
            UnixServer::spawn(&path, |request| Response::success(request.id, json!(true))).unwrap();
        let stalled = UnixStream::connect(&path).unwrap();
        let response = call(
            &path,
            &Request {
                id: "healthy".into(),
                method: "test".into(),
                params: json!({}),
            },
        )
        .unwrap();
        assert_eq!(response.result, Some(json!(true)));
        let started = std::time::Instant::now();
        drop(server);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        drop(stalled);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn connection_capacity_is_released_with_the_permit() {
        let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let permits: Vec<_> = (0..MAX_CONNECTIONS)
            .map(|_| ConnectionPermit::acquire(&active).unwrap())
            .collect();
        assert!(ConnectionPermit::acquire(&active).is_none());
        drop(permits);
        assert!(ConnectionPermit::acquire(&active).is_some());
    }

    #[test]
    fn request_round_trips_over_unix_socket() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("wmcterm-{nonce}.sock"));
        let _server = UnixServer::spawn(&path, |request| {
            Response::success(request.id, json!({"ok": true}))
        })
        .unwrap();
        let request = Request {
            id: "1".into(),
            method: "ping".into(),
            params: Value::Null,
        };
        let response = call(&path, &request).unwrap();
        assert_eq!(response.result, Some(json!({"ok": true})));
    }

    #[test]
    fn replaced_socket_survives_the_previous_server_shutdown() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("wmcterm-restart-{nonce}.sock"));
        let previous = UnixServer::spawn(&path, |request| {
            Response::success(request.id, json!({"server": "previous"}))
        })
        .unwrap();
        let current = UnixServer::spawn(&path, |request| {
            Response::success(request.id, json!({"server": "current"}))
        })
        .unwrap();

        drop(previous);

        let request = Request {
            id: "restart".into(),
            method: "ping".into(),
            params: Value::Null,
        };
        let response = call(&path, &request).unwrap();
        assert_eq!(response.result, Some(json!({"server": "current"})));
        drop(current);
        assert!(path.exists());

        let replacement = UnixServer::spawn(&path, |request| {
            Response::success(request.id, json!({"server": "replacement"}))
        })
        .unwrap();
        let response = call(&path, &request).unwrap();
        assert_eq!(response.result, Some(json!({"server": "replacement"})));
        drop(replacement);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn notify_sends_a_complete_line_without_waiting_for_handler_response() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("wmcterm-notify-{nonce}.sock"));
        let (handled, received) = mpsc::channel();
        let _server = UnixServer::spawn(&path, move |request| {
            std::thread::sleep(Duration::from_millis(300));
            handled.send(request).unwrap();
            Response::success("ignored".into(), Value::Null)
        })
        .unwrap();
        let request = Request {
            id: "one-way".into(),
            method: "agent.update".into(),
            params: json!({"complete": true}),
        };
        let start = Instant::now();
        notify(&path, &request).unwrap();
        assert!(start.elapsed() < Duration::from_millis(150));
        let delivered = received.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(delivered.id, "one-way");
        assert_eq!(delivered.params, json!({"complete": true}));
    }
}
