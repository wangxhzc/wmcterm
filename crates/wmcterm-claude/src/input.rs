use super::*;
use wmcterm_ipc::InputFormRequest;

#[derive(Debug, Clone, PartialEq)]
pub struct ElicitationRequest {
    pub session_id: String,
    pub mcp_server_name: String,
    pub elicitation_id: Option<String>,
    pub form: InputFormRequest,
}

impl ElicitationRequest {
    /// Returning `None` leaves unsupported requests to Claude's native dialog.
    pub fn parse(raw: &Value) -> Option<Self> {
        if serde_json::to_vec(raw).ok()?.len() > 64 * 1024
            || raw["hook_event_name"] != "Elicitation"
            || raw.get("mode").is_some_and(|mode| mode != "form")
            || raw.get("url").is_some()
        {
            return None;
        }
        let text = |key: &str| {
            raw[key]
                .as_str()
                .filter(|value| !value.is_empty() && value.len() <= MAX_TEXT_BYTES)
                .map(str::to_owned)
        };
        let session_id = text("session_id")?;
        let mcp_server_name = text("mcp_server_name")?;
        let elicitation_id = match raw.get("elicitation_id") {
            Some(_) => Some(text("elicitation_id")?),
            None => None,
        };
        // Explicit native IDs deduplicate repeated hooks. Without one, keep
        // concurrent forms from the same server separate.
        let id = elicitation_id.as_ref().map_or_else(next_request_id, |id| {
            format!(
                "claude-elicitation:{}",
                json!([session_id, mcp_server_name, id])
            )
        });
        let form = InputFormRequest {
            id: json!(id),
            method: "Elicitation".into(),
            thread_id: session_id.clone(),
            turn_id: mcp_server_name.clone(),
            item_id: elicitation_id.clone().unwrap_or(id),
            message: text("message")?,
            fields: InputFormRequest::parse_schema(&raw["requested_schema"])?,
        };
        form.validate().ok()?;
        Some(Self {
            session_id,
            mcp_server_name,
            elicitation_id,
            form,
        })
    }

    pub fn registration_params(&self, config: &BridgeConfig) -> Value {
        json!({
            "source": "claude_hook", "workspace_id": config.workspace_id,
            "surface_id": config.surface_id, "lifecycle_id": config.lifecycle_id,
            "session_id": self.session_id, "deadline_unix_ms": deadline_unix_ms(config.timeout),
            "request": {
                "jsonrpc_id": self.form.id, "method": "Elicitation", "kind": "input",
                "thread_id": self.session_id, "turn_id": self.form.turn_id, "item_id": self.form.item_id,
                "params": self.form, "mcp_server_name": self.mcp_server_name,
                "elicitation_id": self.elicitation_id,
            }, "decisions": ["deny", "once"],
        })
    }
}

pub fn cancel_elicitation_response() -> Value {
    json!({"hookSpecificOutput": {"hookEventName": "Elicitation", "action": "cancel"}})
}

#[derive(Debug, Clone, PartialEq)]
pub enum InputControlStatus {
    Pending { request_id: String },
    Resolved { text: Option<String> },
}

pub trait InputControl {
    fn register(
        &mut self,
        request: &ElicitationRequest,
        config: &BridgeConfig,
    ) -> Result<InputControlStatus, ClaudeApprovalError>;
    fn poll(&mut self, request_id: &str) -> Result<InputControlStatus, ClaudeApprovalError>;
    fn cancel(&mut self, request_id: &str, reason: &str) -> Result<(), ClaudeApprovalError>;
}

struct SocketInputControl {
    socket: SocketApprovalControl,
    context: Value,
}

impl SocketInputControl {
    fn new(config: &BridgeConfig, session_id: &str) -> Self {
        Self {
            socket: SocketApprovalControl::new(config.control_socket.clone()),
            context: json!({
                "source": "claude_hook", "workspace_id": config.workspace_id,
                "surface_id": config.surface_id, "lifecycle_id": config.lifecycle_id,
                "session_id": session_id,
            }),
        }
    }

    fn rpc(&self, method: &str, params: Value) -> Result<Value, ClaudeApprovalError> {
        let response = self.socket.call(Request {
            id: format!("claude-input-{}", next_sequence()),
            method: method.into(),
            params,
        })?;
        if let Some(error) = response.error {
            return Err(ClaudeApprovalError::Control(error.message));
        }
        response
            .result
            .ok_or_else(|| ClaudeApprovalError::Protocol("input response has no result".into()))
    }

    fn params(&self, request_id: &str) -> Value {
        let mut params = self.context.clone();
        params["request_id"] = json!(request_id);
        params
    }
}

