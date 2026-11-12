use super::*;

impl WmctermApplication {
    pub(super) fn apply_agent_event(
        &mut self,
        terminal: TerminalId,
        event: AgentEvent,
    ) -> wmcterm_core::AgentState {
        if !matches!(event, AgentEvent::Notification { .. }) {
            self.automatic_agent_status.remove(&terminal);
        }
        let metadata = self
            .surface_for_terminal(terminal)
            .and_then(|surface| self.state.surfaces.get(&surface))
            .and_then(|surface| surface.agent.as_ref());
        // Only Codex's automatic reviewer suppresses permission-wait telemetry.
        let transition_mode = metadata
            .filter(|agent| agent.name.eq_ignore_ascii_case("codex"))
            .map(|agent| agent.run_mode)
            .unwrap_or_default();
        let transition = agent_transition_for_run_mode(&event, transition_mode);
        let suppress_notification = non_actionable_agent_exit(&event);
        let announced = match &event {
            AgentEvent::SessionStart { agent, session_id } => {
                Some((agent.clone(), session_id.clone()))
            }
            _ => None,
        };
        let mut observed_state = wmcterm_core::AgentState::Idle;
        if let Some(surface) = self
            .state
            .surfaces
            .values_mut()
            .find(|surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal))
        {
            let run_mode = surface
                .agent
                .as_ref()
                .map(|agent| agent.run_mode)
                .unwrap_or_default();
            let (name, session_id) = announced.unwrap_or_else(|| {
                surface
                    .agent
                    .as_ref()
                    .map(|agent| (agent.name.clone(), agent.session_id.clone()))
                    .unwrap_or_else(|| ("agent".into(), None))
            });
            observed_state = transition.state.unwrap_or_else(|| {
                surface
                    .agent
                    .as_ref()
                    .map(|agent| agent.state)
                    .unwrap_or_default()
            });
            surface.agent = Some(wmcterm_core::AgentMetadata {
                name,
                state: observed_state,
                session_id,
                run_mode,
            });
            if transition.state.is_some() {
                surface.attention = transition.attention;
            }
        }
        if let Some(message) = transition.notification
            && !suppress_notification
        {
            self.notify_terminal(terminal, &message);
        }
        if let Some(index) = self.workspace_for_terminal(terminal) {
            self.sync_workspace_agent_status(index);
        }
        if let Some(surface) = self.surface_for_terminal(terminal) {
            self.sync_surface_agent_request_state(surface);
        }
        observed_state
    }

    pub(super) fn record_agent_event(
        &mut self,
        terminal: TerminalId,
        params: &Value,
        event: &AgentEvent,
    ) -> Result<bool, String> {
        let (surface, effective_lifecycle) = self.validate_agent_event_route(terminal, params)?;
        if let Some(session_id) = params.get("codex_session_id").and_then(Value::as_str)
            && self
                .managed_codex
                .get(&terminal)
                .is_some_and(|launch| launch.verified && launch.saved.session_id != session_id)
        {
            return Err("Codex hook session does not match the managed thread".into());
        }
        let mut start_guard = None;
        if let AgentEvent::SessionStart {
            agent,
            session_id: Some(id),
        } = event
        {
            // Native reviewer SessionStart hooks belong to an internal thread,
            // not the managed TUI's registered session. /resume is routed by ready.
            if agent.eq_ignore_ascii_case("codex")
                && self
                    .managed_codex
                    .get(&terminal)
                    .is_some_and(|launch| launch.verified && launch.saved.session_id != *id)
            {
                return Err("Codex hook session does not match the managed thread".into());
            }
            let mut guard = self
                .session_start_guards
                .get(&terminal)
                .cloned()
                .unwrap_or_default();
            let emitted = params
                .get("hook_started_at_unix_ns")
                .map(|value| {
                    value
                        .as_u64()
                        .ok_or_else(|| "invalid hook timestamp".to_owned())
                })
                .transpose()?;
            if !guard.observe(&format!("{agent}:{id}"), emitted)? {
                return Ok(false);
            }
            start_guard = Some(guard);
        }
        let sequence = params.get("sequence").and_then(Value::as_u64);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or_default();
        let terminal_cwd = self.cwd_for_terminal(terminal);
        let launch_command = self.commands.get(&terminal).cloned();
        let session = self
            .agent_sessions
            .entry(surface)
            .or_insert_with(|| AgentSession::new(surface));
        let name = match event {
            AgentEvent::SessionStart { agent, .. } => agent.clone(),
            _ if !session.agent_kind.as_str().is_empty() => session.agent_kind.as_str().to_owned(),
            _ => "agent".into(),
        };
        let must_start = session.lifecycle.phase != AgentLifecyclePhase::Running
            || session.lifecycle.external_id.as_deref() != Some(effective_lifecycle.as_str())
            || matches!(event, AgentEvent::SessionStart { session_id: Some(id), .. }
                if session.session_id.as_ref() != Some(id));
        if must_start {
            let generation = session
                .next_lifecycle_generation()
                .map_err(|error| error.to_string())?;
            let (pid, process_started_at_unix_ms) = match (
                params.get("agent_pid").and_then(Value::as_u64),
                params.get("pid_start_time").and_then(Value::as_u64),
            ) {
                (Some(pid), Some(start)) => (u32::try_from(pid).ok(), Some(start)),
                _ => (None, None),
            };
            let session_id = match event {
                AgentEvent::SessionStart { session_id, .. } => session_id.clone(),
                _ => None,
            };
            let start = AgentSessionEvent {
                event_id: format!("{effective_lifecycle}:start:{generation}"),
                lifecycle_generation: generation,
                lifecycle_id: Some(effective_lifecycle.clone()),
                sequence: None,
                occurred_at_unix_ms: now,
                kind: AgentSessionEventKind::Lifecycle(AgentLifecycleEvent::Started {
                    launch: AgentLaunchInfo {
                        agent_kind: AgentKindId::new(name),
                        session_id,
                        pid,
                        process_started_at_unix_ms,
                        cwd: terminal_cwd,
                        transcript: None,
                        launch_command,
                    },
                }),
            };
            session
                .apply_event(&start)
                .map_err(|error| error.to_string())?;
        }
        if matches!(event, AgentEvent::SessionStart { .. }) {
            if let (
                Some(sequence),
                AgentEvent::SessionStart {
                    session_id: Some(id),
                    ..
                },
            ) = (sequence, event)
            {
                let outcome = session
                    .apply_event(&AgentSessionEvent {
                        event_id: format!(
                            "{effective_lifecycle}:start:{}:{sequence}",
                            session.lifecycle.generation
                        ),
                        lifecycle_generation: session.lifecycle.generation,
                        lifecycle_id: Some(effective_lifecycle.clone()),
                        sequence: Some(sequence),
                        occurred_at_unix_ms: now,
                        kind: AgentSessionEventKind::Telemetry(AgentTelemetry::SessionObserved {
                            session_id: id.clone(),
                        }),
                    })
                    .map_err(|error| error.to_string())?;
                if !matches!(
                    outcome,
                    wmcterm_core::AgentEventApplyOutcome::Applied { .. }
                ) {
                    return Ok(false);
                }
            }
            if let AgentEvent::SessionStart {
                agent,
                session_id: Some(session_id),
            } = event
                && agent.eq_ignore_ascii_case("codex")
            {
                clear_codex_subagent_sessions_except(
                    &mut self.codex_subagents,
                    surface,
                    session_id,
                );
            }
            if let Some(guard) = start_guard {
                self.session_start_guards.insert(terminal, guard);
            }
            if must_start {
                for request in self
                    .pending_agent_request_list()
                    .into_iter()
                    .filter(|request| request.surface_id == surface.to_string())
                {
                    let _ = self.pending_agent_requests.cancel(
                        &request.request_id,
                        "Agent session replaced",
                        now,
                    );
                    self.remember_agent_approval_result(
                        request.request_id.clone(),
                        AgentApprovalAction::Deny,
                    );
                    self.conclude_agent_request_state(&request);
                }
            }
            return Ok(true);
        }
        let generation = session.lifecycle.generation;
        let suffix = sequence
            .map(|value| value.to_string())
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let run_mode = self
            .state
            .surfaces
            .get(&surface)
            .and_then(|surface| surface.agent.as_ref())
            .filter(|agent| agent.name.eq_ignore_ascii_case("codex"))
            .map(|agent| agent.run_mode)
            .unwrap_or_default();
        let kind = agent_session_event_kind(event, &suffix, run_mode)
            .ok_or_else(|| "session start must be handled before event mapping".to_owned())?;
        let event = AgentSessionEvent {
            event_id: format!("{effective_lifecycle}:{suffix}"),
            lifecycle_generation: generation,
            lifecycle_id: Some(effective_lifecycle.clone()),
            sequence,
            occurred_at_unix_ms: now,
            kind,
        };
        if let AgentSessionEventKind::ActionRequired(request) = &event.kind {
            let kind = match request.kind {
                AgentRequestKind::Permission => IpcAgentRequestKind::PERMISSION,
                AgentRequestKind::Input => IpcAgentRequestKind::INPUT,
                AgentRequestKind::Confirmation => IpcAgentRequestKind::CONFIRMATION,
            };
            self.pending_agent_requests
                .register(AgentInteractionRequest {
                    request_id: request.request_id.clone(),
                    surface_id: surface.to_string(),
                    lifecycle_id: effective_lifecycle.clone(),
                    agent_pid: params
                        .get("agent_pid")
                        .and_then(Value::as_u64)
                        .and_then(|value| u32::try_from(value).ok()),
                    pid_start_time: params.get("pid_start_time").and_then(Value::as_u64),
                    kind: IpcAgentRequestKind::new(kind),
                    message: request.message.clone(),
                    choices: request
                        .choices
                        .iter()
                        .enumerate()
                        .map(|(index, label)| wmcterm_ipc::AgentChoice {
                            id: index.to_string(),
                            label: label.clone(),
                        })
                        .collect(),
                    deadline_unix_ms: params.get("deadline_unix_ms").and_then(Value::as_u64),
                })
                .map_err(|error| error.to_string())?;
        }
        let actionable = matches!(event.kind, AgentSessionEventKind::ActionRequired(_));
        let applied = session
            .apply_event(&event)
            .map(|outcome| {
                matches!(
                    outcome,
                    wmcterm_core::AgentEventApplyOutcome::Applied { .. }
                )
            })
            .map_err(|error| error.to_string())?;
        if applied && actionable {
            self.sync_surface_agent_request_state(surface);
        }
        Ok(applied)
    }

    pub(super) fn validate_agent_event_route(
        &self,
        terminal: TerminalId,
        params: &Value,
    ) -> Result<(SurfaceId, String), String> {
        let surface = self
            .surface_for_terminal(terminal)
            .ok_or_else(|| "agent target surface no longer exists".to_owned())?;
        let runtime_lifecycle = &self
            .terminals
            .get(&terminal)
            .ok_or_else(|| "agent target terminal is not running".to_owned())?
            .lifecycle_id;
        let effective_lifecycle = effective_agent_lifecycle(
            runtime_lifecycle,
            params.get("lifecycle_id").and_then(Value::as_str),
        )?;
        match (
            params.get("agent_pid").and_then(Value::as_u64),
            params.get("pid_start_time").and_then(Value::as_u64),
        ) {
            (Some(pid), Some(start)) => {
                let pid = u32::try_from(pid).map_err(|_| "invalid agent pid".to_owned())?;
                let expected = AgentProcessIdentity::expected(
                    pid,
                    start,
                    surface.to_string(),
                    effective_lifecycle.clone(),
                );
                validate_agent_process_identity(&expected).map_err(|error| error.to_string())?;
            }
            (None, None) => {}
            _ => return Err("incomplete agent process identity".into()),
        }
        Ok((surface, effective_lifecycle))
    }

    pub(super) fn resolve_agent_session_request(&mut self, request_id: &str) -> Result<(), String> {
        let (_, session) = self
            .agent_sessions
            .iter_mut()
            .find(|(_, session)| {
                session
                    .pending_requests
                    .iter()
                    .any(|request| request.request_id == request_id)
            })
            .ok_or_else(|| "agent session request not found".to_owned())?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or_default();
        let event = AgentSessionEvent {
            event_id: format!("reply:{request_id}:{}", uuid::Uuid::new_v4()),
            lifecycle_generation: session.lifecycle.generation,
            lifecycle_id: session.lifecycle.external_id.clone(),
            sequence: None,
            occurred_at_unix_ms: now,
            kind: AgentSessionEventKind::ActionResolved {
                request_id: request_id.to_owned(),
            },
        };
        session
            .apply_event(&event)
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(super) fn conclude_agent_request_state(&mut self, request: &AgentInteractionRequest) {
        let _ = self.resolve_agent_session_request(&request.request_id);
        if self
            .agent_request_detail
            .as_ref()
            .is_some_and(|(request_id, _)| request_id == &request.request_id)
        {
            self.agent_request_detail = None;
            self.agent_request_detail_scrollbar_drag = None;
        }
        if let Ok(surface) = uuid::Uuid::parse_str(&request.surface_id) {
            if let Some(notification) =
                self.state
                    .notifications
                    .iter_mut()
                    .rev()
                    .find(|notification| {
                        notification.surface == surface
                            && !notification.read
                            && notification.attention == AttentionState::NeedsInput
                            && notification.body == request.message
                    })
            {
                notification.read = true;
            }
            self.sync_surface_agent_request_state(surface);
        }
    }

    pub(super) fn record_claude_hook(
        &mut self,
        terminal: TerminalId,
        hook: &ClaudeHookEvent,
    ) -> Result<(), String> {
        let surface = self.validate_claude_hook(terminal, hook)?;
        apply_claude_subagent_status(&mut self.claude_subagents, surface, hook);
        if hook.agent_id.is_none() {
            let observation =
                match hook.kind {
                    ClaudeHookKind::SessionStart => {
                        hook.cwd.as_deref().map(|cwd| AgentTelemetry::CwdObserved {
                            cwd: PathBuf::from(cwd),
                        })
                    }
                    ClaudeHookKind::CwdChanged => {
                        hook.new_cwd
                            .as_deref()
                            .map(|cwd| AgentTelemetry::CwdObserved {
                                cwd: PathBuf::from(cwd),
                            })
                    }
                    ClaudeHookKind::DirectoryAdded => hook.directory.as_deref().map(|directory| {
                        AgentTelemetry::DirectoryObserved {
                            directory: PathBuf::from(directory),
                        }
                    }),
                    _ => None,
                };
            if let Some(observation) = observation
                && let Some(session) = self.agent_sessions.get_mut(&surface)
            {
                session
                    .apply_event(&AgentSessionEvent {
                        event_id: format!("claude:directory:{}", uuid::Uuid::new_v4()),
                        lifecycle_generation: session.lifecycle.generation,
                        lifecycle_id: session.lifecycle.external_id.clone(),
                        sequence: None,
                        occurred_at_unix_ms: unix_time_ms(),
                        kind: AgentSessionEventKind::Telemetry(observation),
                    })
                    .map_err(|error| error.to_string())?;
            }
            if matches!(
                hook.kind,
                ClaudeHookKind::SessionStart | ClaudeHookKind::CwdChanged
            ) && let Some(index) = self.workspace_for_terminal(terminal)
                && let Some(workspace) = self.state.workspaces.get(index)
                && self
                    .state
                    .panes
                    .get(&workspace.active_pane)
                    .and_then(|pane| pane.active_surface())
                    == Some(surface)
                && let Some(cwd) = self
                    .agent_sessions
                    .get(&surface)
                    .and_then(|session| session.cwd.clone())
            {
                self.state.workspaces[index].cwd = cwd;
                self.refresh_workspace_metadata();
            }
        }
        if hook.kind == ClaudeHookKind::SessionEnd {
            self.claude_hook_modes.remove(&terminal);
            self.close_agent_session_for_terminal(terminal, "Claude session ended".into());
        } else if hook.agent_id.is_none() {
            if hook.kind == ClaudeHookKind::SessionStart {
                self.claude_hook_modes.remove(&terminal);
                if hook.run_mode().is_none() {
                    let mode = self
                        .terminal_processes
                        .get(&terminal)
                        .and_then(|commands| {
                            commands.iter().find(|command| {
                                AgentKind::from_process_command(command) == Some(AgentKind::Claude)
                            })
                        })
                        .map(|command| AgentKind::Claude.run_mode_from_process_command(command))
                        .unwrap_or_default();
                    if let Some(metadata) = self
                        .state
                        .surfaces
                        .get_mut(&surface)
                        .and_then(|surface| surface.agent.as_mut())
                    {
                        metadata.run_mode = mode;
                    }
                }
            }
            if let Some(mode) = hook.run_mode() {
                self.claude_hook_modes.insert(
                    terminal,
                    (
                        self.terminals[&terminal].lifecycle_id.clone(),
                        hook.session_id.clone(),
                        mode,
                    ),
                );
                if let Some(metadata) = self
                    .state
                    .surfaces
                    .get_mut(&surface)
                    .and_then(|surface| surface.agent.as_mut())
                {
                    metadata.run_mode = mode;
                }
            }
        }
        Ok(())
    }

    pub(super) fn validate_claude_hook(
        &self,
        terminal: TerminalId,
        hook: &ClaudeHookEvent,
    ) -> Result<SurfaceId, String> {
        validate_claude_hook_directories(hook)?;
        let (_, _, surface) = terminal_context_ids(&self.state, terminal)
            .ok_or_else(|| "Claude hook terminal context not found".to_owned())?;
        let metadata = self
            .state
            .surfaces
            .get(&surface)
            .and_then(|surface| surface.agent.as_ref())
            .ok_or_else(|| "Claude hook surface has no Agent session".to_owned())?;
        if !matches!(
            metadata.name.to_ascii_lowercase().as_str(),
            "claude" | "claude code"
        ) {
            return Err("Claude hook cannot update a non-Claude surface".into());
        }
        if metadata.session_id.as_deref() != Some(hook.session_id.as_str()) {
            return Err("Claude hook session does not match its surface".into());
        }
        Ok(surface)
    }

    pub(super) fn validate_codex_session(
        &self,
        terminal: TerminalId,
        session_id: &str,
    ) -> Result<SurfaceId, String> {
        let (_, _, surface) = terminal_context_ids(&self.state, terminal)
            .ok_or_else(|| "Codex hook terminal context not found".to_owned())?;
        let metadata = self
            .state
            .surfaces
            .get(&surface)
            .and_then(|surface| surface.agent.as_ref())
            .ok_or_else(|| "Codex hook surface has no Agent session".to_owned())?;
        if !metadata.name.eq_ignore_ascii_case("codex") {
            return Err("Codex hook cannot update a non-Codex surface".into());
        }
        if metadata.session_id.as_deref() != Some(session_id) {
            return Err("Codex hook session does not match its surface".into());
        }
        Ok(surface)
    }

    pub(super) fn record_codex_subagent_hook(
        &mut self,
        terminal: TerminalId,
        hook: &CodexSubagentHookEvent,
    ) -> Result<wmcterm_core::AgentState, String> {
        hook.validate().map_err(|error| error.to_string())?;
        let surface = self.validate_codex_session(terminal, &hook.session_id)?;
        apply_codex_subagent_status(&mut self.codex_subagents, surface, hook);
        let parent = self
            .state
            .surfaces
            .get(&surface)
            .and_then(|surface| surface.agent.as_ref())
            .map(|agent| agent.state)
            .unwrap_or(wmcterm_core::AgentState::Idle);
        let active = codex_subagent_count(&self.codex_subagents, surface, &hook.session_id) > 0;
        if let Some(workspace) = self.workspace_for_surface(surface) {
            self.sync_workspace_agent_status(workspace);
        }
        Ok(effective_codex_agent_state(parent, active))
    }

    pub(super) fn end_codex_subagent_session(
        &mut self,
        terminal: TerminalId,
        session_id: &str,
    ) -> Result<(), String> {
        let surface = self.validate_codex_session(terminal, session_id)?;
        clear_codex_subagent_session(&mut self.codex_subagents, surface, session_id);
        if let Some(workspace) = self.workspace_for_surface(surface) {
            self.sync_workspace_agent_status(workspace);
        }
        Ok(())
    }

    pub(super) fn expire_agent_requests(&mut self) -> bool {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or_default();
        let requests = self.pending_agent_request_list();
        let replies = match self.pending_agent_requests.expire(now) {
            Ok(replies) => replies,
            Err(error) => {
                tracing::warn!(%error, "expiring Agent requests failed");
                return false;
            }
        };
        let changed = !replies.is_empty();
        for reply in replies {
            if let Some(request) = requests
                .iter()
                .find(|request| request.request_id == reply.request_id)
            {
                self.conclude_agent_request_state(request);
            }
        }
        changed
    }

    pub(super) fn close_agent_session_for_terminal(
        &mut self,
        terminal: TerminalId,
        message: String,
    ) {
        let Some(surface) = self.surface_for_terminal(terminal) else {
            return;
        };
        clear_codex_subagent_surface(&mut self.codex_subagents, surface);
        if let Some(workspace) = self.workspace_for_surface(surface) {
            self.sync_workspace_agent_status(workspace);
        }
        let Some(session) = self.agent_sessions.get(&surface) else {
            return;
        };
        let scope = wmcterm_ipc::AgentProcessScope {
            surface_id: surface.to_string(),
            lifecycle_id: session.lifecycle.external_id.clone().unwrap_or_default(),
            // Ending the terminal's Agent lifecycle also cancels requests
            // registered by hooks that did not carry a process identity.
            agent_pid: None,
            pid_start_time: None,
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or_default();
        let requests = self.pending_agent_request_list();
        let replies = self
            .pending_agent_requests
            .process_exited(&scope, now)
            .unwrap_or_default();
        for reply in replies {
            if let Some(request) = requests
                .iter()
                .find(|request| request.request_id == reply.request_id)
            {
                self.conclude_agent_request_state(request);
            }
        }
        let Some(session) = self.agent_sessions.get_mut(&surface) else {
            return;
        };
        if session.lifecycle.phase != AgentLifecyclePhase::Running {
            self.sync_surface_agent_request_state(surface);
            return;
        }
        let event = AgentSessionEvent {
            event_id: format!("terminal-exit:{}", uuid::Uuid::new_v4()),
            lifecycle_generation: session.lifecycle.generation,
            lifecycle_id: session.lifecycle.external_id.clone(),
            sequence: None,
            occurred_at_unix_ms: now,
            kind: AgentSessionEventKind::Lifecycle(AgentLifecycleEvent::Exited {
                success: true,
                message: Some(message),
            }),
        };
        let _ = session.apply_event(&event);
        self.sync_surface_agent_request_state(surface);
    }

    pub(super) fn clear_manual_agent_resume_after_terminal_exit(&mut self, terminal: TerminalId) {
        if !self
            .commands
            .get(&terminal)
            .is_some_and(|command| is_manual_agent_resume_command(command))
        {
            return;
        }
        self.commands.remove(&terminal);
        self.foreground_agents.remove(&terminal);
        self.foreground_agent_input.remove(&terminal);
        self.automatic_agent_status.remove(&terminal);
        let Some(surface) = self.surface_for_terminal(terminal) else {
            return;
        };
        self.agent_sessions.remove(&surface);
        clear_codex_subagent_surface(&mut self.codex_subagents, surface);
        if let Some(surface) = self.state.surfaces.get_mut(&surface) {
            surface.agent = None;
            surface.attention = AttentionState::None;
        }
        if let Some(workspace) = self.workspace_for_surface(surface) {
            self.sync_workspace_agent_status(workspace);
        }
    }

    pub(super) fn update_foreground_agent(
        &mut self,
        terminal: TerminalId,
        commands: &[String],
    ) -> bool {
        self.claude_hook_modes.retain(|id, (lifecycle, _, _)| {
            self.terminals
                .get(id)
                .is_some_and(|runtime| runtime.lifecycle_id == *lifecycle)
        });
        let hook_mode = self
            .claude_hook_modes
            .get(&terminal)
            .and_then(|(_, session, mode)| {
                self.surface_for_terminal(terminal)
                    .and_then(|surface| self.state.surfaces.get(&surface))
                    .and_then(|surface| surface.agent.as_ref())
                    .filter(|metadata| metadata.session_id.as_ref() == Some(session))
                    .map(|_| *mode)
            });
        let detected = commands.iter().find_map(|command| {
            AgentKind::from_process_command(command).map(|agent| {
                (
                    agent,
                    self.managed_codex
                        .get(&terminal)
                        .filter(|_| agent == AgentKind::Codex)
                        .map(|codex| codex.saved.mode)
                        .or_else(|| (agent == AgentKind::Claude).then_some(hook_mode).flatten())
                        .unwrap_or_else(|| agent.run_mode_from_process_command(command)),
                )
            })
        });
        let previous = self.foreground_agents.get(&terminal).copied();
        if previous == detected {
            return false;
        }
        match detected {
            Some((agent, run_mode)) => {
                self.foreground_agents.insert(terminal, (agent, run_mode));
                self.foreground_agent_input.remove(&terminal);
                let should_update = self.automatic_agent_status.contains(&terminal)
                    || self
                        .state
                        .surfaces
                        .values()
                        .find(|surface| {
                            matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal)
                        })
                        .is_none_or(|surface| surface.agent.is_none());
                if !should_update {
                    // Hook/session state owns the task status, but a manually
                    // launched process still owns its current approval mode.
                    if agent_launch_is_managed(
                        self.commands.get(&terminal).map(String::as_str),
                        agent.as_str(),
                    ) {
                        return false;
                    }
                    return self.state.surfaces.values_mut().find(|surface| {
                        matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal)
                    }).and_then(|surface| surface.agent.as_mut())
                        .is_some_and(|metadata| sync_detected_agent_mode(metadata, agent, run_mode));
                }
                self.automatic_agent_status.insert(terminal);
                if let Some(surface) = self.state.surfaces.values_mut().find(
                    |surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal),
                ) {
                    surface.agent = Some(detected_foreground_agent_metadata(agent, run_mode));
                    surface.attention = AttentionState::None;
                }
                if let Some(index) = self.workspace_for_terminal(terminal)
                    && let Some(workspace) = self.state.workspaces.get_mut(index)
                {
                    workspace.status = wmcterm_core::WorkspaceStatus::Idle;
                }
                if let Some(surface) = self.surface_for_terminal(terminal) {
                    self.sync_surface_agent_request_state(surface);
                }
                true
            }
            None => {
                self.foreground_agents.remove(&terminal);
                self.foreground_agent_input.remove(&terminal);
                self.automatic_agent_status.remove(&terminal);
                if let Some(surface) = self.state.surfaces.values_mut().find(
                    |surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal),
                ) {
                    surface.agent = None;
                    surface.attention = AttentionState::None;
                }
                if let Some(index) = self.workspace_for_terminal(terminal)
                    && let Some(workspace) = self.state.workspaces.get_mut(index)
                {
                    workspace.status = wmcterm_core::WorkspaceStatus::Idle;
                }
                if let Some(surface) = self.surface_for_terminal(terminal) {
                    self.agent_sessions.remove(&surface);
                    clear_codex_subagent_surface(&mut self.codex_subagents, surface);
                    self.sync_surface_agent_request_state(surface);
                }
                true
            }
        }
    }

    pub(super) fn mark_foreground_agent_running(&mut self, terminal: TerminalId) {
        let Some((agent, run_mode)) = self.foreground_agents.get(&terminal).copied() else {
            return;
        };
        self.automatic_agent_status.insert(terminal);
        if let Some(surface) = self
            .state
            .surfaces
            .values_mut()
            .find(|surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal))
        {
            let session_id = surface
                .agent
                .as_ref()
                .and_then(|metadata| metadata.session_id.clone());
            surface.agent = Some(wmcterm_core::AgentMetadata {
                name: agent.as_str().to_owned(),
                state: wmcterm_core::AgentState::Running,
                session_id,
                run_mode,
            });
            surface.attention = AttentionState::None;
        }
        if let Some(index) = self.workspace_for_terminal(terminal)
            && let Some(workspace) = self.state.workspaces.get_mut(index)
        {
            workspace.status = wmcterm_core::WorkspaceStatus::Running;
        }
        if let Some(surface) = self.surface_for_terminal(terminal) {
            self.observe_agent_session_state(surface, wmcterm_core::AgentState::Running);
            self.sync_surface_agent_request_state(surface);
        }
    }

    pub(super) fn mark_foreground_agent_idle(&mut self, terminal: TerminalId) {
        let Some((agent, run_mode)) = self.foreground_agents.get(&terminal).copied() else {
            return;
        };
        self.automatic_agent_status.insert(terminal);
        if let Some(surface) = self
            .state
            .surfaces
            .values_mut()
            .find(|surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal))
        {
            let session_id = surface
                .agent
                .as_ref()
                .and_then(|metadata| metadata.session_id.clone());
            surface.agent = Some(wmcterm_core::AgentMetadata {
                name: agent.as_str().to_owned(),
                state: wmcterm_core::AgentState::Idle,
                session_id,
                run_mode,
            });
            surface.attention = AttentionState::None;
        }
        if let Some(index) = self.workspace_for_terminal(terminal)
            && let Some(workspace) = self.state.workspaces.get_mut(index)
        {
            workspace.status = wmcterm_core::WorkspaceStatus::Idle;
        }
        if let Some(surface) = self.surface_for_terminal(terminal) {
            self.observe_agent_session_state(surface, wmcterm_core::AgentState::Idle);
            self.sync_surface_agent_request_state(surface);
        }
    }

    pub(super) fn observe_agent_session_state(
        &mut self,
        surface: SurfaceId,
        state: wmcterm_core::AgentState,
    ) {
        let Some(session) = self.agent_sessions.get_mut(&surface) else {
            return;
        };
        let event = AgentSessionEvent {
            event_id: format!("terminal-input:{}", uuid::Uuid::new_v4()),
            lifecycle_generation: session.lifecycle.generation,
            lifecycle_id: session.lifecycle.external_id.clone(),
            sequence: None,
            occurred_at_unix_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_millis() as u64)
                .unwrap_or_default(),
            kind: AgentSessionEventKind::Telemetry(AgentTelemetry::StateObserved { state }),
        };
        let _ = session.apply_event(&event);
    }
}
