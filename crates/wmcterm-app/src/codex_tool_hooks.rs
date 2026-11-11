use super::*;
use wmcterm_agent::CodexToolHookEvent;

#[derive(Clone, Default)]
pub(super) struct ToolHookTracker {
    session: String,
    lifecycle: String,
    barrier: u64,
    closed: bool,
    waits: Vec<(u64, CodexToolHookEvent)>,
    active: Vec<(u64, CodexToolHookEvent)>,
    candidates: HashMap<u64, HashSet<String>>,
    completed: VecDeque<(u64, CodexToolHookEvent)>,
    model_boundaries: HashMap<String, u64>,
}

impl ToolHookTracker {
    pub(super) fn reset(&mut self, session: &str, lifecycle: &str, timestamp: u64) {
        *self = Self {
            session: session.into(),
            lifecycle: lifecycle.into(),
            barrier: timestamp,
            ..Self::default()
        };
    }

    pub(super) fn observe(
        &mut self,
        hook: &CodexToolHookEvent,
        lifecycle: &str,
        timestamp: u64,
    ) -> Result<Option<AgentEvent>, String> {
        hook.validate()?;
        if hook.agent_id.is_some() {
            return Ok(None);
        }
        if self.session != hook.session_id || self.lifecycle != lifecycle {
            self.reset(&hook.session_id, lifecycle, 0);
        }
        if self.closed {
            return Ok(None);
        }
        if timestamp <= self.barrier
            || self
                .model_boundaries
                .get(&hook.turn_id)
                .is_some_and(|at| timestamp <= *at)
        {
            return Err("stale Codex tool hook".into());
        }
        if let Some(id) = &hook.tool_use_id
            && self
                .completed
                .iter()
                .any(|(_, completed)| completed.tool_use_id.as_ref() == Some(id))
        {
            return Ok(None);
        }
        match hook.hook_event_name.as_str() {
            "PreModelRequest" => {
                self.waits
                    .retain(|(at, wait)| wait.turn_id != hook.turn_id || *at >= timestamp);
                self.active
                    .retain(|(at, active)| active.turn_id != hook.turn_id || *at >= timestamp);
                self.candidates
                    .retain(|at, _| self.waits.iter().any(|(wait, _)| wait == at));
                if self.model_boundaries.len() >= 128
                    && !self.model_boundaries.contains_key(&hook.turn_id)
                {
                    return Err("Codex native turn boundary limit reached".into());
                }
                self.model_boundaries
                    .insert(hook.turn_id.clone(), timestamp);
            }
            "PermissionRequest" => {
                // Completion can arrive before the permission hook's IPC delivery.
                if !self
                    .active
                    .iter()
                    .any(|(at, active)| *at < timestamp && same_call(active, hook))
                    && self.completed.iter().any(|(at, completed)| {
                        *at >= timestamp
                            && completed.turn_id == hook.turn_id
                            && completed.tool_name == hook.tool_name
                            && completed.tool_input == hook.tool_input
                    })
                {
                    return Ok(None);
                }
                if self.waits.iter().any(|(at, _)| *at == timestamp) {
                    return Ok(None);
                }
                if self.waits.len() >= 64 {
                    return Err("Codex native permission wait limit reached".into());
                }
                let candidates = self
                    .active
                    .iter()
                    .filter(|(at, active)| *at < timestamp && same_call(active, hook))
                    .filter_map(|(_, active)| active.tool_use_id.clone())
                    .collect::<HashSet<_>>();
                if !candidates.is_empty() {
                    self.candidates.insert(timestamp, candidates);
                }
                self.waits.push((timestamp, hook.clone()));
            }
            "PostToolUse" => {
                self.active.retain(|(_, active)| {
                    active.turn_id != hook.turn_id || active.tool_use_id != hook.tool_use_id
                });
                for (at, wait) in &self.waits {
                    if *at < timestamp
                        && same_call(wait, hook)
                        && let Some(candidates) = self.candidates.get_mut(at)
                        && let Some(id) = &hook.tool_use_id
                    {
                        candidates.remove(id);
                    }
                }
                self.waits.retain(|(at, wait)| {
                    !(*at < timestamp
                        && wait.turn_id == hook.turn_id
                        && wait.tool_name == hook.tool_name
                        && wait.tool_input == hook.tool_input
                        && self.candidates.get(at).is_none_or(HashSet::is_empty))
                });
                self.candidates
                    .retain(|at, _| self.waits.iter().any(|(wait, _)| wait == at));
                self.completed.push_back((timestamp, hook.clone()));
                if self.completed.len() > 128 {
                    self.completed.pop_front();
                }
            }
            "PreToolUse" => {
                if self.active.len() >= 128 {
                    return Err("Codex native active tool limit reached".into());
                }
                if !self.active.iter().any(|(_, active)| {
                    active.turn_id == hook.turn_id && active.tool_use_id == hook.tool_use_id
                }) {
                    self.active.push((timestamp, hook.clone()));
                }
            }
            _ => {}
        }
        Ok(Some(
            if self.waits.is_empty() || hook.hook_event_name == "PermissionRequest" {
                hook.to_agent_event()
            } else {
                AgentEvent::StateObserved {
                    state: wmcterm_core::AgentState::WaitingForPermission,
                }
            },
        ))
    }

