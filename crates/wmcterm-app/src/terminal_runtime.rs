use super::*;

impl Drop for TerminalRuntime {
    fn drop(&mut self) {
        self.bridge_stopped.store(true, Ordering::Release);
    }
}

impl WmctermApplication {
    pub(super) fn remove_terminal_runtime(&mut self, terminal: TerminalId) {
        if let Some(runtime) = self.terminals.get(&terminal) {
            let lifecycle = runtime.lifecycle_id.clone();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|time| time.as_millis() as u64)
                .unwrap_or_default();
            for request in self.pending_agent_request_list() {
                if request.lifecycle_id == lifecycle {
                    let _ = self.pending_agent_requests.cancel(
                        &request.request_id,
                        "terminal closed",
                        now,
                    );
                    self.conclude_agent_request_state(&request);
                }
            }
        }
        self.terminals.remove(&terminal);
        self.pending
            .lock()
            .expect("PTY event queue poisoned")
            .retain(|event| event.terminal != terminal);
        self.pending_ready.notify_all();
        self.managed_codex.remove(&terminal);
        self.commands.remove(&terminal);
        self.terminal_notices.remove(&terminal);
        self.terminal_processes.remove(&terminal);
        self.last_bell_notifications.remove(&terminal);
        self.foreground_agents.remove(&terminal);
        self.automatic_agent_status.remove(&terminal);
        self.claude_hook_modes.remove(&terminal);
        self.foreground_agent_input.remove(&terminal);
        self.session_start_guards.remove(&terminal);
        self.codex_tool_hooks.remove(&terminal);
        self.last_hook_delivery.remove(&terminal);
        self.restored_history.remove(&terminal);
        self.terminal_key_presses
            .retain(|_, (id, _)| *id != terminal);
        if self.input_focus_terminal == Some(terminal) {
            self.input_focus_terminal = None;
        }
        if self
            .terminal_mouse_capture
            .is_some_and(|(_, id, _)| id == terminal)
        {
            self.terminal_mouse_capture = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_terminal_cancels_only_its_requests_and_clears_lifecycle_caches() {
        let state = AppState::new("cleanup", env::temp_dir());
        let terminal = match state.surfaces.values().next().unwrap().kind {
            SurfaceKind::Terminal(id) => id,
            _ => unreachable!(),
        };
        let surface = *state.surfaces.keys().next().unwrap();
        let mut app = WmctermApplication::new_configured(
            AppEventProxy::Headless,
            Config::default(),
            Some(state),
            HashMap::from([(terminal, "sleep 30".into())]),
            HashMap::new(),
            None,
            HashMap::new(),
            HashMap::new(),
            false,
        )
        .unwrap();
        let lifecycle = app.terminals[&terminal].lifecycle_id.clone();
        let request = |id: &str, lifecycle: &str| AgentInteractionRequest {
            request_id: id.into(),
            surface_id: surface.to_string(),
            lifecycle_id: lifecycle.into(),
            agent_pid: None,
            pid_start_time: None,
            kind: IpcAgentRequestKind::new("input"),
            message: "Question".into(),
            choices: Vec::new(),
            deadline_unix_ms: None,
        };
        app.pending_agent_requests
            .register(request("own", &lifecycle))
            .unwrap();
        app.pending_agent_requests
            .register(request("other", "another-lifecycle"))
            .unwrap();
        app.session_start_guards
            .insert(terminal, Default::default());
        app.last_hook_delivery
            .insert(terminal, json!({"old": true}));
        app.foreground_agent_input
            .insert(terminal, "old input".into());
        assert!(app.remove_surface_by_id(surface));
        assert!(app.pending_agent_requests.get("own").unwrap().is_none());
        assert!(app.pending_agent_requests.get("other").unwrap().is_some());
        assert!(matches!(
            app.pending_agent_requests
                .completed_reply("own")
                .unwrap()
                .unwrap()
                .status,
            wmcterm_ipc::AgentReplyStatus::Cancelled { .. }
        ));
        assert!(!app.session_start_guards.contains_key(&terminal));
        assert!(!app.last_hook_delivery.contains_key(&terminal));
        assert!(!app.foreground_agent_input.contains_key(&terminal));
    }
}
