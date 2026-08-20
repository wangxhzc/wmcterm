//! Claude Code hook integration.
//!
//! This crate implements the blocking `PermissionRequest` command hook.  It
//! deliberately has no dependency on the terminal UI: a hook registers a
//! request with wmcterm, polls for the user's decision, then prints the exact
//! JSON shape expected by Claude Code.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;
use wmcterm_ipc::{Request, call};

pub const APPROVAL_REGISTER_METHOD: &str = "agent.approval.register";
pub const APPROVAL_POLL_METHOD: &str = "agent.approval.poll";
pub const APPROVAL_CANCEL_METHOD: &str = "agent.request.cancel";
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(9 * 60);
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MAX_PAYLOAD_BYTES: usize = 256 * 1024;
const MAX_TEXT_BYTES: usize = 4096;
const MAX_SUGGESTIONS: usize = 32;
const CONTROL_CALL_TIMEOUT: Duration = Duration::from_secs(2);

static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalDecision {
    Deny,
    Once,
    Always,
    All,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PermissionRequest {
    pub request_id: String,
    pub session_id: String,
    pub cwd: String,
    pub permission_mode: String,
    pub tool_name: String,
    pub tool_input: Map<String, Value>,
    pub permission_suggestions: Vec<Value>,
    pub raw: Value,
}

impl PermissionRequest {
    pub fn parse(raw: &Value) -> Result<Self, ClaudeApprovalError> {
        let object = raw.as_object().ok_or(ClaudeApprovalError::InvalidPayload(
            "root must be an object",
        ))?;
        if serde_json::to_vec(raw)?.len() > MAX_PAYLOAD_BYTES {
            return Err(ClaudeApprovalError::InvalidPayload("payload is too large"));
        }
        if required_text(object, "hook_event_name", 64)? != "PermissionRequest" {
            return Err(ClaudeApprovalError::InvalidPayload(
                "hook_event_name must be PermissionRequest",
            ));
        }
        let session_id = required_text(object, "session_id", MAX_TEXT_BYTES)?.to_owned();
        let cwd = required_text(object, "cwd", MAX_TEXT_BYTES)?.to_owned();
        let permission_mode = required_text(object, "permission_mode", 64)?.to_owned();
        if !matches!(
            permission_mode.as_str(),
            "default" | "manual" | "plan" | "acceptEdits" | "auto" | "dontAsk"
        ) {
            return Err(ClaudeApprovalError::UnsafePermissionMode(permission_mode));
        }
        let tool_name = required_text(object, "tool_name", 256)?.to_owned();
        let tool_input = object
            .get("tool_input")
            .and_then(Value::as_object)
            .cloned()
            .ok_or(ClaudeApprovalError::InvalidPayload(
                "tool_input must be an object",
            ))?;
        let permission_suggestions = parse_suggestions(object.get("permission_suggestions"))?;
        Ok(Self {
            request_id: next_request_id(),
            session_id,
            cwd,
            permission_mode,
            tool_name,
            tool_input,
            permission_suggestions,
            raw: raw.clone(),
        })
    }

    pub fn supported_decisions(&self) -> Vec<ApprovalDecision> {
        let mut decisions = vec![ApprovalDecision::Deny, ApprovalDecision::Once];
        // The current wmcterm approval UI exposes a single "always" action.
        // Only offer it when that action maps to exactly one visible Claude
        // suggestion; silently choosing among several rules could grant a
        // broader or simply different permission than the user intended.
        if self.permission_suggestions.len() == 1 {
            decisions.push(ApprovalDecision::Always);
        }
        decisions
    }

    pub fn registration_params(&self, config: &BridgeConfig) -> Value {
        let decisions = self
            .supported_decisions()
            .into_iter()
            .map(|decision| serde_json::to_value(decision).expect("decision serializes"))
            .collect::<Vec<_>>();
        json!({
            "source": "claude_hook",
            "workspace_id": config.workspace_id,
            "surface_id": config.surface_id,
            "lifecycle_id": config.lifecycle_id,
            "deadline_unix_ms": deadline_unix_ms(config.timeout),
            "request": {
                "jsonrpc_id": self.request_id,
                "method": "PermissionRequest",
                "kind": "permission",
                "thread_id": self.session_id,
                "turn_id": "hook",
                "item_id": self.request_id,
                "params": self.raw,
            },
            "decisions": decisions,
        })
    }

    pub fn response(&self, decision: ApprovalDecision) -> Value {
        match decision {
            ApprovalDecision::Once => allow_response(None),
            ApprovalDecision::Always if self.permission_suggestions.len() == 1 => {
                allow_response(self.permission_suggestions.first().cloned())
            }
            ApprovalDecision::Always => {
                deny_response("wmcterm rejected an unavailable or ambiguous decision")
            }
            ApprovalDecision::Deny | ApprovalDecision::All => {
                deny_response("Permission denied in wmcterm")
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub control_socket: PathBuf,
    pub workspace_id: String,
    pub surface_id: String,
    pub lifecycle_id: String,
    pub timeout: Duration,
    pub poll_interval: Duration,
}

impl BridgeConfig {
    pub fn new(
        control_socket: PathBuf,
        workspace_id: String,
        surface_id: String,
        lifecycle_id: String,
    ) -> Self {
        Self {
            control_socket,
            workspace_id,
            surface_id,
            lifecycle_id,
            timeout: DEFAULT_APPROVAL_TIMEOUT,
            poll_interval: DEFAULT_POLL_INTERVAL,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalControlStatus {
    Pending { approval_id: String },
    Resolved { decision: ApprovalDecision },
}

pub trait ApprovalControl {
    fn register(
        &mut self,
        request: &PermissionRequest,
        config: &BridgeConfig,
    ) -> Result<ApprovalControlStatus, ClaudeApprovalError>;
    fn poll(&mut self, approval_id: &str) -> Result<ApprovalControlStatus, ClaudeApprovalError>;
    fn cancel(&mut self, approval_id: &str, reason: &str) -> Result<(), ClaudeApprovalError>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookOutcome {
    /// Structured stdout for Claude Code. This is always an allow or deny
    /// decision; errors never fall through to Claude's native prompt.
    pub output: Value,
    /// Diagnostic text for stderr/logging. `output` is already fail-closed.
    pub diagnostic: Option<String>,
}

impl HookOutcome {
    fn denied(error: impl std::fmt::Display) -> Self {
        let diagnostic = error.to_string();
        let detail = diagnostic
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .take(240)
            .collect::<String>();
        Self {
            output: deny_response(&format!("wmcterm permission bridge failed: {detail}")),
            diagnostic: Some(diagnostic),
        }
    }
}

/// Resolve a Claude `PermissionRequest` using the wmcterm control socket.
/// Every parse, IPC, timeout, cancellation, or protocol error returns a
/// structured Claude denial.
pub fn handle_permission_request(raw: &Value, config: &BridgeConfig) -> HookOutcome {
    let mut control = SocketApprovalControl::new(config.control_socket.clone());
    handle_permission_request_with(raw, config, &mut control, None)
}

/// Testable/embeddable form of [`handle_permission_request`]. Passing a stop
/// flag lets the owning process cancel a pending request during shutdown.
pub fn handle_permission_request_with(
    raw: &Value,
    config: &BridgeConfig,
    control: &mut impl ApprovalControl,
    stop: Option<&AtomicBool>,
) -> HookOutcome {
    let request = match PermissionRequest::parse(raw) {
        Ok(request) => request,
        Err(error) => return HookOutcome::denied(error),
    };
    let registered = match control.register(&request, config) {
        Ok(status) => status,
        Err(error) => return HookOutcome::denied(error),
    };
    let ApprovalControlStatus::Pending { approval_id } = registered else {
        if let ApprovalControlStatus::Resolved { decision } = registered {
            return resolved_outcome(&request, decision);
        }
        unreachable!();
    };
    if approval_id.is_empty() {
        return HookOutcome::denied(ClaudeApprovalError::Protocol(
            "pending approval has an empty id".into(),
        ));
    }

    let started = Instant::now();
    loop {
        if stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
            let _ = control.cancel(&approval_id, "hook_interrupted");
            return HookOutcome::denied(ClaudeApprovalError::Interrupted);
        }
        if started.elapsed() >= config.timeout {
            let _ = control.cancel(&approval_id, "hook_timeout");
            return HookOutcome::denied(ClaudeApprovalError::Timeout(config.timeout));
        }
        match control.poll(&approval_id) {
            Ok(ApprovalControlStatus::Resolved { decision }) => {
                return resolved_outcome(&request, decision);
            }
            Ok(ApprovalControlStatus::Pending {
                approval_id: returned,
            }) if returned == approval_id => {}
            Ok(ApprovalControlStatus::Pending { .. }) => {
                let _ = control.cancel(&approval_id, "protocol_error");
                return HookOutcome::denied(ClaudeApprovalError::Protocol(
                    "approval id changed while polling".into(),
                ));
            }
            Err(error) => {
                let _ = control.cancel(&approval_id, "control_disconnected");
                return HookOutcome::denied(error);
            }
        }
        let remaining = config.timeout.saturating_sub(started.elapsed());
        std::thread::sleep(config.poll_interval.min(remaining));
    }
}

fn resolved_outcome(request: &PermissionRequest, decision: ApprovalDecision) -> HookOutcome {
    if request.supported_decisions().contains(&decision) {
        HookOutcome {
            output: request.response(decision),
            diagnostic: None,
        }
    } else {
        HookOutcome::denied(ClaudeApprovalError::Protocol(format!(
            "unsupported wmcterm decision {decision:?}"
        )))
    }
}

pub fn deny_response(message: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": {
                "behavior": "deny",
                "message": message,
                "interrupt": false,
            }
        }
    })
}

fn allow_response(suggestion: Option<Value>) -> Value {
    let mut decision = Map::from_iter([("behavior".into(), Value::String("allow".into()))]);
    if let Some(suggestion) = suggestion {
        decision.insert("updatedPermissions".into(), Value::Array(vec![suggestion]));
    }
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PermissionRequest",
            "decision": decision,
        }
    })
}