impl InputControl for SocketInputControl {
    fn register(
        &mut self,
        request: &ElicitationRequest,
        config: &BridgeConfig,
    ) -> Result<InputControlStatus, ClaudeApprovalError> {
        parse_status(&self.rpc("agent.input.register", request.registration_params(config))?)
    }
    fn poll(&mut self, request_id: &str) -> Result<InputControlStatus, ClaudeApprovalError> {
        parse_status(&self.rpc("agent.input.poll", self.params(request_id))?)
    }
    fn cancel(&mut self, request_id: &str, reason: &str) -> Result<(), ClaudeApprovalError> {
        let mut params = self.params(request_id);
        params["reason"] = json!(reason);
        self.rpc("agent.request.cancel", params).map(|_| ())
    }
}

fn parse_status(value: &Value) -> Result<InputControlStatus, ClaudeApprovalError> {
    match value["status"].as_str() {
        Some("pending") => value["approval_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(|id| InputControlStatus::Pending {
                request_id: id.into(),
            })
            .ok_or_else(|| ClaudeApprovalError::Protocol("pending input has no id".into())),
        Some("resolved") => {
            let resolution = &value["resolution"];
            if resolution["approved"] == false
                || resolution["choice_id"] == "deny"
                || value["cancelled"] == true
            {
                Ok(InputControlStatus::Resolved { text: None })
            } else if resolution
                .get("approved")
                .is_none_or(|value| value.is_null() || value == true)
                && resolution
                    .get("choice_id")
                    .is_none_or(|value| value.is_null() || value == "once")
            {
                resolution["text"]
                    .as_str()
                    .map(|text| InputControlStatus::Resolved {
                        text: Some(text.into()),
                    })
                    .ok_or_else(|| ClaudeApprovalError::Protocol("input reply has no text".into()))
            } else {
                Err(ClaudeApprovalError::Protocol(
                    "invalid input resolution".into(),
                ))
            }
        }
        _ => Err(ClaudeApprovalError::Protocol("invalid input status".into())),
    }
}

/// A handled hook suppresses Claude's native dialog and its Result hook.
/// `None` means no decision: Claude should show its native dialog instead.
pub fn handle_elicitation(raw: &Value, config: &BridgeConfig) -> Option<HookOutcome> {
    let request = ElicitationRequest::parse(raw)?;
    let mut control = SocketInputControl::new(config, &request.session_id);
    Some(handle_elicitation_with(
        &request,
        config,
        &mut control,
        None,
    ))
}

pub fn handle_elicitation_with(
    request: &ElicitationRequest,
    config: &BridgeConfig,
    control: &mut impl InputControl,
    stop: Option<&AtomicBool>,
) -> HookOutcome {
    let started = Instant::now();
    let interrupted = || {
        stop.is_some_and(|stop| stop.load(Ordering::Acquire)) || started.elapsed() >= config.timeout
    };
    let fail = |error: String| HookOutcome {
        output: cancel_elicitation_response(),
        diagnostic: Some(error),
    };
    let status = match control.register(request, config) {
        Ok(status) => status,
        Err(error) => return fail(error.to_string()),
    };
    let mut pending = match &status {
        InputControlStatus::Pending { request_id } => Some(request_id.clone()),
        InputControlStatus::Resolved { .. } => None,
    };
    let mut status = status;
    loop {
        if interrupted() {
            if let Some(id) = pending.as_deref() {
                let _ = control.cancel(id, "hook_interrupted_or_timeout");
            }
            return fail("Claude elicitation bridge interrupted or timed out".into());
        }
        match status {
            InputControlStatus::Resolved { text } => {
                return match request.form.response(text.as_deref()) {
                    Ok(output) => HookOutcome {
                        output,
                        diagnostic: None,
                    },
                    Err(error) => fail(error),
                };
            }
            InputControlStatus::Pending { request_id } => {
                if request_id.is_empty() || pending.as_ref().is_some_and(|id| id != &request_id) {
                    if let Some(id) = pending.as_deref() {
                        let _ = control.cancel(id, "protocol_error");
                    }
                    return fail("Claude input id changed while polling".into());
                }
                pending = Some(request_id);
            }
        }
        let id = pending.as_deref().expect("pending input has an id");
        status = match control.poll(id) {
            Ok(status) => status,
            Err(error) => {
                let _ = control.cancel(id, "control_disconnected");
                return fail(error.to_string());
            }
        };
        if matches!(status, InputControlStatus::Pending { .. }) {
            std::thread::sleep(
                config
                    .poll_interval
                    .min(config.timeout.saturating_sub(started.elapsed())),
            );
        }
    }
}

