use super::*;
use wmcterm_core::AgentState;

fn hook(
    app: &WmctermApplication,
    terminal: TerminalId,
    session: &str,
    kind: &str,
    extra: Value,
) -> Value {
    let mut payload = json!({"session_id": session, "message": "regression notification"});
    payload
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    let hook = wmcterm_agent::normalize_claude_hook_event(Some(kind), &payload).unwrap();
    let mut params = serde_json::to_value(hook.to_agent_event()).unwrap();
    params["surface"] = app
        .surface_for_terminal(terminal)
        .unwrap()
        .to_string()
        .into();
    params["lifecycle_id"] = app.terminals[&terminal].lifecycle_id.clone().into();
    params["claude_hook"] = serde_json::to_value(hook).unwrap();
    params
}

fn send(app: &mut WmctermApplication, params: Value) -> Response {
    app.handle_rpc(Request {
        id: uuid::Uuid::new_v4().to_string(),
        method: "agent.update".into(),
        params,
    })
}

fn accept(
    app: &mut WmctermApplication,
    terminal: TerminalId,
    session: &str,
    kind: &str,
    extra: Value,
) {
    let params = hook(app, terminal, session, kind, extra);
    let response = send(app, params);
    assert!(response.error.is_none(), "{:?}", response.error);
}

fn snapshot(app: &WmctermApplication, surface: SurfaceId, terminal: TerminalId) -> Value {
    json!({"session": app.agent_sessions.get(&surface), "surface": app.state.surfaces[&surface],
        "mode": app.claude_hook_modes.get(&terminal), "notifications": app.state.notifications,
        "requests": app.pending_agent_request_list()})
}

// Called from the existing isolated application test: winit allows one event loop per process.
pub(super) fn check(app: &mut WmctermApplication, terminal: TerminalId, surface: SurfaceId) {
    app.config.agent.notifications = false;
    check_approvals(app, terminal, surface);
    check_ordering_and_isolation(app, terminal, surface);
    check_lifecycle(app, terminal, surface);
    check_session_approval_isolation(app, terminal, surface);
}

