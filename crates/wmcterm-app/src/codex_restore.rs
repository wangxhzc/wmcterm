use super::*;

fn mode_args(mode: AgentRunMode) -> Vec<String> {
    match mode {
        AgentRunMode::Interactive => vec![
            "--ask-for-approval".into(),
            "on-request".into(),
            "--sandbox".into(),
            "workspace-write".into(),
        ],
        AgentRunMode::Unattended => vec!["--approve-for-me".into()],
        AgentRunMode::FullAccess => vec!["--dangerously-bypass-approvals-and-sandbox".into()],
    }
}

impl ManagedCodexRuntime {
    fn claim(&mut self, token: &str) -> Result<Vec<String>, String> {
        if self.saved.parent_surface.is_some()
            || self.launch_token.as_deref() != Some(token)
            || token.is_empty()
        {
            return Err("invalid or already consumed Codex launch token".into());
        }
        let mut args = if let Some(value) = &self.saved.permissions {
            let permissions: wmcterm_codex::CodexPermissions =
                serde_json::from_value(value.clone()).map_err(|error| error.to_string())?;
            if permissions.run_mode() != self.saved.mode {
                return Err("saved Codex permissions do not match its mode".into());
            }
            let policy = toml::Value::try_from(&permissions.approval_policy)
                .map_err(|error| error.to_string())?;
            vec![
                "-c".into(),
                format!("approval_policy={policy}"),
                "-c".into(),
                format!("approvals_reviewer={}", permissions.approvals_reviewer),
                "--sandbox".into(),
                permissions.sandbox,
            ]
        } else {
            mode_args(self.saved.mode)
        };
        if !self.saved.session_id.is_empty() {
            uuid::Uuid::parse_str(&self.saved.session_id)
                .map_err(|_| "invalid saved Codex thread ID")?;
            args.extend(["resume".into(), self.saved.session_id.clone()]);
        }
        self.launch_token = None;
        self.claimed = true;
        Ok(args)
    }
}

impl WmctermApplication {
    pub(super) fn codex_child_needs_reconnect(&self, terminal: TerminalId) -> bool {
        let Some(child) = self.managed_codex.get(&terminal) else {
            return false;
        };
        let Some(parent) = child
            .saved
            .parent_surface
            .and_then(|parent| self.state.surfaces.get(&parent))
        else {
            return false;
        };
        let SurfaceKind::Terminal(root) = parent.kind else {
            return false;
        };
        self.managed_codex.get(&root).is_some_and(|root| {
            root.verified && root.endpoint.is_some() && root.endpoint != child.endpoint
        })
    }

    pub(super) fn codex_launch_stopped(&mut self, params: &Value) -> Result<Value, String> {
        let (terminal, _) = self.codex_agent_update_route(params)?;
        let launch = self
            .managed_codex
            .get_mut(&terminal)
            .ok_or("Codex launch is not registered")?;
        if launch.endpoint.as_deref() != params["endpoint"].as_str() {
            return Err("stale Codex endpoint".into());
        }
        launch.endpoint = None;
        launch.claimed = false;
        launch.verified = false;
        Ok(json!({"ok": true}))
    }

    pub(super) fn claim_codex_launch(&mut self, params: &Value) -> Result<Value, String> {
        let (terminal, _) = self.codex_agent_update_route(params)?;
        let token = params["token"]
            .as_str()
            .ok_or("missing Codex launch token")?;
        let launch = self
            .managed_codex
            .get_mut(&terminal)
            .ok_or("no GUI-authorized Codex launch")?;
        Ok(json!({"args": launch.claim(token)?}))
    }

    pub(super) fn register_codex_launch(&mut self, params: &Value) -> Result<Value, String> {
        let (terminal, _) = self.codex_agent_update_route(params)?;
        let mode: AgentRunMode =
            serde_json::from_value(params["mode"].clone()).map_err(|error| error.to_string())?;
        let permissions: wmcterm_codex::CodexPermissions =
            serde_json::from_value(params["permissions"].clone())
                .map_err(|error| error.to_string())?;
        if permissions.run_mode() != mode {
            return Err("registered Codex permissions do not match its mode".into());
        }
        if mode == AgentRunMode::FullAccess
            && !self.managed_codex.get(&terminal).is_some_and(|launch| {
                launch.claimed
                    && launch.saved.mode == AgentRunMode::FullAccess
                    && launch.endpoint.is_none()
            })
        {
            return Err("full access requires a consumed GUI-authorized launch token".into());
        }
        let endpoint = params["endpoint"]
            .as_str()
            .ok_or("missing Codex endpoint")?;
        let port = endpoint
            .strip_prefix("ws://127.0.0.1:")
            .and_then(|port| port.parse::<u16>().ok());
        if port.is_none_or(|port| port == 0) {
            return Err("Codex endpoint must be a private loopback port".into());
        }
        let program = params["program"]
            .as_str()
            .filter(|program| !program.is_empty())
            .ok_or("missing Codex executable")?;
        let launch = self.managed_codex.entry(terminal).or_insert_with(|| {
            ManagedCodexRuntime::new(ManagedCodexResume {
                session_id: String::new(),
                mode,
                parent_surface: None,
                permissions: None,
            })
        });
        if launch.endpoint.is_some() {
            return Err("Codex is already registered for this terminal lifecycle".into());
        }
        launch.saved.mode = mode;
        launch.saved.permissions = Some(params["permissions"].clone());
        launch.endpoint = Some(endpoint.into());
        launch.program = Some(program.into());
        launch.cwd = params["cwd"].as_str().map(PathBuf::from);
        Ok(json!({"ok": true}))
    }