#[derive(Debug, Error)]
pub enum ClaudeApprovalError {
    #[error("invalid Claude PermissionRequest: {0}")]
    InvalidPayload(&'static str),
    #[error("unsafe Claude permission mode `{0}`")]
    UnsafePermissionMode(String),
    #[error("invalid Claude hook JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("wmcterm control error: {0}")]
    Control(String),
    #[error("wmcterm approval protocol error: {0}")]
    Protocol(String),
    #[error("wmcterm approval timed out after {0:?}")]
    Timeout(Duration),
    #[error("wmcterm approval was interrupted")]
    Interrupted,
}

pub struct SocketApprovalControl {
    worker: Option<mpsc::Sender<ControlWork>>,
}

impl SocketApprovalControl {
    pub fn new(path: PathBuf) -> Self {
        let (sender, receiver) = mpsc::channel::<ControlWork>();
        let worker = std::thread::Builder::new()
            .name("wmcterm-claude-control".into())
            .spawn(move || {
                while let Ok(work) = receiver.recv() {
                    let _ = work.reply.send(call(&path, &work.request));
                }
            })
            .ok()
            .map(|_| sender);
        Self { worker }
    }

    fn call(&self, request: Request) -> Result<wmcterm_ipc::Response, ClaudeApprovalError> {
        let worker = self.worker.as_ref().ok_or_else(|| {
            ClaudeApprovalError::Control("control socket worker could not start".into())
        })?;
        let (reply, response) = mpsc::sync_channel(1);
        worker
            .send(ControlWork { request, reply })
            .map_err(|_| ClaudeApprovalError::Control("control socket worker stopped".into()))?;
        match response.recv_timeout(CONTROL_CALL_TIMEOUT) {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(error)) => Err(ClaudeApprovalError::Control(error.to_string())),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ClaudeApprovalError::Control(
                "control socket request timed out".into(),
            )),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ClaudeApprovalError::Control(
                "control socket worker disconnected".into(),
            )),
        }
    }

    fn request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<ApprovalControlStatus, ClaudeApprovalError> {
        let response = self.call(Request {
            id: format!("claude-approval-{}", next_sequence()),
            method: method.into(),
            params,
        })?;
        if let Some(error) = response.error {
            return Err(ClaudeApprovalError::Control(error.message));
        }
        parse_control_status(
            response
                .result
                .as_ref()
                .ok_or_else(|| ClaudeApprovalError::Control("response has no result".into()))?,
        )
    }
}

