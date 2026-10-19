use super::*;

fn registration(app: &WmctermApplication, surface: SurfaceId, id: &str) -> Value {
    let form = wmcterm_codex::InputRequest {
        id: json!(id),
        method: "mcpServer/elicitation/request".into(),
        thread_id: "input-regression".into(),
        turn_id: "turn".into(),
        item_id: id.into(),
        message: "Configure".into(),
        fields: vec![
            wmcterm_codex::InputField {
                id: "count".into(),
                label: "Count".into(),
                kind: "integer".into(),
                required: true,
                options: vec![],
            },
            wmcterm_codex::InputField {
                id: "note".into(),
                label: "备注".into(),
                kind: "string".into(),
                required: false,
                options: vec![],
            },
            wmcterm_codex::InputField {
                id: "enabled".into(),
                label: "Enabled".into(),
                kind: "boolean".into(),
                required: true,
                options: vec!["true".into(), "false".into()],
            },
        ],
    };
    json!({"source":"codex_app_server", "workspace_id":app.state.workspaces[0].id, "surface_id":surface,
        "request":{"jsonrpc_id":id,"method":form.method,"kind":"input","thread_id":form.thread_id,
            "turn_id":form.turn_id,"item_id":id,"params":form}, "decisions":["deny","once"]})
}

fn register(app: &mut WmctermApplication, surface: SurfaceId, id: &str) -> String {
    let params = registration(app, surface, id);
    app.register_agent_input(&params).unwrap()["approval_id"]
        .as_str()
        .unwrap()
        .into()
}

fn submit(app: &mut WmctermApplication, text: &str) {
    app.prompt.as_mut().unwrap().editor = TextEditor::new(text.into());
    app.submit_prompt();
}