    pub(super) fn codex_launch_ready(&mut self, params: &Value) -> Result<Value, String> {
        let (terminal, surface) = self.codex_agent_update_route(params)?;
        let session_id = params["session_id"]
            .as_str()
            .ok_or("missing Codex thread ID")?;
        uuid::Uuid::parse_str(session_id).map_err(|_| "invalid Codex thread ID")?;
        let mode: AgentRunMode =
            serde_json::from_value(params["mode"].clone()).map_err(|error| error.to_string())?;
        let launch = self
            .managed_codex
            .get_mut(&terminal)
            .ok_or("Codex launch is not registered")?;
        if launch.endpoint.is_none() || mode != launch.saved.mode {
            return Err("Codex launch mode does not match registration".into());
        }
        launch.saved.session_id = session_id.into();
        launch.verified = true;
        if let Some(cwd) = params["cwd"].as_str() {
            launch.cwd = Some(cwd.into());
        }
        let event = AgentEvent::SessionStart {
            agent: "codex".into(),
            session_id: Some(session_id.into()),
        };
        self.record_agent_event(terminal, params, &event)?;
        self.apply_agent_event(terminal, event);
        if let Some(metadata) = self
            .state
            .surfaces
            .get_mut(&surface)
            .and_then(|surface| surface.agent.as_mut())
        {
            metadata.run_mode = mode;
        }
        let children: Vec<_> = self
            .managed_codex
            .iter()
            .filter(|(id, child)| {
                child.saved.parent_surface == Some(surface)
                    && (!self.terminals.contains_key(id)
                        || (self.codex_child_needs_reconnect(**id)
                            && matches!(
                                self.terminal_notices.get(id),
                                Some(TerminalNotice::Exited(_))
                            )))
            })
            .map(|(id, child)| (*id, child.saved.session_id.clone()))
            .collect();
        for (id, _) in children {
            self.terminals.remove(&id);
            if let Err(error) = self.spawn_terminal(id) {
                self.report_terminal_notice(id, TerminalNotice::SpawnFailed(error.to_string()));
            }
        }
        self.rebuild_scene();
        Ok(json!({"ok": true}))
    }

    pub(super) fn codex_turn_event(&mut self, params: &Value) -> Result<Value, String> {
        let (terminal, _) = self.codex_agent_update_route(params)?;
        let session_id = params["session_id"]
            .as_str()
            .ok_or("missing Codex thread ID")?;
        let launch = self
            .managed_codex
            .get(&terminal)
            .ok_or("Codex launch is not registered")?;
        if !launch.verified || launch.saved.session_id != session_id {
            return Err("stale Codex turn event".into());
        }
        self.validate_codex_session(terminal, session_id)?;
        let event: AgentEvent = serde_json::from_value(params["agent_event"].clone())
            .map_err(|error| error.to_string())?;
        if !matches!(
            event,
            AgentEvent::ToolUse { .. }
                | AgentEvent::Stop { .. }
                | AgentEvent::TurnComplete { .. }
                | AgentEvent::TurnFailed { .. }
        ) {
            return Err("invalid Codex turn event".into());
        }
        self.record_agent_event(terminal, params, &event)?;
        let state = self.apply_agent_event(terminal, event);
        self.rebuild_scene();
        Ok(json!({"state": state}))
    }