impl ApprovalControl for SocketApprovalControl {
    fn register(
        &mut self,
        request: &PermissionRequest,
        config: &BridgeConfig,
    ) -> Result<ApprovalControlStatus, ClaudeApprovalError> {
        self.request(
            APPROVAL_REGISTER_METHOD,
            request.registration_params(config),
        )
    }

    fn poll(&mut self, approval_id: &str) -> Result<ApprovalControlStatus, ClaudeApprovalError> {
        self.request(APPROVAL_POLL_METHOD, json!({"approval_id": approval_id}))
    }

    fn cancel(&mut self, approval_id: &str, reason: &str) -> Result<(), ClaudeApprovalError> {
        let response = self.call(Request {
            id: format!("claude-approval-{}", next_sequence()),
            method: APPROVAL_CANCEL_METHOD.into(),
            params: json!({"request_id": approval_id, "reason": reason}),
        })?;
        if let Some(error) = response.error {
            return Err(ClaudeApprovalError::Control(error.message));
        }
        Ok(())
    }
}

struct ControlWork {
    request: Request,
    reply: mpsc::SyncSender<Result<wmcterm_ipc::Response, wmcterm_ipc::IpcError>>,
}

fn parse_control_status(value: &Value) -> Result<ApprovalControlStatus, ClaudeApprovalError> {
    match value.get("status").and_then(Value::as_str) {
        Some("pending") => value
            .get("approval_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(|approval_id| ApprovalControlStatus::Pending {
                approval_id: approval_id.to_owned(),
            })
            .ok_or_else(|| ClaudeApprovalError::Protocol("pending response has no id".into())),
        Some("resolved") => {
            let decision = value
                .pointer("/decision/mode")
                .or_else(|| value.get("decision"))
                .cloned()
                .ok_or_else(|| {
                    ClaudeApprovalError::Protocol("resolved response has no decision".into())
                })?;
            serde_json::from_value(decision)
                .map(|decision| ApprovalControlStatus::Resolved { decision })
                .map_err(|error| {
                    ClaudeApprovalError::Protocol(format!("invalid decision: {error}"))
                })
        }
        Some(status) => Err(ClaudeApprovalError::Protocol(format!(
            "unknown approval status `{status}`"
        ))),
        None => Err(ClaudeApprovalError::Protocol(
            "approval response has no status".into(),
        )),
    }
}