// Share the existing isolated winit test; only one event loop is allowed per process.
pub(super) fn check(app: &mut WmctermApplication, terminal: TerminalId, surface: SurfaceId) {
    for request in app.pending_agent_request_list() {
        app.resolve_agent_request_from_ui(request.request_id, AgentApprovalAction::Deny);
    }
    let response = app.handle_rpc(Request { id: "input-session".into(), method: "agent.update".into(),
        params: json!({"event":"session_start", "agent":"codex", "session_id":"input-regression",
            "surface":surface,"surface_id":surface,"lifecycle_id":app.terminals[&terminal].lifecycle_id}) });
    assert!(response.error.is_none(), "{:?}", response.error);
    app.window_focused = true;
    app.focus_input_quarantine = false;
    app.workspace_search_focused = false;
    app.search_visible = false;
    app.palette_visible = false;
    app.config.agent.notifications = false;

    let id = register(app, surface, "multi");
    let params = registration(app, surface, "multi");
    assert_eq!(
        app.register_agent_input(&params).unwrap()["approval_id"],
        id
    );
    let deadline = app
        .pending_agent_requests
        .get(&id)
        .unwrap()
        .unwrap()
        .deadline_unix_ms
        .unwrap();
    assert!(deadline <= unix_time_ms() + 9 * 60 * 1000);
    let mut conflict = params.clone();
    conflict["request"]["params"]["fields"][0]["label"] = json!("Different");
    assert!(
        app.register_agent_input(&conflict)
            .unwrap_err()
            .contains("conflicting")
    );
    let mut mismatch = params.clone();
    mismatch["request"]["turn_id"] = json!("wrong-turn");
    assert!(app.register_agent_input(&mismatch).is_err());
    mismatch = params.clone();
    mismatch["workspace_id"] = json!(uuid::Uuid::new_v4());
    assert!(app.register_agent_input(&mismatch).is_err());

    app.resolve_agent_request_from_ui(id.clone(), AgentApprovalAction::AllowOnce);
    submit(app, "3.5");
    assert!(app.prompt.as_ref().unwrap().error.is_some());
    assert_eq!(app.agent_input_edit.as_ref().unwrap().1, 0);
    submit(app, "3");
    assert_eq!(app.agent_input_edit.as_ref().unwrap().1, 1);
    // Exercise the same preedit/commit handlers as winit IME events.
    app.update_ime_preedit("zhongwen");
    assert_eq!(app.prompt.as_ref().unwrap().editor.text(), "");
    assert!(app.prompt.as_ref().unwrap().editor.has_preedit());
    app.commit_ime_text("中文🙂e\u{301}".into());
    assert_eq!(app.prompt.as_ref().unwrap().editor.text(), "中文🙂e\u{301}");
    assert!(!app.prompt.as_ref().unwrap().editor.has_preedit());
    app.submit_prompt();
    submit(app, "not-a-boolean");
    assert!(app.prompt.as_ref().unwrap().error.is_some());
    submit(app, "false");
    assert!(app.prompt.is_none());
    let reply = app.poll_agent_input(&json!({"request_id":id})).unwrap();
    let answers: Value =
        serde_json::from_str(reply["resolution"]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        answers,
        json!({"count":"3","note":"中文🙂e\u{301}","enabled":"false"})
    );

    let id = register(app, surface, "optional");
    app.resolve_agent_request_from_ui(id.clone(), AgentApprovalAction::AllowOnce);
    submit(app, "1");
    submit(app, "");
    submit(app, "true");
    let reply = app.poll_agent_input(&json!({"request_id":id})).unwrap();
    let form = app.agent_input_forms.get(&id).unwrap();
    assert_eq!(
        form.response(reply["resolution"]["text"].as_str()).unwrap()["result"]["content"],
        json!({"count":1,"enabled":true})
    );

    for cancel in [false, true] {
        let id = register(
            app,
            surface,
            if cancel {
                "editing-cancel"
            } else {
                "editing-expire"
            },
        );
        app.resolve_agent_request_from_ui(id.clone(), AgentApprovalAction::AllowOnce);
        if cancel {
            app.pending_agent_requests
                .cancel(&id, "test_cancel", unix_time_ms())
                .unwrap();
        } else {
            app.pending_agent_requests.expire(u64::MAX).unwrap();
        }
        submit(app, "5");
        assert!(app.agent_input_edit.is_none());
        assert!(app.prompt.is_none());
        let reply = app.poll_agent_input(&json!({"request_id":id})).unwrap();
        assert_eq!(
            reply["status"],
            if cancel { "cancelled" } else { "expired" }
        );
    }

    // Same upstream request ID on different threads must remain independent.
    let a = register(app, surface, "isolated");
    let mut other = registration(app, surface, "isolated");
    other["request"]["thread_id"] = json!("other-thread");
    other["request"]["params"]["thread_id"] = json!("other-thread");
    let b = app.register_agent_input(&other).unwrap()["approval_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(a, b);
    app.resolve_agent_request_from_ui(a, AgentApprovalAction::Deny);
    assert_eq!(
        app.poll_agent_input(&json!({"request_id":b})).unwrap()["status"],
        "pending"
    );
    app.resolve_agent_request_from_ui(b, AgentApprovalAction::Deny);

    check_layout(app, surface);
    // Fill the real pending/form stores, then reclaim one completed form.
    let mut ids = Vec::new();
    for index in 0..256 {
        ids.push(register(app, surface, &format!("capacity-{index}")));
    }
    assert_eq!(
        register(app, surface, "capacity-0"),
        ids[0],
        "duplicates must remain idempotent at capacity"
    );
    let overflow = registration(app, surface, "overflow");
    assert!(
        app.register_agent_input(&overflow)
            .unwrap_err()
            .contains("too many retained")
    );
    app.resolve_agent_request_from_ui(ids.remove(0), AgentApprovalAction::Deny);
    let recovered = app.register_agent_input(&overflow).unwrap()["approval_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(app.pending_agent_requests.len().unwrap(), 256);
    assert_eq!(app.agent_input_forms.len(), 256);
    for id in ids.into_iter().chain([recovered]) {
        app.resolve_agent_request_from_ui(id, AgentApprovalAction::Deny);
    }
    assert!(app.pending_agent_requests.is_empty().unwrap());
    // Completed-reply history is bounded; after eviction, the same request can register anew.
    app.pending_agent_requests = PendingAgentRequests::new(1).unwrap();
    app.agent_input_forms.clear();
    let first = register(app, surface, "history-0");
    app.resolve_agent_request_from_ui(first.clone(), AgentApprovalAction::Deny);
    for index in 1..65 {
        let id = register(app, surface, &format!("history-{index}"));
        app.resolve_agent_request_from_ui(id, AgentApprovalAction::Deny);
    }
    assert!(
        app.pending_agent_requests
            .completed_reply(&first)
            .unwrap()
            .is_none()
    );
    let mut updated = registration(app, surface, "history-0");
    updated["request"]["params"]["fields"][0]["label"] = json!("Updated after eviction");
    let renewed = app.register_agent_input(&updated).unwrap()["approval_id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(renewed, first);
    assert_eq!(
        app.agent_input_forms[&renewed].fields[0].label,
        "Updated after eviction"
    );
    assert_eq!(
        app.poll_agent_input(&json!({"request_id":renewed}))
            .unwrap()["status"],
        "pending"
    );
    app.resolve_agent_request_from_ui(renewed, AgentApprovalAction::Deny);
}

fn check_layout(app: &mut WmctermApplication, surface: SurfaceId) {
    let mut params = registration(app, surface, "layout");
    params["request"]["params"]["fields"] = json!([{"id":"choice", "label":"很长的问题🙂".repeat(100),
        "kind":"string","required":true,"options":(0..32).map(|index|format!("选项{index} {}", "long text".repeat(20))).collect::<Vec<_>>()}]);
    let id = app.register_agent_input(&params).unwrap()["approval_id"]
        .as_str()
        .unwrap()
        .to_owned();
    app.resolve_agent_request_from_ui(id.clone(), AgentApprovalAction::AllowOnce);
    for scale in [1.0, 1.5, 2.0] {
        app.scale_factor = scale;
        for size in [PhysicalSize::new(640, 480), PhysicalSize::new(1000, 650)] {
            app.window_size = size;
            for selected in [0, 5, 6, 31] {
                let text = app.agent_input_options()[selected].clone();
                app.prompt.as_mut().unwrap().editor = TextEditor::new(text);
                let panel = app.prompt_panel();
                let input = app.prompt_input_bounds();
                let rows = app.agent_input_option_rows();
                assert_eq!(rows.len(), 6);
                assert!(rows.iter().any(|(index, _)| *index == selected));
                for (_, row) in &rows {
                    assert!(row.x >= panel.x && row.x + row.width <= panel.x + panel.width);
                    assert!(row.y >= input.y + input.height);
                    assert!(row.y + row.height <= panel.y + panel.height);
                    assert!(row.y + row.height <= size.height as f32);
                }
                assert!(
                    rows.windows(2)
                        .all(|pair| pair[0].1.y + pair[0].1.height <= pair[1].1.y)
                );
            }
        }
    }
    app.prompt = None;
    app.agent_input_edit = None;
    app.resolve_agent_request_from_ui(id, AgentApprovalAction::Deny);
    app.scale_factor = 1.0;
}