    pub(super) fn codex_child(&mut self, params: &Value) -> Result<Value, String> {
        let mut route = params.clone();
        route["surface_id"] = params["root_surface_id"].clone();
        route
            .as_object_mut()
            .ok_or("invalid Codex child params")?
            .remove("surface");
        let (root_terminal, root_surface) = self.codex_agent_update_route(&route)?;
        let root = self
            .managed_codex
            .get(&root_terminal)
            .ok_or("Codex root is not registered")?;
        let endpoint = root
            .endpoint
            .as_deref()
            .ok_or("Codex root endpoint is unavailable")?;
        let program = root
            .program
            .as_ref()
            .ok_or("Codex root executable is unavailable")?;
        let thread = params["thread_id"]
            .as_str()
            .ok_or("missing child thread ID")?;
        uuid::Uuid::parse_str(thread).map_err(|_| "invalid child thread ID")?;
        for (terminal, child) in &self.managed_codex {
            if child.saved.parent_surface == Some(root_surface)
                && child.saved.session_id == thread
                && let Some(surface) = self.surface_for_terminal(*terminal)
            {
                return Ok(json!({"surface_id": surface}));
            }
        }
        let mode = root.saved.mode;
        let endpoint = endpoint.to_owned();
        let mut create = params.clone();
        create["command"] = json!(wmcterm_codex::resume_command(program, &endpoint, thread));
        create["focus"] = json!(false);
        let root_workspace = self
            .workspace_for_surface(root_surface)
            .ok_or("Codex root workspace is unavailable")?;
        let (workspace, pane) = self.rpc_target_pane(&create)?;
        if workspace != root_workspace {
            return Err("Codex child must remain in its root workspace".into());
        }
        let options = parse_terminal_create_options(&create)?;
        let result =
            self.rpc_split_pane(workspace, pane, split_direction(&create)?, options, false)?;
        let surface: SurfaceId = serde_json::from_value(result["surface_id"].clone())
            .map_err(|error| error.to_string())?;
        if let Some(SurfaceKind::Terminal(terminal)) = self
            .state
            .surfaces
            .get(&surface)
            .map(|surface| &surface.kind)
        {
            let mut runtime = ManagedCodexRuntime::new(ManagedCodexResume {
                session_id: thread.into(),
                mode,
                parent_surface: Some(root_surface),
                permissions: None,
            });
            runtime.cwd = params["cwd"].as_str().map(PathBuf::from);
            runtime.endpoint = Some(endpoint);
            self.managed_codex.insert(*terminal, runtime);
        }
        Ok(result)
    }

