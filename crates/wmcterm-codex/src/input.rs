use super::*;

pub use wmcterm_ipc::InputField;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InputRequest {
    pub id: Value,
    pub method: String,
    pub thread_id: String,
    pub turn_id: String,
    pub item_id: String,
    pub message: String,
    pub fields: Vec<InputField>,
}

impl InputRequest {
    fn shared_form(&self) -> wmcterm_ipc::InputFormRequest {
        wmcterm_ipc::InputFormRequest {
            id: self.id.clone(),
            method: self.method.clone(),
            thread_id: self.thread_id.clone(),
            turn_id: self.turn_id.clone(),
            item_id: self.item_id.clone(),
            message: self.message.clone(),
            fields: self.fields.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if !matches!(
            self.method.as_str(),
            "item/tool/requestUserInput" | "mcpServer/elicitation/request"
        ) {
            return Err("invalid Codex input method".into());
        }
        self.shared_form().validate()
    }
    /// Unsupported forms stay in the native Codex UI, including secret fields.
    pub fn parse(value: &Value) -> Option<Self> {
        let method = value["method"].as_str()?;
        if !matches!(
            method,
            "item/tool/requestUserInput" | "mcpServer/elicitation/request"
        ) {
            return None;
        }
        if serde_json::to_vec(value).ok()?.len() > 64 * 1024 {
            return None;
        }
        let id = value.get("id")?.clone();
        if !id.is_string() && !id.is_number() {
            return None;
        }
        let params = &value["params"];
        let mut fields = Vec::new();
        let message;
        if method == "item/tool/requestUserInput" {
            message = "Codex".into();
            for question in params["questions"].as_array()? {
                if question["isSecret"].as_bool().unwrap_or(false) {
                    return None;
                }
                fields.push(InputField {
                    id: question["id"].as_str()?.into(),
                    label: question["question"].as_str()?.into(),
                    kind: "answer".into(),
                    required: true,
                    options: question["options"]
                        .as_array()
                        .map(|options| {
                            options
                                .iter()
                                .filter_map(|option| option["label"].as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                });
            }
        } else {
            if params["mode"].as_str()? != "form" {
                return None;
            }
            message = params["message"].as_str()?.to_owned();
            fields = wmcterm_ipc::InputFormRequest::parse_schema(&params["requestedSchema"])?;
        }
        if fields.is_empty() || fields.len() > 32 {
            return None;
        }
        let mut ids = HashSet::new();
        for field in &fields {
            if field.id.is_empty()
                || field.id.len() > 256
                || field.label.len() > 4096
                || field.options.len() > 32
                || !ids.insert(field.id.clone())
            {
                return None;
            }
        }
        let request = Self {
            id,
            method: method.into(),
            thread_id: params["threadId"].as_str()?.into(),
            turn_id: params["turnId"].as_str().unwrap_or("elicitation").into(),
            item_id: params["itemId"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value["id"].to_string()),
            message,
            fields,
        };
        request.validate().ok()?;
        Some(request)
    }

    pub fn field_value(&self, index: usize, text: &str) -> Result<Value, String> {
        self.shared_form().field_value(index, text)
    }

    pub fn response(&self, text: Option<&str>) -> Result<Value, String> {
        self.shared_form().response(text)
    }

    fn registration(&self, config: &BridgeConfig) -> Value {
        json!({"source": "codex_app_server", "workspace_id": config.workspace_id, "surface_id": config.root_surface_id,
            "request": {"jsonrpc_id": self.id, "method": self.method, "kind": "input", "thread_id": self.thread_id, "turn_id": self.turn_id, "item_id": self.item_id, "params": self},
            "decisions": ["deny", "once"]})
    }
}

#[derive(Default)]
pub(super) struct InputBridge {
    pending: HashMap<String, InputRequest>,
}

impl InputBridge {
    fn rpc(config: &BridgeConfig, method: &str, params: Value) -> Result<Value, BridgeError> {
        let response = call(
            &config.control_socket,
            &Request {
                id: format!("codex-input-{}", next_rpc_id()),
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
            .ok_or_else(|| BridgeError::Control("missing input response".into()))
    }

    pub fn begin(
        &mut self,
        config: &BridgeConfig,
        request: InputRequest,
    ) -> Result<(), BridgeError> {
        if self.pending.len() >= 64 {
            return Err(BridgeError::Control("too many input requests".into()));
        }
        let response = Self::rpc(config, "agent.input.register", request.registration(config))?;
        let id = response["approval_id"]
            .as_str()
            .ok_or_else(|| BridgeError::Control("missing input request id".into()))?;
        self.pending.entry(id.into()).or_insert(request);
        Ok(())
    }

    pub fn poll(&mut self, config: &BridgeConfig, socket: &mut Socket) -> Result<(), BridgeError> {
        for id in self.pending.keys().cloned().collect::<Vec<_>>() {
            let request = &self.pending[&id];
            let response = Self::rpc(config, "agent.input.poll", json!({"request_id": id}));
            let result = match response {
                Ok(value) if value["status"] == "pending" => continue,
                Ok(value) => {
                    let cancelled = value["resolution"]["approved"] == false
                        || value["resolution"]["choice_id"] == "deny";
                    request
                        .response(if cancelled {
                            None
                        } else {
                            value["resolution"]["text"].as_str()
                        })
                        .unwrap_or_else(|_| request.response(None).expect("cancel serializes"))
                }
                Err(_) => {
                    let _ = Self::rpc(config, "agent.request.cancel", json!({"request_id": id}));
                    request.response(None).expect("cancel serializes")
                }
            };
            send_json(socket, &result)?;
            self.pending.remove(&id);
        }
        Ok(())
    }

    pub fn reconcile(&mut self, config: &BridgeConfig, thread: &str, id: &Value) {
        let ids = self
            .pending
            .iter()
            .filter(|(_, request)| request.thread_id == thread && request.id == *id)
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            let _ = Self::rpc(
                config,
                "agent.request.cancel",
                json!({"request_id": id, "reason": "resolved_by_codex_client"}),
            );
            self.pending.remove(&id);
        }
    }

    pub fn complete_turn(&mut self, config: &BridgeConfig, thread: &str, turn: &str) {
        let ids = self
            .pending
            .values()
            .filter(|request| request.thread_id == thread && request.turn_id == turn)
            .map(|request| request.id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.reconcile(config, thread, &id);
        }
    }

    pub fn cancel_all(&mut self, config: &BridgeConfig) {
        for (id, _) in self.pending.drain() {
            let _ = Self::rpc(
                config,
                "agent.request.cancel",
                json!({"request_id": id, "reason": "codex_disconnected"}),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(path: std::path::PathBuf) -> BridgeConfig {
        BridgeConfig {
            endpoint: "ws://127.0.0.1:1".into(),
            control_socket: path,
            workspace_id: "workspace".into(),
            root_surface_id: "surface".into(),
            codex_program: "codex".into(),
            workspace_cwd: std::env::temp_dir(),
            max_auto_depth: 2,
        }
    }

    fn socket_pair() -> (Socket, tungstenite::WebSocket<std::net::TcpStream>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let accepted =
            std::thread::spawn(move || tungstenite::accept(listener.accept().unwrap().0).unwrap());
        let (client, _) = connect(endpoint).unwrap();
        (client, accepted.join().unwrap())
    }

    fn questions() -> Value {
        json!({"id": "q", "method": "item/tool/requestUserInput", "params": {"threadId": "thread", "turnId": "turn", "itemId": "item", "questions": [
            {"id": "target", "question": "目标？\nChoose a target", "options": [{"label": "生产环境"}]},
            {"id": "reason", "question": "Reason", "options": null}
        ]}})
    }

    #[test]
    fn multiple_questions_preserve_unicode_and_exact_routing() {
        let request = InputRequest::parse(&questions()).unwrap();
        assert_eq!(request.fields[0].options, ["生产环境"]);
        assert_eq!(
            request
                .response(Some(r#"{"target":"生产环境","reason":"测试🙂"}"#))
                .unwrap(),
            json!({"id":"q", "result":{"answers":{"target":{"answers":["生产环境"]},"reason":{"answers":["测试🙂"]}}}})
        );
        assert_eq!(
            request.response(None).unwrap()["result"],
            json!({"answers":{}})
        );
        for text in [
            r#"{"target":"prod"}"#,
            r#"{"target":1,"reason":"test"}"#,
            r#"{"target":"prod","reason":"test","extra":"bad"}"#,
        ] {
            assert!(request.response(Some(text)).is_err());
        }
    }

    #[test]
    fn secrets_unknown_forms_and_duplicate_question_ids_stay_native() {
        let mut value = questions();
        value["params"]["questions"][0]["isSecret"] = json!(true);
        assert!(InputRequest::parse(&value).is_none());
        value["params"]["questions"][0]["isSecret"] = json!(false);
        value["params"]["questions"][1]["id"] = json!("target");
        assert!(InputRequest::parse(&value).is_none());
        assert!(InputRequest::parse(&json!({"id":1,"method":"mcpServer/elicitation/request","params":{"threadId":"t","mode":"url","url":"https://example.com"}})).is_none());
    }

    #[test]
    fn mcp_form_validates_types_options_and_optional_fields() {
        let mut value = json!({"id": 7,"method":"mcpServer/elicitation/request","params":{"threadId":"thread","mode":"form","message":"Configure", "requestedSchema":{"type":"object","required":["count","enabled","mode"],"properties":{"count":{"type":"integer"},"enabled":{"type":"boolean"},"mode":{"type":"string","enum":["safe","fast"]},"note":{"type":"string"}}}}});
        let request = InputRequest::parse(&value).unwrap();
        assert_eq!(
            request
                .response(Some(r#"{"count":"3","enabled":"false","mode":"safe"}"#))
                .unwrap()["result"],
            json!({"action":"accept","content":{"count":3,"enabled":false,"mode":"safe"},"_meta":null})
        );
        for text in [
            r#"{"count":"3.5","enabled":"false","mode":"safe"}"#,
            r#"{"count":"3","enabled":"yes","mode":"safe"}"#,
            r#"{"count":"3","enabled":"false","mode":"unsafe"}"#,
        ] {
            assert!(request.response(Some(text)).is_err());
        }
        assert_eq!(
            request.response(None).unwrap()["result"]["action"],
            "cancel"
        );
        value["params"]["requestedSchema"]["properties"]["mode"]["enum"] = json!([]);
        assert!(InputRequest::parse(&value).is_none());
        value["params"]["requestedSchema"]["properties"]["mode"]["enum"] = json!(["safe"]);
        value["params"]["requestedSchema"]["required"] = json!(["missing"]);
        assert!(InputRequest::parse(&value).is_none());
        value["params"]["requestedSchema"]["required"] = json!(["count"]);
        value["params"]["requestedSchema"]["properties"]["count"]["minimum"] = json!(0);
        assert!(InputRequest::parse(&value).is_none());
    }

    #[test]
    fn input_limits_and_numeric_boundaries_are_enforced() {
        let request = InputRequest::parse(&questions()).unwrap();
        for count in [0, 33] {
            let mut form = request.clone();
            form.fields = (0..count)
                .map(|index| InputField {
                    id: index.to_string(),
                    ..request.fields[0].clone()
                })
                .collect();
            assert!(form.validate().is_err());
        }
        let mut form = request.clone();
        form.fields = (0..32)
            .map(|index| InputField {
                id: index.to_string(),
                ..request.fields[0].clone()
            })
            .collect();
        assert!(form.validate().is_ok());
        form.fields[0].options = vec!["x".into(); 33];
        assert!(form.validate().is_err());
        form.fields[0].options = vec!["x".repeat(4097)];
        assert!(form.validate().is_err());
        form.fields[0].options.clear();
        form.fields[0].kind = "number".into();
        for answer in ["NaN", "inf", "1e309", "abc"] {
            assert!(form.field_value(0, answer).is_err());
        }
        assert_eq!(form.field_value(0, "-1.25e2").unwrap(), json!(-125.0));
        form.fields[0].kind = "integer".into();
        assert!(form.field_value(0, "9223372036854775808").is_err());
        assert_eq!(
            form.field_value(0, "-9223372036854775808").unwrap(),
            json!(i64::MIN)
        );
        assert!(request.response(Some(&"x".repeat(65537))).is_err());
        assert!(request.field_value(0, &"界".repeat(1366)).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn bridge_faults_cancel_fail_closed_and_retain_unsent_answers_for_cleanup() {
        use std::sync::Mutex;
        for status in [
            "rpc_error",
            "cancelled",
            "expired",
            "invalid_answer",
            "pending",
        ] {
            let path = std::env::temp_dir().join(format!(
                "wmcterm-input-fault-{}-{}.sock",
                std::process::id(),
                next_rpc_id()
            ));
            let cancelled = Arc::new(Mutex::new(Vec::new()));
            let observed = cancelled.clone();
            let server = wmcterm_ipc::UnixServer::spawn(&path, move |request| {
                if request.method == "agent.request.cancel" {
                    observed
                        .lock()
                        .unwrap()
                        .push(request.params["request_id"].clone());
                    return wmcterm_ipc::Response::success(request.id, json!({}));
                }
                if status == "rpc_error" {
                    return wmcterm_ipc::Response::error(
                        request.id,
                        -32000,
                        "injected polling failure",
                    );
                }
                wmcterm_ipc::Response::success(
                    request.id,
                    json!({"status": if status == "invalid_answer" { "resolved" } else { status }, "resolution":{"text":"{}"}}),
                )
            })
            .unwrap();
            let config = config(path);
            let mut bridge = InputBridge::default();
            let request = InputRequest::parse(&questions()).unwrap();
            bridge.pending.insert("one".into(), request.clone());
            let (mut client, mut peer) = socket_pair();
            bridge.poll(&config, &mut client).unwrap();
            if status == "pending" {
                assert_eq!(bridge.pending.len(), 1);
                bridge.cancel_all(&config);
                assert_eq!(cancelled.lock().unwrap().len(), 1);
            } else {
                let Message::Text(text) = peer.read().unwrap() else {
                    panic!("expected cancellation")
                };
                assert_eq!(
                    serde_json::from_str::<Value>(&text).unwrap(),
                    request.response(None).unwrap()
                );
                assert!(bridge.pending.is_empty());
                assert_eq!(
                    cancelled.lock().unwrap().len(),
                    usize::from(status == "rpc_error")
                );
            }
            drop(server);
        }
        let path = std::env::temp_dir().join(format!(
            "wmcterm-input-send-fault-{}-{}.sock",
            std::process::id(),
            next_rpc_id()
        ));
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let observed = cancelled.clone();
        let _server = wmcterm_ipc::UnixServer::spawn(&path, move |request| {
            if request.method == "agent.request.cancel" { observed.lock().unwrap().push(request.params); }
            wmcterm_ipc::Response::success(request.id, json!({"status":"resolved", "resolution":{"text":r#"{"target":"test","reason":"ready"}"#}}))
        }).unwrap();
        let config = config(path);
        let mut bridge = InputBridge::default();
        bridge
            .pending
            .insert("one".into(), InputRequest::parse(&questions()).unwrap());
        let (mut client, mut peer) = socket_pair();
        client.close(None).unwrap();
        let _ = peer.read();
        assert!(bridge.poll(&config, &mut client).is_err());
        assert_eq!(
            bridge.pending.len(),
            1,
            "unsent reply must remain available for disconnect cleanup"
        );
        bridge.cancel_all(&config);
        assert_eq!(cancelled.lock().unwrap().len(), 1);
    }

    #[test]
    fn unavailable_control_preserves_native_input_until_a_registered_request_needs_cancelling() {
        let path = std::env::temp_dir().join(format!(
            "wmcterm-missing-input-{}-{}.sock",
            std::process::id(),
            next_rpc_id()
        ));
        let config = config(path);
        let (mut client, mut peer) = socket_pair();
        peer.get_mut()
            .set_read_timeout(Some(std::time::Duration::from_millis(50)))
            .unwrap();
        let mut watcher = CodexWatcher::new(config.clone());
        watcher
            .handle_server_value(&mut client, &questions())
            .unwrap();
        assert!(watcher.inputs.pending.is_empty());
        assert!(
            matches!(peer.read(), Err(tungstenite::Error::Io(error)) if matches!(error.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)),
            "registration failure must not cancel the native TUI request"
        );
        watcher.inputs.pending.insert(
            "registered-before-disconnect".into(),
            InputRequest::parse(&questions()).unwrap(),
        );
        watcher.inputs.poll(&config, &mut client).unwrap();
        let Message::Text(text) = peer.read().unwrap() else {
            panic!("expected cancellation after control disconnect")
        };
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            InputRequest::parse(&questions())
                .unwrap()
                .response(None)
                .unwrap()
        );
        assert!(watcher.inputs.pending.is_empty());
    }

    #[test]
    fn bridge_capacity_and_thread_turn_isolation() {
        let mut bridge = InputBridge::default();
        let request = InputRequest::parse(&questions()).unwrap();
        for index in 0..64 {
            let mut form = request.clone();
            form.id = json!(index);
            form.thread_id = format!("thread-{}", index % 2);
            bridge.pending.insert(index.to_string(), form);
        }
        let config = config(std::env::temp_dir().join(format!(
            "wmcterm-no-input-server-{}-{}.sock",
            std::process::id(),
            next_rpc_id()
        )));
        assert!(
            bridge
                .begin(&config, request)
                .unwrap_err()
                .to_string()
                .contains("too many")
        );
        bridge.complete_turn(&config, "thread-0", "other-turn");
        assert_eq!(bridge.pending.len(), 64);
        bridge.complete_turn(&config, "thread-0", "turn");
        assert_eq!(bridge.pending.len(), 32);
        assert!(
            bridge
                .pending
                .values()
                .all(|form| form.thread_id == "thread-1")
        );
        bridge.reconcile(&config, "thread-1", &json!(1));
        assert_eq!(bridge.pending.len(), 31);
        bridge.cancel_all(&config);
        assert!(bridge.pending.is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn bridge_polls_replies_and_reconciles_only_matching_requests() {
        use std::sync::Mutex;
        let path = std::env::temp_dir().join(format!(
            "wmcterm-input-{}-{}.sock",
            std::process::id(),
            next_rpc_id()
        ));
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let observed = cancelled.clone();
        let _server = wmcterm_ipc::UnixServer::spawn(&path, move |request| {
            let value = match request.method.as_str() {
                "agent.input.register" => json!({"approval_id":"input-one","status":"pending"}),
                "agent.input.poll" => json!({"status":"resolved","resolution":{"text": r#"{"target":"prod","reason":"test"}"#}}),
                "agent.request.cancel" => { observed.lock().unwrap().push(request.params["request_id"].clone()); json!({"ok":true}) },
                _ => panic!("unexpected RPC"),
            };
            wmcterm_ipc::Response::success(request.id, value)
        }).unwrap();
        let config = BridgeConfig {
            endpoint: "ws://127.0.0.1:1".into(),
            control_socket: path,
            workspace_id: "workspace".into(),
            root_surface_id: "surface".into(),
            codex_program: "codex".into(),
            workspace_cwd: std::env::temp_dir(),
            max_auto_depth: 2,
        };
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("ws://{}", listener.local_addr().unwrap());
        let received = std::thread::spawn(move || {
            let mut socket = tungstenite::accept(listener.accept().unwrap().0).unwrap();
            let Message::Text(text) = socket.read().unwrap() else {
                panic!("expected JSON response")
            };
            serde_json::from_str::<Value>(&text).unwrap()
        });
        let (mut socket, _) = connect(endpoint).unwrap();
        let request = InputRequest::parse(&questions()).unwrap();
        let mut bridge = InputBridge::default();
        bridge.begin(&config, request.clone()).unwrap();
        bridge.poll(&config, &mut socket).unwrap();
        assert!(bridge.pending.is_empty());
        assert_eq!(
            received.join().unwrap(),
            request
                .response(Some(r#"{"target":"prod","reason":"test"}"#))
                .unwrap()
        );
        bridge.begin(&config, request.clone()).unwrap();
        bridge.reconcile(&config, "another-thread", &request.id);
        assert_eq!(bridge.pending.len(), 1);
        bridge.complete_turn(&config, "thread", "another-turn");
        assert_eq!(bridge.pending.len(), 1);
        bridge.complete_turn(&config, "thread", "turn");
        assert!(bridge.pending.is_empty());
        bridge.begin(&config, request).unwrap();
        bridge.cancel_all(&config);
        assert!(bridge.pending.is_empty());
        assert_eq!(cancelled.lock().unwrap().len(), 2);
    }
}