fn required_text<'a>(
    object: &'a Map<String, Value>,
    key: &'static str,
    max: usize,
) -> Result<&'a str, ClaudeApprovalError> {
    let text = object
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or(ClaudeApprovalError::InvalidPayload(key))?;
    if text.len() > max {
        return Err(ClaudeApprovalError::InvalidPayload(
            "text field is too large",
        ));
    }
    Ok(text)
}

fn parse_suggestions(value: Option<&Value>) -> Result<Vec<Value>, ClaudeApprovalError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let suggestions = value.as_array().ok_or(ClaudeApprovalError::InvalidPayload(
        "permission_suggestions must be an array",
    ))?;
    if suggestions.len() > MAX_SUGGESTIONS {
        return Err(ClaudeApprovalError::InvalidPayload(
            "too many permission_suggestions",
        ));
    }
    suggestions
        .iter()
        .map(|suggestion| {
            validate_suggestion(suggestion)?;
            Ok(suggestion.clone())
        })
        .collect()
}

fn validate_suggestion(value: &Value) -> Result<(), ClaudeApprovalError> {
    let object = value
        .as_object()
        .ok_or(ClaudeApprovalError::InvalidPayload(
            "permission suggestion must be an object",
        ))?;
    let kind = required_text(object, "type", 64)?;
    if !matches!(
        kind,
        "addRules"
            | "replaceRules"
            | "removeRules"
            | "setMode"
            | "addDirectories"
            | "removeDirectories"
    ) {
        return Err(ClaudeApprovalError::InvalidPayload(
            "unknown permission suggestion type",
        ));
    }
    let destination = required_text(object, "destination", 64)?;
    if !matches!(
        destination,
        "session" | "localSettings" | "projectSettings" | "userSettings"
    ) {
        return Err(ClaudeApprovalError::InvalidPayload(
            "unknown permission suggestion destination",
        ));
    }
    match kind {
        "addRules" | "replaceRules" | "removeRules" => {
            let behavior = required_text(object, "behavior", 16)?;
            if !matches!(behavior, "allow" | "deny" | "ask") {
                return Err(ClaudeApprovalError::InvalidPayload(
                    "invalid permission rule behavior",
                ));
            }
            let rules = object.get("rules").and_then(Value::as_array).ok_or(
                ClaudeApprovalError::InvalidPayload("permission rules must be an array"),
            )?;
            if rules.is_empty() || rules.len() > 128 {
                return Err(ClaudeApprovalError::InvalidPayload(
                    "invalid permission rule count",
                ));
            }
            for rule in rules {
                let rule = rule.as_object().ok_or(ClaudeApprovalError::InvalidPayload(
                    "permission rule must be an object",
                ))?;
                required_text(rule, "toolName", 256)?;
                if let Some(content) = rule.get("ruleContent") {
                    let content = content.as_str().ok_or(ClaudeApprovalError::InvalidPayload(
                        "permission ruleContent must be text",
                    ))?;
                    if content.len() > MAX_TEXT_BYTES {
                        return Err(ClaudeApprovalError::InvalidPayload(
                            "permission ruleContent is too large",
                        ));
                    }
                }
            }
        }
        "setMode" => {
            let mode = required_text(object, "mode", 64)?;
            if !matches!(
                mode,
                "default" | "manual" | "plan" | "acceptEdits" | "auto" | "dontAsk"
            ) {
                return Err(ClaudeApprovalError::UnsafePermissionMode(mode.into()));
            }
        }
        "addDirectories" | "removeDirectories" => {
            let directories = object.get("directories").and_then(Value::as_array).ok_or(
                ClaudeApprovalError::InvalidPayload("directories must be an array"),
            )?;
            if directories.is_empty()
                || directories.len() > 128
                || directories.iter().any(|path| {
                    path.as_str()
                        .is_none_or(|path| path.is_empty() || path.len() > MAX_TEXT_BYTES)
                })
            {
                return Err(ClaudeApprovalError::InvalidPayload(
                    "invalid permission directories",
                ));
            }
        }
        _ => unreachable!(),
    }
    if contains_bypass_permissions(value) {
        return Err(ClaudeApprovalError::UnsafePermissionMode(
            "bypassPermissions".into(),
        ));
    }
    Ok(())
}