/// Result hooks only observe native responses. Missing IDs cannot safely match
/// a request, because multiple concurrent forms may share a session and server.
pub fn reconcile_elicitation_result(
    raw: &Value,
    config: &BridgeConfig,
) -> Result<(), ClaudeApprovalError> {
    if raw["hook_event_name"] != "ElicitationResult"
        || !matches!(
            raw["action"].as_str(),
            Some("accept" | "decline" | "cancel")
        )
    {
        return Ok(());
    }
    let Some(session_id) = raw["session_id"].as_str().filter(|id| !id.is_empty()) else {
        return Ok(());
    };
    let Some(server) = raw["mcp_server_name"].as_str().filter(|id| !id.is_empty()) else {
        return Ok(());
    };
    let Some(id) = raw["elicitation_id"].as_str().filter(|id| !id.is_empty()) else {
        return Ok(());
    };
    let control = SocketInputControl::new(config, session_id);
    let mut params = control.context.clone();
    params["mcp_server_name"] = json!(server);
    params["elicitation_id"] = json!(id);
    control.rpc("agent.input.reconcile", params).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    fn raw() -> Value {
        json!({"hook_event_name":"Elicitation","session_id":"session","mcp_server_name":"server","message":"Configure","requested_schema":{"type":"object","required":["count","enabled"],"properties":{"count":{"type":"integer"},"enabled":{"type":"boolean"},"label":{"type":"string"}}}})
    }
    fn config() -> BridgeConfig {
        let mut config = BridgeConfig::new(
            PathBuf::from("unused"),
            "workspace".into(),
            "surface".into(),
            "lifecycle".into(),
        );
        config.poll_interval = Duration::ZERO;
        config
    }
    struct Fake {
        statuses: VecDeque<InputControlStatus>,
        cancelled: Vec<String>,
    }
    impl InputControl for Fake {
        fn register(
            &mut self,
            _: &ElicitationRequest,
            _: &BridgeConfig,
        ) -> Result<InputControlStatus, ClaudeApprovalError> {
            Ok(self.statuses.pop_front().unwrap())
        }
        fn poll(&mut self, _: &str) -> Result<InputControlStatus, ClaudeApprovalError> {
            self.statuses
                .pop_front()
                .ok_or_else(|| ClaudeApprovalError::Control("lost socket".into()))
        }
        fn cancel(&mut self, id: &str, _: &str) -> Result<(), ClaudeApprovalError> {
            self.cancelled.push(id.into());
            Ok(())
        }
    }
    #[test]
    fn native_fallback_for_url_secrets_and_constraints() {
        for (path, value) in [
            ("/mode", json!("url")),
            (
                "/requested_schema/properties/label/format",
                json!("password"),
            ),
            ("/requested_schema/properties/count/minimum", json!(1)),
        ] {
            let mut value_raw = raw();
            if let Some(slot) = value_raw.pointer_mut(path) {
                *slot = value;
            } else if path == "/mode" {
                value_raw["mode"] = value;
            } else {
                let (parent, key) = path.rsplit_once('/').unwrap();
                value_raw.pointer_mut(parent).unwrap()[key] = value;
            }
            assert!(ElicitationRequest::parse(&value_raw).is_none());
        }
    }

    #[test]
    fn native_identity_deduplicates_hooks_but_missing_ids_remain_distinct() {
        let mut raw = raw();
        let first = ElicitationRequest::parse(&raw).unwrap();
        let second = ElicitationRequest::parse(&raw).unwrap();
        assert_ne!(first.form.id, second.form.id);
        raw["elicitation_id"] = json!("native");
        let first = ElicitationRequest::parse(&raw).unwrap();
        assert_eq!(first, ElicitationRequest::parse(&raw).unwrap());
        for key in ["session_id", "mcp_server_name", "elicitation_id"] {
            let mut another = raw.clone();
            another[key] = json!("different");
            assert_ne!(
                first.form.id,
                ElicitationRequest::parse(&another).unwrap().form.id
            );
        }
    }
    #[test]
    fn centralized_answer_skips_native_ui_with_typed_values() {
        let request = ElicitationRequest::parse(&raw()).unwrap();
        let mut control = Fake {
            statuses: VecDeque::from([
                InputControlStatus::Pending {
                    request_id: "a".into(),
                },
                InputControlStatus::Resolved {
                    text: Some(r#"{"count":"3","enabled":"false","label":"中文🙂"}"#.into()),
                },
            ]),
            cancelled: vec![],
        };
        let outcome = handle_elicitation_with(&request, &config(), &mut control, None);
        assert_eq!(
            outcome.output,
            json!({"hookSpecificOutput":{"hookEventName":"Elicitation","action":"accept","content":{"count":3,"enabled":false,"label":"中文🙂"}}})
        );
        let params = request.registration_params(&config());
        assert_eq!(params["session_id"], "session");
        assert_eq!(params["lifecycle_id"], "lifecycle");
        assert_eq!(params["request"]["params"]["thread_id"], "session");
    }
    #[test]
    fn malformed_reply_disconnect_and_identity_change_cancel() {
        let request = ElicitationRequest::parse(&raw()).unwrap();
        for reply in [
            None,
            Some(InputControlStatus::Pending {
                request_id: "wrong".into(),
            }),
            Some(InputControlStatus::Resolved {
                text: Some(r#"{"count":"3.5","enabled":"false"}"#.into()),
            }),
        ] {
            let mut statuses = VecDeque::from([InputControlStatus::Pending {
                request_id: "a".into(),
            }]);
            statuses.extend(reply);
            let mut control = Fake {
                statuses,
                cancelled: vec![],
            };
            let outcome = handle_elicitation_with(&request, &config(), &mut control, None);
            assert_eq!(outcome.output["hookSpecificOutput"]["action"], "cancel");
            assert!(outcome.diagnostic.is_some());
        }
    }
    #[test]
    fn deadline_and_stop_never_accept_even_immediate_resolution() {
        let request = ElicitationRequest::parse(&raw()).unwrap();
        let mut config = config();
        config.timeout = Duration::ZERO;
        let mut control = Fake {
            statuses: VecDeque::from([InputControlStatus::Resolved {
                text: Some(r#"{"count":"3","enabled":"true"}"#.into()),
            }]),
            cancelled: vec![],
        };
        assert_eq!(
            handle_elicitation_with(&request, &config, &mut control, None).output,
            cancel_elicitation_response()
        );
        let stop = AtomicBool::new(true);
        control.statuses.push_back(InputControlStatus::Pending {
            request_id: "a".into(),
        });
        assert_eq!(
            handle_elicitation_with(&request, &config, &mut control, Some(&stop)).output,
            cancel_elicitation_response()
        );
    }
    #[test]
    fn denied_native_results_are_never_accepted_or_ambiguously_reconciled() {
        for action in ["accept", "decline", "cancel"] {
            let raw = json!({"hook_event_name":"ElicitationResult","session_id":"session","mcp_server_name":"server","action":action});
            assert!(reconcile_elicitation_result(&raw, &config()).is_ok());
        }
    }

    #[test]
    fn text_only_replies_work_but_denial_and_unknown_choices_win() {
        let text = r#"{"count":"3","enabled":"true"}"#;
        assert_eq!(
            parse_status(&json!({"status":"resolved","resolution":{"text":text}})).unwrap(),
            InputControlStatus::Resolved {
                text: Some(text.into())
            }
        );
        for resolution in [
            json!({"text":text,"approved":false}),
            json!({"text":text,"choice_id":"deny"}),
        ] {
            assert_eq!(
                parse_status(&json!({"status":"resolved","resolution":resolution})).unwrap(),
                InputControlStatus::Resolved { text: None }
            );
        }
        assert!(
            parse_status(
                &json!({"status":"resolved","resolution":{"text":text,"choice_id":"always"}})
            )
            .is_err()
        );
        assert!(parse_status(&json!({"status":"resolved","resolution":{}})).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn socket_bridge_binds_every_call_and_reconciles_exact_native_identity() {
        use std::sync::{Arc, Mutex};
        let path =
            std::env::temp_dir().join(format!("wmcterm-claude-input-{}.sock", next_request_id()));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let _server = wmcterm_ipc::UnixServer::spawn(&path, move |request| {
            observed.lock().unwrap().push((request.method.clone(),request.params.clone()));
            let result = match request.method.as_str() {
                "agent.input.register" => json!({"status":"pending","approval_id":"central-request"}),
                "agent.input.poll" => json!({"status":"resolved","resolution":{"text":r#"{"count":"3","enabled":"true"}"#}}),
                "agent.input.reconcile" => json!({"cancelled":true}),
                _ => panic!("unexpected RPC {}",request.method),
            };
            wmcterm_ipc::Response::success(request.id,result)
        }).unwrap();
        let mut config = config();
        config.control_socket = path;
        let outcome = handle_elicitation(&raw(), &config).unwrap();
        assert_eq!(outcome.output["hookSpecificOutput"]["action"], "accept");
        let result = json!({"hook_event_name":"ElicitationResult","session_id":"session","mcp_server_name":"server","elicitation_id":"native-id","action":"cancel"});
        reconcile_elicitation_result(&result, &config).unwrap();
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 3);
        for (_, params) in calls.iter() {
            assert_eq!(params["workspace_id"], "workspace");
            assert_eq!(params["surface_id"], "surface");
            assert_eq!(params["lifecycle_id"], "lifecycle");
            assert_eq!(params["session_id"], "session");
        }
        assert_eq!(calls[1].1["request_id"], "central-request");
        assert_eq!(calls[2].1["elicitation_id"], "native-id");
        assert_eq!(calls[2].1["mcp_server_name"], "server");
        assert!(calls[2].1.get("action").is_none());
    }
}