    pub(super) fn codex_child_mode(&mut self, params: &Value) -> Result<Value, String> {
        let (_, root_surface) = self.codex_agent_update_route(params)?;
        let thread = params["thread_id"]
            .as_str()
            .ok_or("missing child thread ID")?;
        let mode: AgentRunMode =
            serde_json::from_value(params["mode"].clone()).map_err(|error| error.to_string())?;
        let terminal = self.managed_codex.iter_mut().find_map(|(terminal, child)| {
            if child.saved.parent_surface == Some(root_surface) && child.saved.session_id == thread
            {
                child.saved.mode = mode;
                Some(*terminal)
            } else {
                None
            }
        });
        if let Some(surface) = terminal.and_then(|terminal| self.surface_for_terminal(terminal))
            && let Some(metadata) = self
                .state
                .surfaces
                .get_mut(&surface)
                .and_then(|surface| surface.agent.as_mut())
        {
            metadata.run_mode = mode;
        }
        Ok(json!({"ok": true}))
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(crate) fn check_launch_rpc(
        app: &mut WmctermApplication,
        terminal: TerminalId,
        surface: SurfaceId,
        lifecycle: &str,
    ) {
        let permissions = wmcterm_codex::CodexPermissions::extract(&[
            "--dangerously-bypass-approvals-and-sandbox".into(),
        ])
        .unwrap()
        .0;
        let mut params = json!({"surface_id": surface, "lifecycle_id": lifecycle, "token": "secret",
            "mode": "full-access", "permissions": permissions, "endpoint": "ws://127.0.0.1:12345", "program": "codex"});
        assert!(app.register_codex_launch(&params).is_err());
        let mut launch = ManagedCodexRuntime::new(ManagedCodexResume {
            session_id: String::new(),
            mode: AgentRunMode::FullAccess,
            parent_surface: None,
            permissions: None,
        });
        launch.launch_token = Some("secret".into());
        app.managed_codex.insert(terminal, launch);
        params["lifecycle_id"] = json!("stale");
        assert!(app.claim_codex_launch(&params).is_err());
        params["lifecycle_id"] = json!(lifecycle);
        app.claim_codex_launch(&params).unwrap();
        assert!(app.claim_codex_launch(&params).is_err());
        app.register_codex_launch(&params).unwrap();
        assert!(app.register_codex_launch(&params).is_err());
        params["session_id"] = json!(uuid::Uuid::new_v4());
        params["mode"] = json!("unattended");
        assert!(app.codex_launch_ready(&params).is_err());
        assert!(app.managed_codex[&terminal].saved.session_id.is_empty());
        params["mode"] = json!("full-access");
        app.codex_launch_ready(&params).unwrap();
        let root_session = params["session_id"].as_str().unwrap().to_owned();
        let running = AgentEvent::ToolUse {
            tool: "active task".into(),
        };
        app.record_agent_event(terminal, &params, &running).unwrap();
        app.apply_agent_event(terminal, running);
        let reviewer = AgentEvent::SessionStart {
            agent: "codex".into(),
            session_id: Some(uuid::Uuid::new_v4().to_string()),
        };
        assert!(
            app.record_agent_event(terminal, &params, &reviewer)
                .is_err()
        );
        assert_eq!(
            app.state.surfaces[&surface]
                .agent
                .as_ref()
                .unwrap()
                .session_id
                .as_deref(),
            Some(root_session.as_str())
        );
        assert_eq!(
            app.state.surfaces[&surface].agent.as_ref().unwrap().state,
            wmcterm_core::AgentState::Running
        );
        assert_eq!(
            app.state.surfaces[&surface]
                .agent
                .as_ref()
                .unwrap()
                .run_mode,
            AgentRunMode::FullAccess
        );
        app.codex_launch_stopped(&params).unwrap();
        assert!(app.register_codex_launch(&params).is_err());
        app.managed_codex.remove(&terminal);
        app.agent_sessions.remove(&surface);
    }

    #[test]
    fn restored_launch_claims_all_three_modes_exactly_once() {
        for mode in [
            AgentRunMode::Interactive,
            AgentRunMode::Unattended,
            AgentRunMode::FullAccess,
        ] {
            let session_id = uuid::Uuid::new_v4().to_string();
            let mut runtime = ManagedCodexRuntime::new(ManagedCodexResume {
                session_id: session_id.clone(),
                mode,
                parent_surface: None,
                permissions: None,
            });
            runtime.launch_token = Some("secret".into());
            assert!(runtime.claim("wrong").is_err());
            assert!(!runtime.claimed);
            let args = runtime.claim("secret").unwrap();
            assert!(args.ends_with(&["resume".into(), session_id]));
            assert_eq!(
                wmcterm_codex::CodexPermissions::extract(&args)
                    .unwrap()
                    .0
                    .run_mode(),
                mode
            );
            assert!(runtime.claim("secret").is_err());
        }
    }

    #[test]
    fn restore_keeps_mode_but_resets_task_state_and_rejects_mismatched_identity() {
        for mode in [
            AgentRunMode::Interactive,
            AgentRunMode::Unattended,
            AgentRunMode::FullAccess,
        ] {
            let mut state = AppState::new("restore", PathBuf::from("/tmp"));
            let surface = state.surfaces.values_mut().next().unwrap();
            let SurfaceKind::Terminal(terminal_id) = surface.kind else {
                panic!("terminal")
            };
            let session_id = uuid::Uuid::new_v4().to_string();
            surface.agent = Some(wmcterm_core::AgentMetadata {
                name: "codex".into(),
                state: wmcterm_core::AgentState::Running,
                session_id: Some(session_id.clone()),
                run_mode: AgentRunMode::Interactive,
            });
            let terminal = TerminalResume {
                terminal_id,
                cwd: "/tmp".into(),
                command: None,
                scrollback: Some("history".into()),
                agent: Some(AgentResumeInfo::Codex {
                    session_id: session_id.clone(),
                }),
                managed_codex: Some(ManagedCodexResume {
                    session_id,
                    mode,
                    parent_surface: None,
                    permissions: None,
                }),
            };
            reconcile_restored_agent_modes(&mut state, std::slice::from_ref(&terminal));
            let metadata = state
                .surfaces
                .values()
                .next()
                .unwrap()
                .agent
                .as_ref()
                .unwrap();
            assert_eq!(metadata.run_mode, mode);
            assert_eq!(metadata.state, wmcterm_core::AgentState::Idle);
            let mut session = Session {
                version: SESSION_VERSION,
                app: state,
                terminals: vec![terminal],
                agent_sessions: vec![],
                window: None,
            };
            session.terminals[0]
                .managed_codex
                .as_mut()
                .unwrap()
                .session_id = uuid::Uuid::new_v4().to_string();
            discard_codex_resumes_without_record(&mut session, |_| true);
            assert!(session.terminals[0].managed_codex.is_none());
            assert!(session.terminals[0].agent.is_none());
            assert_eq!(session.terminals[0].scrollback.as_deref(), Some("history"));
        }
    }
}