fn contains_bypass_permissions(value: &Value) -> bool {
    match value {
        Value::String(text) => text == "bypassPermissions",
        Value::Array(items) => items.iter().any(contains_bypass_permissions),
        Value::Object(object) => object.values().any(contains_bypass_permissions),
        _ => false,
    }
}

fn next_sequence() -> u64 {
    REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
}

fn next_request_id() -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "claude-hook-{}-{timestamp}-{}",
        std::process::id(),
        next_sequence()
    )
}

fn deadline_unix_ms(timeout: Duration) -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let deadline = now.saturating_add(timeout.as_millis());
    u64::try_from(deadline).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    fn request(suggestions: Value) -> Value {
        json!({
            "session_id": "session-1",
            "transcript_path": "/tmp/transcript.jsonl",
            "cwd": "/work",
            "permission_mode": "default",
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {"command":"cargo test"},
            "permission_suggestions": suggestions,
        })
    }

    fn suggestion() -> Value {
        json!({
            "type":"addRules",
            "rules":[{"toolName":"Bash","ruleContent":"cargo test"}],
            "behavior":"allow",
            "destination":"localSettings"
        })
    }

    fn config() -> BridgeConfig {
        BridgeConfig {
            control_socket: "/tmp/wmcterm.sock".into(),
            workspace_id: "workspace".into(),
            surface_id: "surface".into(),
            lifecycle_id: "lifecycle".into(),
            timeout: Duration::from_millis(10),
            poll_interval: Duration::ZERO,
        }
    }

    #[derive(Default)]
    struct MockControl {
        register: Option<Result<ApprovalControlStatus, ClaudeApprovalError>>,
        polls: VecDeque<Result<ApprovalControlStatus, ClaudeApprovalError>>,
        cancelled: Vec<(String, String)>,
        registration: Option<Value>,
    }

    impl ApprovalControl for MockControl {
        fn register(
            &mut self,
            request: &PermissionRequest,
            config: &BridgeConfig,
        ) -> Result<ApprovalControlStatus, ClaudeApprovalError> {
            self.registration = Some(request.registration_params(config));
            self.register.take().unwrap()
        }

        fn poll(
            &mut self,
            _approval_id: &str,
        ) -> Result<ApprovalControlStatus, ClaudeApprovalError> {
            self.polls.pop_front().unwrap_or_else(|| {
                Ok(ApprovalControlStatus::Pending {
                    approval_id: "approval-1".into(),
                })
            })
        }

        fn cancel(&mut self, approval_id: &str, reason: &str) -> Result<(), ClaudeApprovalError> {
            self.cancelled
                .push((approval_id.to_owned(), reason.to_owned()));
            Ok(())
        }
    }

    #[test]
    fn parses_real_permission_request_and_registers_claude_source() {
        let parsed = PermissionRequest::parse(&request(json!([suggestion()]))).unwrap();
        assert_eq!(parsed.tool_name, "Bash");
        assert_eq!(
            parsed.supported_decisions(),
            vec![
                ApprovalDecision::Deny,
                ApprovalDecision::Once,
                ApprovalDecision::Always
            ]
        );
        let wire = parsed.registration_params(&config());
        assert_eq!(wire["source"], "claude_hook");
        assert_eq!(wire["request"]["thread_id"], "session-1");
        assert_eq!(wire["lifecycle_id"], "lifecycle");
        assert!(wire["deadline_unix_ms"].as_u64().is_some());
        assert_eq!(wire["decisions"], json!(["deny", "once", "always"]));
    }

    #[test]
    fn once_allows_without_persisting_permission() {
        let parsed = PermissionRequest::parse(&request(json!([suggestion()]))).unwrap();
        assert_eq!(
            parsed.response(ApprovalDecision::Once),
            json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}})
        );
    }

    #[test]
    fn always_echoes_exact_claude_suggestion() {
        let suggested = suggestion();
        let parsed = PermissionRequest::parse(&request(json!([suggested.clone()]))).unwrap();
        assert_eq!(
            parsed.response(ApprovalDecision::Always)["hookSpecificOutput"]["decision"]["updatedPermissions"],
            json!([suggested])
        );
    }

    #[test]
    fn no_suggestion_never_offers_always_or_all() {
        let parsed = PermissionRequest::parse(&request(json!([]))).unwrap();
        assert_eq!(
            parsed.supported_decisions(),
            vec![ApprovalDecision::Deny, ApprovalDecision::Once]
        );
        assert_eq!(
            parsed.response(ApprovalDecision::All)["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );
    }

    #[test]
    fn multiple_suggestions_do_not_offer_an_ambiguous_always_choice() {
        let parsed = PermissionRequest::parse(&request(json!([
            suggestion(),
            {"type":"addDirectories", "directories":["/tmp/other"], "destination":"session"}
        ])))
        .unwrap();
        assert_eq!(
            parsed.supported_decisions(),
            vec![ApprovalDecision::Deny, ApprovalDecision::Once]
        );
        assert_eq!(
            parsed.response(ApprovalDecision::Always)["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );
    }

    #[test]
    fn rejects_bypass_mode_in_request_and_suggestion() {
        let mut direct = request(json!([]));
        direct["permission_mode"] = json!("bypassPermissions");
        assert!(matches!(
            PermissionRequest::parse(&direct),
            Err(ClaudeApprovalError::UnsafePermissionMode(_))
        ));
        let bypass = json!({
            "type":"setMode", "mode":"bypassPermissions", "destination":"session"
        });
        assert!(matches!(
            PermissionRequest::parse(&request(json!([bypass]))),
            Err(ClaudeApprovalError::UnsafePermissionMode(_))
        ));
    }

    #[test]
    fn resolved_decision_returns_claude_hook_json() {
        let mut control = MockControl {
            register: Some(Ok(ApprovalControlStatus::Pending {
                approval_id: "approval-1".into(),
            })),
            polls: VecDeque::from([Ok(ApprovalControlStatus::Resolved {
                decision: ApprovalDecision::Once,
            })]),
            ..Default::default()
        };
        let outcome =
            handle_permission_request_with(&request(json!([])), &config(), &mut control, None);
        assert_eq!(
            outcome.output["hookSpecificOutput"]["decision"]["behavior"],
            "allow"
        );
        assert!(outcome.diagnostic.is_none());
    }

    #[test]
    fn malformed_control_decision_fails_closed() {
        let mut control = MockControl {
            register: Some(Ok(ApprovalControlStatus::Resolved {
                decision: ApprovalDecision::Always,
            })),
            ..Default::default()
        };
        let outcome =
            handle_permission_request_with(&request(json!([])), &config(), &mut control, None);
        assert_eq!(
            outcome.output["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );
        assert!(outcome.diagnostic.is_some());
    }

    #[test]
    fn polling_disconnect_cancels_and_denies() {
        let mut control = MockControl {
            register: Some(Ok(ApprovalControlStatus::Pending {
                approval_id: "approval-1".into(),
            })),
            polls: VecDeque::from([Err(ClaudeApprovalError::Control("gone".into()))]),
            ..Default::default()
        };
        let outcome =
            handle_permission_request_with(&request(json!([])), &config(), &mut control, None);
        assert_eq!(
            outcome.output["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );
        assert_eq!(
            control.cancelled,
            vec![("approval-1".into(), "control_disconnected".into())]
        );
    }

    #[test]
    fn stop_flag_cancels_and_denies() {
        let mut control = MockControl {
            register: Some(Ok(ApprovalControlStatus::Pending {
                approval_id: "approval-1".into(),
            })),
            ..Default::default()
        };
        let stop = AtomicBool::new(true);
        let outcome = handle_permission_request_with(
            &request(json!([])),
            &config(),
            &mut control,
            Some(&stop),
        );
        assert_eq!(
            outcome.output["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );
        assert_eq!(
            control.cancelled,
            vec![("approval-1".into(), "hook_interrupted".into())]
        );
    }

    #[test]
    fn deadline_cancels_and_denies() {
        let mut control = MockControl {
            register: Some(Ok(ApprovalControlStatus::Pending {
                approval_id: "approval-1".into(),
            })),
            ..Default::default()
        };
        let mut expired = config();
        expired.timeout = Duration::ZERO;
        let outcome =
            handle_permission_request_with(&request(json!([])), &expired, &mut control, None);
        assert_eq!(
            outcome.output["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );
        assert_eq!(
            control.cancelled,
            vec![("approval-1".into(), "hook_timeout".into())]
        );
    }

    #[test]
    fn invalid_payload_denies_without_touching_control_socket() {
        let mut control = MockControl::default();
        let outcome = handle_permission_request_with(
            &json!({"hook_event_name":"PermissionRequest"}),
            &config(),
            &mut control,
            None,
        );
        assert_eq!(
            outcome.output["hookSpecificOutput"]["decision"]["behavior"],
            "deny"
        );
        assert!(
            outcome.output["hookSpecificOutput"]["decision"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("session_id"))
        );
        assert!(control.registration.is_none());
    }

    #[test]
    fn parses_strict_control_status() {
        assert_eq!(
            parse_control_status(&json!({"status":"resolved","decision":{"mode":"once"}})).unwrap(),
            ApprovalControlStatus::Resolved {
                decision: ApprovalDecision::Once
            }
        );
        assert!(parse_control_status(&json!({"status":"pending"})).is_err());
        assert!(
            parse_control_status(&json!({"status":"resolved","decision":{"mode":"yes"}})).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn socket_control_reuses_one_worker_for_multiple_calls() {
        let path = std::env::temp_dir().join(format!(
            "wmcterm-claude-control-test-{}-{}.sock",
            std::process::id(),
            next_sequence()
        ));
        let _server = wmcterm_ipc::UnixServer::spawn(&path, |request| {
            wmcterm_ipc::Response::success(
                request.id,
                json!({"status":"pending","approval_id":"approval-1"}),
            )
        })
        .unwrap();
        let mut control = SocketApprovalControl::new(path);
        for _ in 0..3 {
            assert_eq!(
                control.poll("approval-1").unwrap(),
                ApprovalControlStatus::Pending {
                    approval_id: "approval-1".into()
                }
            );
        }
    }
}