    pub(super) fn close(&mut self) {
        self.waits.clear();
        self.active.clear();
        self.candidates.clear();
        self.completed.clear();
        self.closed = true;
    }
}

fn same_call(left: &CodexToolHookEvent, right: &CodexToolHookEvent) -> bool {
    left.turn_id == right.turn_id
        && left.tool_name == right.tool_name
        && left.tool_input == right.tool_input
}

impl WmctermApplication {
    pub(super) fn project_codex_tool_hook(
        &self,
        terminal: TerminalId,
        params: &Value,
        event: &AgentEvent,
    ) -> Result<Option<(ToolHookTracker, AgentEvent)>, String> {
        let hook =
            wmcterm_agent::normalize_codex_tool_hook_event(None, &params["codex_tool_hook"])?;
        if hook.to_agent_event() != *event {
            return Err("Codex tool hook does not match its projected Agent event".into());
        }
        self.validate_codex_session(terminal, &hook.session_id)?;
        let (_, lifecycle) = self.validate_agent_event_route(terminal, params)?;
        let timestamp = params["hook_started_at_unix_ns"]
            .as_u64()
            .filter(|at| *at > 0)
            .ok_or("missing Codex tool hook timestamp")?;
        if self.managed_codex.contains_key(&terminal) {
            return Ok(None);
        }
        let mut tracker = self
            .codex_tool_hooks
            .get(&terminal)
            .cloned()
            .unwrap_or_default();
        if hook.hook_event_name == "PermissionRequest"
            && self
                .state
                .surfaces
                .get(
                    &self
                        .surface_for_terminal(terminal)
                        .ok_or("surface not found")?,
                )
                .and_then(|surface| surface.agent.as_ref())
                .is_some_and(|agent| agent.run_mode != AgentRunMode::Interactive)
        {
            return Ok(Some((
                tracker,
                AgentEvent::ToolUse {
                    tool: hook.tool_name,
                },
            )));
        }
        Ok(tracker
            .observe(&hook, &lifecycle, timestamp)?
            .map(|event| (tracker, event)))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn hook(kind: &str, command: &str) -> CodexToolHookEvent {
        wmcterm_agent::normalize_codex_tool_hook_event(
            Some(kind),
            &json!({
                "session_id": "session", "turn_id": "turn", "tool_name": "exec_command",
                "tool_input": {"cmd": command}, "tool_use_id": command
            }),
        )
        .unwrap()
    }

    #[test]
    fn completed_tool_clears_only_its_native_permission_wait() {
        let mut tracker = ToolHookTracker::default();
        tracker
            .observe(&hook("PermissionRequest", "a"), "life", 10)
            .unwrap();
        tracker
            .observe(&hook("PermissionRequest", "b"), "life", 20)
            .unwrap();
        let result = tracker
            .observe(&hook("PostToolUse", "a"), "life", 30)
            .unwrap();
        assert!(matches!(
            result,
            Some(AgentEvent::StateObserved {
                state: wmcterm_core::AgentState::WaitingForPermission
            })
        ));
        let result = tracker
            .observe(&hook("PostToolUse", "b"), "life", 40)
            .unwrap();
        assert!(matches!(result, Some(AgentEvent::ToolUse { .. })));
        assert!(tracker.waits.is_empty());
        assert!(
            tracker
                .observe(&hook("PostToolUse", "b"), "life", 41)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn bash_approval_description_is_not_execution_identity() {
        let payload = |kind: &str, input: Value| {
            json!({
                "hook_event_name": kind, "session_id": "session", "turn_id": "turn",
                "tool_name": "Bash", "tool_use_id": "call", "tool_input": input,
                "tool_response": "not retained"
            })
        };
        let permission = wmcterm_agent::normalize_codex_tool_hook_event(
            None,
            &payload(
                "PermissionRequest",
                json!({"command": "printf fixture", "description": "approval prose"}),
            ),
        )
        .unwrap();
        let post = wmcterm_agent::normalize_codex_tool_hook_event(
            None,
            &payload("PostToolUse", json!({"command": "printf fixture"})),
        )
        .unwrap();
        assert!(same_call(&permission, &post));
        assert!(
            serde_json::to_value(&post)
                .unwrap()
                .get("tool_response")
                .is_none()
        );
        let mut tracker = ToolHookTracker::default();
        tracker.observe(&permission, "life", 10).unwrap();
        assert!(matches!(
            tracker.observe(&post, "life", 20).unwrap(),
            Some(AgentEvent::ToolUse { .. })
        ));
    }

    #[test]
    fn next_model_request_clears_denied_tools_only_in_its_own_turn() {
        let mut tracker = ToolHookTracker::default();
        tracker
            .observe(&hook("PermissionRequest", "denied"), "life", 10)
            .unwrap();
        let mut other = hook("PermissionRequest", "other");
        other.turn_id = "other-turn".into();
        tracker.observe(&other, "life", 20).unwrap();
        let mut model = wmcterm_agent::normalize_codex_tool_hook_event(
            Some("PreModelRequest"),
            &json!({"session_id": "session", "turn_id": "turn", "messages": ["not forwarded"]}),
        )
        .unwrap();
        assert!(
            serde_json::to_value(&model)
                .unwrap()
                .get("messages")
                .is_none()
        );
        tracker.observe(&model, "life", 30).unwrap();
        assert_eq!(tracker.waits.len(), 1);
        assert!(
            tracker
                .observe(&hook("PermissionRequest", "denied"), "life", 15)
                .is_err()
        );
        model.turn_id = "other-turn".into();
        assert!(matches!(
            tracker.observe(&model, "life", 40).unwrap(),
            Some(AgentEvent::ToolUse { .. })
        ));
        assert!(tracker.waits.is_empty());
    }

    #[test]
    fn identical_parallel_tools_retain_wait_until_all_candidates_finish() {
        let mut tracker = ToolHookTracker::default();
        let first = hook("PreToolUse", "same");
        let mut second = first.clone();
        second.tool_use_id = Some("second".into());
        tracker.observe(&first, "life", 1).unwrap();
        tracker.observe(&second, "life", 2).unwrap();
        tracker
            .observe(&hook("PermissionRequest", "same"), "life", 3)
            .unwrap();
        let mut post = first;
        post.hook_event_name = "PostToolUse".into();
        tracker.observe(&post, "life", 4).unwrap();
        assert_eq!(tracker.waits.len(), 1);
        second.hook_event_name = "PostToolUse".into();
        tracker.observe(&second, "life", 5).unwrap();
        assert!(tracker.waits.is_empty());
    }

    #[test]
    fn delayed_hooks_cannot_reactivate_finished_turns_or_replace_sessions() {
        let mut tracker = ToolHookTracker::default();
        tracker
            .observe(&hook("PostToolUse", "a"), "life", 20)
            .unwrap();
        assert!(
            tracker
                .observe(&hook("PermissionRequest", "a"), "life", 10)
                .unwrap()
                .is_none()
        );
        tracker.reset("session", "life", 30);
        assert!(
            tracker
                .observe(&hook("PostToolUse", "a"), "life", 20)
                .is_err()
        );
        let mut child = hook("PostToolUse", "a");
        child.agent_id = Some("child".into());
        assert!(tracker.observe(&child, "life", 40).unwrap().is_none());
        tracker.close();
        assert!(
            tracker
                .observe(&hook("PostToolUse", "b"), "life", 50)
                .unwrap()
                .is_none()
        );
    }

    pub(crate) fn check_rpc(
        app: &mut WmctermApplication,
        terminal: TerminalId,
        surface: SurfaceId,
    ) {
        use wmcterm_core::AgentState;
        app.config.agent.notifications = false;
        app.managed_codex.remove(&terminal);
        let lifecycle = app.terminals[&terminal].lifecycle_id.clone();
        let timestamp = (unix_time_ms() + 1) * 1_000_000;
        let call = |app: &mut WmctermApplication, mut params: Value, at: u64| {
            params["surface"] = surface.to_string().into();
            params["surface_id"] = surface.to_string().into();
            params["lifecycle_id"] = lifecycle.clone().into();
            params["hook_started_at_unix_ns"] = at.into();
            let response = app.handle_rpc(Request {
                id: "native-hook".into(),
                method: "agent.update".into(),
                params,
            });
            if let Some(error) = &response.error {
                eprintln!("native hook at {at}: {error:?}");
            }
            response
        };
        let start = json!({"event": "session_start", "agent": "codex", "session_id": "session"});
        assert!(call(app, start, timestamp).error.is_none());
        let native = |kind: &str, command: &str| {
            let hook = hook(kind, command);
            let mut params = serde_json::to_value(hook.to_agent_event()).unwrap();
            params["codex_tool_hook"] = serde_json::to_value(hook).unwrap();
            params
        };
        let state =
            |app: &WmctermApplication| app.state.surfaces[&surface].agent.as_ref().unwrap().state;
        assert!(
            call(app, native("PermissionRequest", "a"), timestamp + 1)
                .error
                .is_none()
        );
        assert_eq!(state(app), AgentState::WaitingForPermission);
        assert!(app.pending_agent_request_list().is_empty());
        assert!(
            call(app, native("PreToolUse", "b"), timestamp + 2)
                .error
                .is_none()
        );
        assert_eq!(state(app), AgentState::WaitingForPermission);
        assert!(
            call(app, native("PostToolUse", "b"), timestamp + 3)
                .error
                .is_none()
        );
        assert_eq!(state(app), AgentState::WaitingForPermission);
        assert!(
            call(app, native("PostToolUse", "a"), timestamp + 4)
                .error
                .is_none()
        );
        assert_eq!(state(app), AgentState::Running);
        assert_eq!(app.agent_sessions[&surface].state, AgentState::Running);
        assert_ne!(
            app.state.workspaces[0].status,
            wmcterm_core::WorkspaceStatus::NeedsInput
        );

        for field in ["session_id", "turn_id", "tool_use_id"] {
            let mut invalid = native("PostToolUse", "a");
            invalid["codex_tool_hook"][field] = "".into();
            assert!(call(app, invalid, timestamp + 5).error.is_some());
        }
        let mut old_session = native("PostToolUse", "a");
        old_session["codex_tool_hook"]["session_id"] = "other-session".into();
        assert!(call(app, old_session, timestamp + 5).error.is_some());
        let mut child = native("PermissionRequest", "child");
        child["codex_tool_hook"]["agent_id"] = "child".into();
        assert!(call(app, child, timestamp + 6).error.is_none());
        assert_eq!(state(app), AgentState::Running);

        assert!(
            call(
                app,
                json!({"event": "stop", "success": true, "message": null}),
                timestamp + 7
            )
            .error
            .is_none()
        );
        assert_eq!(state(app), AgentState::Completed);
        assert!(
            call(app, native("PostToolUse", "late"), timestamp + 8)
                .error
                .is_none()
        );
        assert_eq!(state(app), AgentState::Completed);
        assert!(call(app, json!({"event": "tool_use", "tool": "agent activity", "codex_hook_event": "UserPromptSubmit"}), timestamp + 9).error.is_none());
        assert_eq!(state(app), AgentState::Running);
        assert!(
            call(app, native("PermissionRequest", "a"), timestamp + 10)
                .error
                .is_none()
        );
        assert_eq!(state(app), AgentState::WaitingForPermission);
        assert!(
            call(app, native("PostToolUse", "a"), timestamp + 4)
                .error
                .is_some()
        );
        assert_eq!(state(app), AgentState::WaitingForPermission);
        assert!(
            call(app, native("PostToolUse", "a"), timestamp + 11)
                .error
                .is_none()
        );
        assert_eq!(state(app), AgentState::Running);
        let registered = app.register_agent_approval(&json!({
            "source": "codex_app_server", "workspace_id": app.state.workspaces[0].id,
            "surface_id": surface,
            "request": {"jsonrpc_id": "native-interleave", "method": "item/commandExecution/requestApproval",
                "kind": "command_execution", "thread_id": "session", "turn_id": "turn", "item_id": "real",
                "params": {"command": "pwd"}}, "decisions": ["deny", "once"]
        })).unwrap();
        assert!(
            call(app, native("PostToolUse", "unrelated"), timestamp + 12)
                .error
                .is_none()
        );
        assert_eq!(state(app), AgentState::WaitingForPermission);
        assert_eq!(app.pending_agent_request_list().len(), 1);
        app.resolve_agent_request_from_ui(
            registered["approval_id"].as_str().unwrap().into(),
            AgentApprovalAction::Deny,
        );
        assert!(app.pending_agent_request_list().is_empty());
    }
}