fn check_session_approval_isolation(
    app: &mut WmctermApplication,
    terminal: TerminalId,
    surface: SurfaceId,
) {
    let registration = |app: &WmctermApplication, session: &str, item: &str| {
        json!({
            "source": "claude_hook", "workspace_id": app.state.workspaces[0].id,
            "surface_id": surface, "lifecycle_id": app.terminals[&terminal].lifecycle_id,
            "request": {"jsonrpc_id": item, "method": "PermissionRequest", "kind": "permission",
                "thread_id": session, "turn_id": "turn", "item_id": item,
                "params": {"tool_name": "Bash", "tool_input": {"command": "pwd"}}},
            "decisions": ["deny", "once"]
        })
    };
    let params = registration(app, "legacy-session", "same-request");
    let approved = app.register_agent_approval(&params).unwrap()["approval_id"]
        .as_str()
        .unwrap()
        .to_owned();
    app.resolve_agent_request_from_ui(approved.clone(), AgentApprovalAction::AllowOnce);
    assert_eq!(
        app.register_agent_approval(&params).unwrap()["decision"]["mode"],
        "once"
    );
    let pending = app
        .register_agent_approval(&registration(app, "legacy-session", "pending"))
        .unwrap()["approval_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut new_start = hook(app, terminal, "new-session", "SessionStart", json!({}));
    new_start["hook_started_at_unix_ns"] = 20.into();
    assert!(send(app, new_start).error.is_none());
    assert!(
        app.register_agent_approval(&params)
            .unwrap_err()
            .contains("session")
    );
    assert!(app.pending_agent_requests.is_empty().unwrap());
    for id in [pending, approved.clone()] {
        assert_eq!(
            app.poll_agent_approval(&json!({"approval_id": id}))
                .unwrap()["decision"]["mode"],
            "deny"
        );
    }
    let mut old_start = hook(app, terminal, "legacy-session", "SessionStart", json!({}));
    let before = snapshot(app, surface, terminal);
    assert!(send(app, old_start.clone()).error.is_some());
    assert_eq!(snapshot(app, surface, terminal), before);
    old_start["hook_started_at_unix_ns"] = 10.into();
    assert!(send(app, old_start.clone()).error.is_some());
    assert_eq!(snapshot(app, surface, terminal), before);
    old_start["hook_started_at_unix_ns"] = 30.into();
    assert!(send(app, old_start).error.is_none());
    let resumed = app.register_agent_approval(&params).unwrap();
    assert_eq!(resumed["status"], "pending");
    assert_ne!(resumed["approval_id"], approved);
    app.spawn_terminal(terminal).unwrap();
    accept(app, terminal, "legacy-session", "SessionStart", json!({}));
    assert!(
        app.register_agent_approval(&params)
            .unwrap_err()
            .contains("lifecycle")
    );
    let restarted = app
        .register_agent_approval(&registration(app, "legacy-session", "same-request"))
        .unwrap();
    assert_eq!(restarted["status"], "pending");
    assert_ne!(restarted["approval_id"], resumed["approval_id"]);
    for request in app.pending_agent_request_list() {
        app.resolve_agent_request_from_ui(request.request_id, AgentApprovalAction::Deny);
    }
}

fn check_approvals(app: &mut WmctermApplication, terminal: TerminalId, surface: SurfaceId) {
    for (index, action) in [
        Some(AgentApprovalAction::AllowOnce),
        Some(AgentApprovalAction::Deny),
        None,
    ]
    .into_iter()
    .enumerate()
    {
        let registered = app.register_agent_approval(&json!({
            "source": "claude_hook", "workspace_id": app.state.workspaces[0].id,
            "surface_id": surface, "lifecycle_id": app.terminals[&terminal].lifecycle_id,
            "deadline_unix_ms": if action.is_none() { Some(1_u64) } else { None },
            "request": {"jsonrpc_id": format!("interleaved-{index}"), "method": "PermissionRequest",
                "kind": "permission", "thread_id": "claude-current", "turn_id": "hook", "item_id": "tool",
                "params": {"tool_name": "Bash", "tool_input": {"command": "pwd"}}},
            "decisions": ["deny", "once"]
        })).unwrap();
        let id = registered["approval_id"].as_str().unwrap().to_owned();
        for notification in [
            "auth_success",
            "elicitation_dialog",
            "agent_completed",
            "quota_auto_resume_fired",
        ] {
            accept(
                app,
                terminal,
                "claude-current",
                "Notification",
                json!({"notification_type": notification}),
            );
            app.mark_all_notifications_read();
            assert_eq!(app.pending_agent_requests.len().unwrap(), 1);
            assert_eq!(
                app.state.surfaces[&surface].agent.as_ref().unwrap().state,
                AgentState::WaitingForPermission
            );
            assert_eq!(
                app.poll_agent_approval(&json!({"approval_id": id}))
                    .unwrap()["status"],
                "pending"
            );
        }
        if let Some(action) = action {
            app.resolve_agent_request_from_ui(id.clone(), action);
            assert_eq!(
                app.poll_agent_approval(&json!({"approval_id": id}))
                    .unwrap()["decision"]["mode"],
                agent_approval_mode(action)
            );
            app.resolve_agent_request_from_ui(id.clone(), AgentApprovalAction::AlwaysAllow);
            assert_eq!(
                app.poll_agent_approval(&json!({"approval_id": id}))
                    .unwrap()["decision"]["mode"],
                agent_approval_mode(action)
            );
        } else {
            assert!(app.expire_agent_requests());
            app.resolve_agent_request_from_ui(id.clone(), AgentApprovalAction::AllowOnce);
            assert!(!app.agent_approval_results.contains_key(&id));
        }
        assert!(app.pending_agent_requests.is_empty().unwrap());
        assert_eq!(app.agent_sessions[&surface].state, AgentState::Running);
        assert_eq!(
            app.state.surfaces[&surface].agent.as_ref().unwrap().state,
            AgentState::Running
        );
    }
}

fn check_ordering_and_isolation(
    app: &mut WmctermApplication,
    terminal: TerminalId,
    surface: SurfaceId,
) {
    let pane = app.state.workspaces[0].active_pane;
    let created = app
        .rpc_create_surface(
            0,
            pane,
            TerminalCreateOptions {
                command: Some("exit".into()),
                ..Default::default()
            },
            false,
        )
        .unwrap();
    let other_surface: SurfaceId = serde_json::from_value(created["surface_id"].clone()).unwrap();
    let SurfaceKind::Terminal(other) = app.state.surfaces[&other_surface].kind else {
        panic!("terminal expected")
    };
    accept(
        app,
        other,
        "other-session",
        "SessionStart",
        json!({"permission_mode": "default"}),
    );
    let other_metadata = app.state.surfaces[&other_surface].agent.clone();
    let mut sequenced = hook(
        app,
        terminal,
        "claude-current",
        "Notification",
        json!({"notification_type": "auth_success", "permission_mode": "auto"}),
    );
    sequenced["sequence"] = 100.into();
    assert!(send(app, sequenced.clone()).error.is_none());
    let before = snapshot(app, surface, terminal);
    sequenced["claude_hook"]["permission_mode"] = "bypassPermissions".into();
    let duplicate = send(app, sequenced.clone());
    assert!(duplicate.error.is_none());
    assert_eq!(snapshot(app, surface, terminal), before);
    sequenced["sequence"] = 99.into();
    assert!(send(app, sequenced).error.is_some());
    assert_eq!(snapshot(app, surface, terminal), before);
    assert_eq!(app.state.surfaces[&other_surface].agent, other_metadata);
    let mut start = hook(
        app,
        terminal,
        "claude-current",
        "SessionStart",
        json!({"permission_mode": "auto"}),
    );
    start["sequence"] = 101.into();
    assert!(send(app, start.clone()).error.is_none());
    let before = snapshot(app, surface, terminal);
    start["claude_hook"]["permission_mode"] = "default".into();
    assert!(send(app, start.clone()).error.is_none());
    assert_eq!(snapshot(app, surface, terminal), before);
    start["sequence"] = 98.into();
    assert!(send(app, start).error.is_some());
    assert_eq!(snapshot(app, surface, terminal), before);
    for kind in ["Notification", "SessionStart", "SessionEnd"] {
        let child = hook(
            app,
            terminal,
            "claude-current",
            kind,
            json!({"agent_id": "child", "permission_mode": "bypassPermissions", "notification_type": "auth_success"}),
        );
        let before = snapshot(app, surface, terminal);
        let response = send(app, child);
        if kind == "Notification" {
            assert!(response.error.is_none());
        } else {
            assert!(response.error.is_some());
            assert_eq!(snapshot(app, surface, terminal), before);
        }
        assert_eq!(
            app.claude_hook_modes[&terminal].2,
            AgentRunMode::Unattended,
            "{kind}"
        );
        assert_eq!(
            app.state.surfaces[&surface]
                .agent
                .as_ref()
                .unwrap()
                .run_mode,
            AgentRunMode::Unattended,
            "{kind}"
        );
    }
    accept(
        app,
        other,
        "other-session",
        "Notification",
        json!({"notification_type": "auth_success", "permission_mode": "bypassPermissions"}),
    );
    assert_eq!(app.claude_hook_modes[&terminal].2, AgentRunMode::Unattended);
    assert_eq!(app.claude_hook_modes[&other].2, AgentRunMode::FullAccess);
}

fn check_lifecycle(app: &mut WmctermApplication, terminal: TerminalId, surface: SurfaceId) {
    accept(app, terminal, "claude-current", "SessionEnd", json!({}));
    assert!(!app.claude_hook_modes.contains_key(&terminal));
    accept(
        app,
        terminal,
        "next-session",
        "SessionStart",
        json!({"permission_mode": "default"}),
    );
    assert_eq!(
        app.agent_sessions[&surface].session_id.as_deref(),
        Some("next-session")
    );
    assert_eq!(app.agent_sessions[&surface].state, AgentState::Idle);
    assert_eq!(
        app.state.surfaces[&surface]
            .agent
            .as_ref()
            .unwrap()
            .run_mode,
        AgentRunMode::Interactive
    );
    accept(
        app,
        terminal,
        "next-session",
        "Notification",
        json!({"notification_type": "auth_success", "permission_mode": "auto"}),
    );
    accept(
        app,
        terminal,
        "next-session",
        "Notification",
        json!({"notification_type": "auth_success"}),
    );
    assert_eq!(app.claude_hook_modes[&terminal].2, AgentRunMode::Unattended);
    accept(app, terminal, "legacy-session", "SessionStart", json!({}));
    assert!(!app.claude_hook_modes.contains_key(&terminal));
    assert_eq!(
        app.state.surfaces[&surface]
            .agent
            .as_ref()
            .unwrap()
            .run_mode,
        AgentRunMode::Interactive
    );
    assert_eq!(
        app.agent_sessions[&surface].session_id.as_deref(),
        Some("legacy-session")
    );
    assert_eq!(
        app.state.surfaces[&surface]
            .agent
            .as_ref()
            .unwrap()
            .session_id
            .as_deref(),
        Some("legacy-session")
    );
    let stale = hook(
        app,
        terminal,
        "next-session",
        "Notification",
        json!({"permission_mode": "auto"}),
    );
    let before = snapshot(app, surface, terminal);
    assert!(send(app, stale).error.is_some());
    assert_eq!(snapshot(app, surface, terminal), before);

    accept(
        app,
        terminal,
        "legacy-session",
        "Notification",
        json!({"notification_type": "auth_success", "permission_mode": "auto"}),
    );
    let stale = hook(
        app,
        terminal,
        "legacy-session",
        "Notification",
        json!({"permission_mode": "bypassPermissions"}),
    );
    let previous_lifecycle = app.terminals[&terminal].lifecycle_id.clone();
    app.spawn_terminal(terminal).unwrap();
    assert_ne!(app.terminals[&terminal].lifecycle_id, previous_lifecycle);
    assert!(!app.claude_hook_modes.contains_key(&terminal));
    accept(
        app,
        terminal,
        "legacy-session",
        "SessionStart",
        json!({"permission_mode": "default"}),
    );
    let before = snapshot(app, surface, terminal);
    assert!(send(app, stale).error.is_some());
    assert_eq!(snapshot(app, surface, terminal), before);
    assert_eq!(
        app.agent_sessions[&surface].session_id.as_deref(),
        Some("legacy-session")
    );
    assert_eq!(
        app.state.surfaces[&surface]
            .agent
            .as_ref()
            .unwrap()
            .run_mode,
        AgentRunMode::Interactive
    );
}
