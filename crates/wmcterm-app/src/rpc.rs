use super::*;

impl WmctermApplication {
    pub(super) fn handle_rpc(&mut self, request: Request) -> Response {
        let id = request.id.clone();
        let result: Result<Value, String> = (|| {
            match request.method.as_str() {
            "app.diagnostics" => Ok(json!({
                "version": env!("CARGO_PKG_VERSION"),
                "server_executable": env::current_exe().ok(),
                "client_executable": request.params.get("client_executable"),
                "provider_versions": request.params.get("provider_versions"),
                "config_path": wmcterm_config::config_path(),
                "terminals": self.terminals.iter().map(|(id, runtime)| json!({
                    "terminal_id": id, "surface_id": self.surface_for_terminal(*id),
                    "lifecycle_id": runtime.lifecycle_id,
                    "agent": self.surface_for_terminal(*id).and_then(|surface| self.state.surfaces.get(&surface)).and_then(|surface| surface.agent.as_ref()),
                    "last_hook": self.last_hook_delivery.get(id),
                    "cwd": self.cwd_for_terminal(*id),
                    "agent_session": self.surface_for_terminal(*id).and_then(|surface| self.agent_sessions.get(&surface)),
                    "input_focused": self.input_focus_terminal == Some(*id),
                    "mouse_tracking": runtime.snapshot.mouse_tracking,
                    "keyboard_flags": runtime.snapshot.keyboard_flags,
                    "focus_reporting": runtime.snapshot.focus_reporting,
                    "bracketed_paste": runtime.snapshot.bracketed_paste
                })).collect::<Vec<_>>()
            })),
            "app.status" => Ok(json!({
                "product": "wmcterm",
                "version": env!("CARGO_PKG_VERSION"),
                "active_workspace": self.state.active_workspace().map(|workspace| workspace.id),
                "active_terminal": self.active_terminal_id(),
                "workspaces": self.state.workspaces.len(),
                "panes": self.state.panes.len(),
                "surfaces": self.state.surfaces.len(),
                "running_terminals": self.terminals.len(),
                "unread_notifications": self.state.notifications.iter().filter(|item| !item.read).count(),
            })),
            "app.capabilities" => Ok(json!({
                "protocol_version": 5,
                "product": "wmcterm",
                "supported_agents": AgentKind::ALL,
                "version": env!("CARGO_PKG_VERSION"),
                "transport_limits": {
                    "request_bytes": wmcterm_ipc::MAX_REQUEST_BYTES,
                    "response_bytes": wmcterm_ipc::MAX_RESPONSE_BYTES,
                    "connections": wmcterm_ipc::MAX_CONNECTIONS,
                    "io_timeout_ms": wmcterm_ipc::IO_TIMEOUT.as_millis(),
                },
                "document_refresh": {
                    "background": true,
                    "interval_ms": 1000,
                    "log_batch_bytes": wmcterm_surface::LOG_READ_BYTES,
                    "log_history_bytes": wmcterm_surface::MAX_LOG_HISTORY_BYTES,
                    "log_line_bytes": wmcterm_surface::MAX_LOG_LINE_BYTES,
                },
                "session_checkpoints": { "interval_seconds": 5, "history_interval_seconds": 60 },
                "context": {
                    "marker_env": "WMCTERM",
                    "workspace_env": "WMCTERM_WORKSPACE_ID",
                    "pane_env": "WMCTERM_PANE_ID",
                    "surface_env": "WMCTERM_SURFACE_ID",
                    "terminal_lifecycle_env": "WMCTERM_TERMINAL_LIFECYCLE_ID",
                    "control_socket_env": "WMCTERM_CONTROL_SOCKET",
                    "legacy_socket_env": "WMCTERM_SOCKET",
                    "implicit_surface_target": true,
                    "explicit_surface_target": true,
                    "atomic_target_create": true,
                    "focus_preserving_create": true,
                    "focus_preserving_documents": true,
                    "atomic_close_others": true,
                    "destructive_rpc_requires_target": true,
                    "non_intrusive_scrollback_read": true,
                    "screen_revisions": true,
                    "screen_wait_cli": true,
                },
                "create_options": [
                    "workspace", "pane", "surface", "direction", "focus", "command", "cwd",
                    "environment", "title"
                ],
                "agent_approval": {
                    "register_method": "agent.approval.register",
                    "poll_method": "agent.approval.poll",
                    "sources": ["codex_app_server", "claude_hook"],
                    "decision_modes": ["deny", "once", "always", "all"],
                    "claude_permission_changes": "always applies the single displayed suggestion; its label describes the operation, and the message shows scope and storage",
                    "fail_closed": true
                },
                "codex_restore": {
                    "managed_modes": ["interactive", "unattended", "full-access"],
                    "preserves_mode": true,
                    "rebuilds_teams_bridge": true,
                    "permission_verification": "server response before turn/start",
                    "settings_updates": "verified threads only; non-null permission, reviewer and cwd updates rejected; restart to change the launch profile",
                    "turn_workspace": "turn/start preserves verified cwd and runtime workspace roots; matching overrides are removed and changes rejected",
                    "full_access": "GUI confirmation or persisted managed session; not a CLI/RPC launch option",
                    "failure": "no automatic permission downgrade; Enter retries an exited terminal",
                    "legacy": "missing managed restore record uses interactive provider resume"
                },
                "claude_launch": {
                    "interactive_permission_mode": "manual",
                    "unattended_permission_mode": "auto",
                    "hook_disabling_options": "bare, safe-mode, restricted and custom setting-sources rejected",
                    "hook_settings_validation": "additional settings and discovered user/project settings reject disableAllHooks and allowManagedHooksOnly; inherited SIMPLE and SAFE_MODE flags rejected",
                    "requires_installed_hooks": true,
                    "approval_hook_failure": "block; requires Claude 2.1.295+",
                    "hook_command_form": "exec command plus args; no shell required",
                    "auto_denials": "PermissionDenied is informational; never retries or creates approval buttons",
                    "directory_events": "root CwdChanged and DirectoryAdded update session metadata; child directories cannot change the root",
                    "prompt_submission_event": true,
                    "detached_modes": "bg, background, cloud, desktop and tmux rejected"
                },
                "codex_input": {
                    "methods": ["item/tool/requestUserInput", "mcpServer/elicitation/request"],
                    "forms": "ordinary non-secret questions and primitive MCP form schemas; unsupported forms stay native",
                    "reply": "resolution.text is a JSON object mapping field ids to strings",
                    "deadline_ms": 540000,
                    "strict_permissions": "strict choice grants turn scope with strictAutoReview; never session scope"
                },
                "claude_input": {
                    "methods": ["Elicitation", "ElicitationResult"],
                    "forms": "primitive MCP form schemas; URL, secret and complex forms stay native",
                    "ownership": "supported form hooks block and replace the native dialog; failures cancel",
                    "reply": "resolution.text is a JSON object mapping field ids to strings",
                    "binding": "workspace, surface, terminal lifecycle and active session",
                    "deadline_ms": 540000
                },
                "program_status": {"protocol": "OSC 7501", "max_records": 64, "priority": "identity-bearing hooks and pending requests precede anonymous OSC", "actionable": false},
                "codex_native_tool_hooks": {
                    "events": ["PreToolUse", "PostToolUse", "PermissionRequest", "PreModelRequest"],
                    "checked_version": "0.167.0",
                    "binding": "explicit surface, terminal lifecycle, root session, turn and tool input",
                    "actionable": false,
                    "installation": "explicit Hook installation and native Codex review required"
                },
                "agent_events": [
                    "session_start", "tool_use", "permission_request", "needs_input",
                    "turn_complete", "turn_failed", "notification", "stop", "state_observed"
                ],
                "terminal_input": {
                    "local_search": "Ctrl+Alt+F; next match Ctrl+Alt+G",
                    "mouse_override": "Shift forces local selection, scroll, menu and primary paste",
                    "protocols": ["SGR mouse including hover", "focus reporting", "Kitty keyboard press/repeat/release"]
                },
                "ssh": {
                    "shell": "bash >= 4.2.46",
                    "target": "explicit surface_id UUID; integrated idle local Bash",
                    "remote_directory": "~/.wmcterm/tmp",
                    "max_command_bytes": 16384,
                    "max_output_bytes": 1048576,
                    "output": "merged PTY bytes, UTF-8 lossy, ANSI preserved",
                    "persistent": false,
                    "focus": false,
                    "cli_new": "ssh HOST [--new] [--pane UUID]; defaults to WMCTERM_PANE_ID, splits right without focus and waits for local and remote readiness",
                    "agent_usage": "only when the user explicitly requests wmcterm SSH",
                    "cli_command_sources": ["--command", "--command-file", "--command-stdin"],
                    "exec_expected_host": "optional exact host match, checked atomically with connection_id and ready state",
                    "authentication": "interactive OpenSSH; connecting includes authentication waits"
                },
                "methods": [
                    "ssh.connect", "ssh.status", "ssh.exec", "ssh.cancel",
                    "app.status", "app.diagnostics", "workspace.list", "workspace.create", "workspace.activate",
                    "workspace.rename", "workspace.describe", "workspace.move",
                    "workspace.cwd",
                    "workspace.close", "pane.list", "pane.split", "pane.focus", "pane.zoom",
                    "pane.close", "pane.close-others", "surface.list", "surface.new", "surface.activate",
                    "surface.close", "surface.close-others", "surface.read", "surface.search", "surface.markdown",
                    "surface.logs", "screen.read", "send", "send.line", "terminal.search", "terminal.clear",
                    "terminal.scroll", "notification.list", "notification.mark", "notify",
                    "agent.update", "agent.request.list", "agent.request.reply", "agent.request.cancel",
                    "agent.input.register", "agent.input.poll", "agent.input.reconcile",
                    "agent.approval.register", "agent.approval.poll"
                ],
                "control_keys": [
                    "enter", "tab", "escape", "backspace", "up", "down", "left", "right",
                    "ctrl+a..ctrl+z", "ctrl+space", "ctrl+[", "ctrl+\\", "ctrl+]", "ctrl+^", "ctrl+_"
                ]
            })),
            "workspace.list" => Ok(json!(
                self.state
                    .workspaces
                    .iter()
                    .map(|w| json!({"id": w.id, "name": w.name, "cwd": w.cwd, "status": w.status, "description": w.description, "git_branch": w.git_branch, "pull_request": w.pull_request, "listening_ports": w.listening_ports}))
                    .collect::<Vec<_>>()
            )),
            "workspace.activate" | "workspace.rename" | "workspace.describe" | "workspace.cwd"
            | "workspace.move" | "workspace.close" => request
                .params
                .get("id")
                .and_then(Value::as_str)
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .ok_or_else(|| "invalid params.id".to_owned())
                .and_then(|workspace| {
                    let command = match request.method.as_str() {
                        "workspace.activate" => WorkspaceCommand::Activate { id: workspace },
                        "workspace.rename" => WorkspaceCommand::Rename {
                            id: workspace,
                            name: request
                                .params
                                .get("name")
                                .and_then(Value::as_str)
                                .ok_or_else(|| "missing params.name".to_owned())?
                                .to_owned(),
                        },
                        "workspace.describe" => WorkspaceCommand::Describe {
                            id: workspace,
                            description: request
                                .params
                                .get("description")
                                .and_then(Value::as_str)
                                .ok_or_else(|| "missing params.description".to_owned())?
                                .to_owned(),
                        },
                        "workspace.cwd" => {
                            let input = request
                                .params
                                .get("cwd")
                                .and_then(Value::as_str)
                                .ok_or_else(|| "missing params.cwd".to_owned())?;
                            let current = self
                                .state
                                .workspaces
                                .iter()
                                .find(|item| item.id == workspace)
                                .ok_or_else(|| "workspace not found".to_owned())?
                                .cwd
                                .clone();
                            WorkspaceCommand::ChangeDirectory {
                                id: workspace,
                                cwd: resolve_existing_directory(input, &current)?,
                            }
                        }
                        "workspace.move" => WorkspaceCommand::Move {
                            id: workspace,
                            offset: request
                                .params
                                .get("offset")
                                .and_then(Value::as_i64)
                                .unwrap_or(0) as isize,
                        },
                        "workspace.close" => {
                            let closed = self.remove_workspace_by_id(workspace);
                            return Ok(json!({
                                "workspace_id": workspace,
                                "closed": closed,
                                "confirmation_required": false,
                            }));
                        }
                        _ => unreachable!(),
                    };
                    self.state
                        .execute(AppCommand::Workspace(command))
                        .map_err(|e| e.to_string())?;
                    self.resize_terminals();
                    if request.method == "workspace.cwd" {
                        self.refresh_workspace_metadata();
                    }
                    self.rebuild_scene();
                    Ok(json!({"ok": true}))
                }),
            "workspace.create" => {
                let name = request
                    .params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("workspace")
                    .to_owned();
                let process_cwd = env::current_dir().map_err(|error| error.to_string())?;
                let cwd_input = request
                    .params
                    .get("cwd")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| process_cwd.to_str().unwrap_or("."));
                // Validate before mutating AppState. A bad Agent-provided cwd
                // must not leave an active workspace whose first PTY can never
                // start.
                let cwd = resolve_existing_directory(cwd_input, &process_cwd)?;
                self.state
                    .execute(AppCommand::Workspace(WorkspaceCommand::Create {
                        name,
                        cwd,
                    }))
                    .map_err(|e| e.to_string())
                    .and_then(|_| {
                        if let Some(command) = request.params.get("command").and_then(Value::as_str)
                            && let Some(terminal) = self.active_terminal_id()
                        {
                            self.commands.insert(terminal, command.to_owned());
                        }
                        let missing: Vec<_> = self
                            .state
                            .surfaces
                            .values()
                            .filter_map(|s| match s.kind {
                                SurfaceKind::Terminal(t) if !self.terminals.contains_key(&t) => {
                                    Some(t)
                                }
                                _ => None,
                            })
                            .collect();
                        for terminal in missing {
                            if let Err(error) = self.spawn_terminal(terminal) {
                                let message = error.to_string();
                                self.report_terminal_notice(
                                    terminal,
                                    TerminalNotice::SpawnFailed(message.clone()),
                                );
                                self.rebuild_scene();
                                return Err(message);
                            }
                        }
                        self.resize_terminals();
                        self.rebuild_scene();
                        Ok(json!({"workspace_id": self.state.active_workspace().map(|w| w.id)}))
                    })
            }
            "pane.split" => {
                let placement = split_direction(&request.params)?;
                let focus = request
                    .params
                    .get("focus")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let options = parse_terminal_create_options(&request.params)?;
                let (workspace, pane) = self.rpc_target_pane(&request.params)?;
                self.rpc_split_pane(workspace, pane, placement, options, focus)
            }
            "pane.list" => {
                let mut panes = Vec::new();
                let workspace_id = optional_uuid_param(&request.params, "workspace")?;
                let workspace = match workspace_id {
                    Some(id) => self.state.workspaces.iter().find(|workspace| workspace.id == id),
                    None => self.state.active_workspace(),
                };
                if workspace_id.is_some() && workspace.is_none() {
                    return Err("workspace not found".to_owned());
                }
                if let Some(workspace) = workspace {
                    let mut ids = Vec::new();
                    workspace.root.leaves(&mut ids);
                    panes.extend(ids.into_iter().filter_map(|id| self.state.panes.get(&id)).map(
                        |pane| json!({"id": pane.id, "surfaces": pane.surfaces, "active_surface": pane.active_surface, "zoomed": pane.zoomed}),
                    ));
                }
                Ok(json!(panes))
            }
            "pane.close" | "pane.zoom" => {
                let pane = request
                    .params
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|value| uuid::Uuid::parse_str(value).ok())
                    .ok_or_else(|| "invalid or missing params.id".to_owned())?;
                if request.method == "pane.close" {
                    if !self.state.panes.contains_key(&pane) {
                        return Err("pane not found".into());
                    }
                    let can_remove = self.state.workspaces.iter().any(|workspace| {
                        if !workspace.root.contains(pane) {
                            return false;
                        }
                        let mut leaves = Vec::new();
                        workspace.root.leaves(&mut leaves);
                        leaves.len() > 1
                    });
                    if !can_remove {
                        return Err("cannot close the last pane in a workspace".into());
                    }
                    let closed = self.remove_pane_by_id(pane);
                    self.resize_terminals();
                    self.rebuild_scene();
                    return Ok(json!({
                        "closed": usize::from(closed),
                        "closed_ids": if closed { vec![pane] } else { Vec::new() },
                        "confirmation_required": false,
                    }));
                } else {
                    self.state
                        .execute(AppCommand::Pane(PaneCommand::ToggleZoom { id: pane }))
                        .map_err(|e| e.to_string())?;
                    self.rebuild_scene();
                }
                Ok(json!({"ok": true}))
            }
            "pane.close-others" => {
                let anchor = optional_uuid_param(&request.params, "anchor")?
                    .ok_or_else(|| "missing params.anchor".to_owned())?;
                let workspace = self
                    .state
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.root.contains(anchor))
                    .ok_or_else(|| "anchor pane not found".to_owned())?;
                let mut candidates = Vec::new();
                workspace.root.leaves(&mut candidates);
                let targets = wmcterm_core::plan_close_others(&candidates, anchor)
                    .map_err(|error| error.to_string())?;
                let closed_ids = targets
                    .iter()
                    .copied()
                    .filter(|pane| self.remove_pane_by_id(*pane))
                    .collect::<Vec<_>>();
                self.resize_terminals();
                self.rebuild_scene();
                Ok(json!({
                    "anchor": anchor,
                    "closed": closed_ids.len(),
                    "closed_ids": closed_ids,
                    "pending_ids": Vec::<PaneId>::new(),
                    "confirmation_required": false,
                }))
            }
            "surface.list" => {
                let workspace_id = optional_uuid_param(&request.params, "workspace")?
                    .ok_or_else(|| "missing params.workspace".to_owned())?;
                let workspace = self.state.workspaces.iter().find(|workspace| workspace.id == workspace_id)
                    .ok_or_else(|| "workspace not found".to_owned())?;
                let mut pane_ids = Vec::new();
                workspace.root.leaves(&mut pane_ids);
                let surfaces = pane_ids.into_iter()
                    .filter_map(|pane| self.state.panes.get(&pane))
                    .flat_map(|pane| pane.surfaces.iter())
                    .filter_map(|id| self.state.surfaces.get(id))
                    .map(|surface| {
                        let terminal = match surface.kind { SurfaceKind::Terminal(id) => Some(id), _ => None };
                        json!({"id": surface.id, "title": surface.title, "kind": surface.kind, "terminal": terminal, "attention": surface.attention, "agent": surface.agent})
                    }).collect::<Vec<_>>();
                Ok(json!(surfaces))
            },
            "surface.read" | "surface.search" => {
                let surface = request.params.get("id").and_then(Value::as_str).and_then(|value| uuid::Uuid::parse_str(value).ok()).or_else(|| {
                    self.state.active_workspace().and_then(|workspace| self.state.panes.get(&workspace.active_pane)).and_then(|pane| pane.active_surface())
                }).ok_or_else(|| "no surface selected".to_owned())?;
                let backend = self.surface_backends.get(&surface).ok_or_else(|| "surface is not a document or log".to_owned())?;
                if request.method == "surface.read" {
                    Ok(json!({"text": backend.render_text()}))
                } else {
                    let query = request.params.get("query").and_then(Value::as_str).ok_or_else(|| "missing params.query".to_owned())?;
                    Ok(json!(backend.search(query).into_iter().map(|item| json!({"line": item.line, "text": item.text})).collect::<Vec<_>>()))
                }
            }
            "surface.activate" => request
                .params
                .get("id")
                .and_then(Value::as_str)
                .and_then(|value| uuid::Uuid::parse_str(value).ok())
                .ok_or_else(|| "invalid params.id".to_owned())
                .and_then(|surface: SurfaceId| {
                    self.activate_surface_id(surface).map_err(|error| error.to_string())?;
                    Ok(json!({"ok": true}))
                }),
            "ssh.connect" | "ssh.status" | "ssh.exec" | "ssh.cancel" => {
                let surface = request.params.get("surface_id").and_then(Value::as_str)
                    .ok_or_else(|| "SSH operations require an explicit surface_id UUID".to_owned())?;
                uuid::Uuid::parse_str(surface).map_err(|_| "invalid surface_id UUID".to_owned())?;
                let mut params = request.params.clone();
                if params.get("surface").is_some_and(|value| value.as_str() != Some(surface)) {
                    return Err("surface and surface_id disagree".into());
                }
                params["surface"] = surface.into();
                rpc_target_pane_in_state(&self.state, &params)?;
                let terminal = self.terminal_for_params(&params)?;
                let runtime = self.terminals.get(&terminal).ok_or_else(|| "target terminal is not running".to_owned())?;
                runtime.session.ssh_request(&request.method, request.params.clone())
            }
            "screen.read" => {
                let terminal = self.terminal_for_params(&request.params)?;
                let runtime = self.terminals.get(&terminal).ok_or_else(|| "target terminal is not running".to_owned())?;
                let include_scrollback = request
                    .params
                    .get("scrollback")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let lines = optional_screen_lines(&request.params)?;
                let mut text = if include_scrollback {
                    runtime
                        .session
                        .export_history()
                        .ok_or_else(|| "timed out reading terminal scrollback".to_owned())?
                } else {
                    runtime.snapshot.to_plain_text()
                };
                if let Some(lines) = lines {
                    text = tail_lines(&text, lines);
                }
                let snapshot = &runtime.snapshot;
                Ok(json!({
                    "text": text,
                    "revision": runtime.snapshot_revision,
                    "cols": snapshot.cols,
                    "rows": snapshot.rows,
                    "cursor": snapshot.cursor.as_ref().map(|cursor| json!({
                        "column": cursor.x,
                        "row": cursor.y,
                        "visible": cursor.visible,
                    })),
                    "scrollback_rows": snapshot.scrollback_rows,
                    "viewport_offset": snapshot.viewport_offset,
                    "at_bottom": snapshot.viewport_offset == 0,
                    "truncated_top": !include_scrollback && snapshot.screen_row_origin > 0,
                    "includes_scrollback": include_scrollback,
                }))
            }
            "send" | "send.line" => {
                let text = request.params.get("text").and_then(Value::as_str).ok_or_else(|| "missing params.text".to_owned())?;
                let terminal = self.terminal_for_params(&request.params)?;
                let runtime = self.terminals.get(&terminal).ok_or_else(|| "target terminal is not running".to_owned())?;
                let command = if request.method == "send.line" {
                    SessionCommand::PasteLine(text.to_owned())
                } else if request
                    .params
                    .get("paste")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    SessionCommand::Paste(text.to_owned())
                } else {
                    SessionCommand::Input(text.as_bytes().to_vec())
                };
                runtime.session.send(command).map_err(|error| error.to_string())?;
                Ok(json!({"ok": true}))
            }
            "terminal.search" => {
                let query = request.params.get("query").and_then(Value::as_str).ok_or_else(|| "missing params.query".to_owned())?.to_owned();
                let direction = request.params.get("direction").and_then(Value::as_i64).unwrap_or(1) as isize;
                let terminal = self.terminal_for_params(&request.params)?;
                let runtime = self.terminals.get(&terminal).ok_or_else(|| "terminal is not running".to_owned())?;
                runtime.session.send(SessionCommand::Search { query, direction }).map_err(|e| e.to_string())?;
                Ok(json!({"ok": true}))
            }
            "terminal.clear" => {
                let terminal = self.terminal_for_params(&request.params)?;
                let runtime = self.terminals.get(&terminal).ok_or_else(|| "terminal is not running".to_owned())?;
                runtime.session.send(SessionCommand::ClearScrollback).map_err(|e| e.to_string())?;
                Ok(json!({"ok": true}))
            }
            "terminal.scroll" => {
                let lines = request.params.get("lines").and_then(Value::as_i64).unwrap_or(0) as isize;
                let terminal = self.terminal_for_params(&request.params)?;
                let runtime = self.terminals.get(&terminal).ok_or_else(|| "terminal is not running".to_owned())?;
                runtime.session.send(SessionCommand::Scroll(lines)).map_err(|e| e.to_string())?;
                Ok(json!({"ok": true}))
            }
            "surface.new" => {
                let focus = request
                    .params
                    .get("focus")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let options = parse_terminal_create_options(&request.params)?;
                let (workspace, pane) = self.rpc_target_pane(&request.params)?;
                self.rpc_create_surface(workspace, pane, options, focus)
            }
            "surface.markdown" | "surface.logs" => {
                let path = request.params.get("path").and_then(Value::as_str)
                    .ok_or_else(|| "missing params.path".to_owned())?;
                let focus = request.params.get("focus").and_then(Value::as_bool).unwrap_or(true);
                let (workspace, pane) = self.rpc_target_pane(&request.params)?;
                self.rpc_open_text_surface(
                    workspace,
                    pane,
                    std::path::Path::new(path),
                    request.method == "surface.logs",
                    focus,
                )
            },
            "surface.close" => {
                let surface = optional_uuid_param(&request.params, "id")?
                    .ok_or_else(|| "missing params.id".to_owned())?;
                if !self.state.surfaces.contains_key(&surface) {
                    return Err("surface not found".into());
                }
                let closed = self.remove_surface_by_id(surface);
                self.resize_terminals();
                self.rebuild_scene();
                Ok(json!({
                    "closed": usize::from(closed),
                    "closed_ids": if closed { vec![surface] } else { Vec::new() },
                    "confirmation_required": false,
                }))
            }
            "surface.close-others" => {
                let anchor = optional_uuid_param(&request.params, "anchor")?
                    .ok_or_else(|| "missing params.anchor".to_owned())?;
                let pane = self
                    .state
                    .panes
                    .values()
                    .find(|pane| pane.surfaces.contains(&anchor))
                    .ok_or_else(|| "anchor surface not found".to_owned())?;
                let targets = wmcterm_core::plan_close_others(&pane.surfaces, anchor)
                    .map_err(|error| error.to_string())?;
                let closed_ids = targets
                    .iter()
                    .copied()
                    .filter(|surface| self.remove_surface_by_id(*surface))
                    .collect::<Vec<_>>();
                self.resize_terminals();
                self.rebuild_scene();
                Ok(json!({
                    "anchor": anchor,
                    "closed": closed_ids.len(),
                    "closed_ids": closed_ids,
                    "pending_ids": Vec::<SurfaceId>::new(),
                    "confirmation_required": false,
                }))
            }
            "notify" => {
                let title = request
                    .params
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("wmcterm")
                    .to_owned();
                let body = request
                    .params
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let terminal = self.terminal_for_params(&request.params)?;
                self.notify_terminal(terminal, &body);
                if let Some(last) = self.state.notifications.last_mut() {
                    last.title = title;
                }
                self.rebuild_scene();
                Ok(json!({"ok": true}))
            }
            "notification.list" => Ok(json!(self.state.notifications)),
            "notification.mark" => {
                let notification = request
                    .params
                    .get("id")
                    .and_then(Value::as_str)
                    .and_then(|value| uuid::Uuid::parse_str(value).ok())
                    .ok_or_else(|| "invalid params.id".to_owned())?;
                let read = request
                    .params
                    .get("read")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                if !self
                    .state
                    .notifications
                    .iter()
                    .any(|item| item.id == notification)
                {
                    return Err("notification not found".into());
                }
                self.set_notification_read(notification, read);
                self.rebuild_scene();
                Ok(json!({"ok": true}))
            }
            "agent.approval.register" => {
                if request.params["request"]["kind"] == "input" { return Err("use agent.input.register for input requests".into()); }
                self.register_agent_approval(&request.params)
            },
            "agent.input.register" => self.register_agent_input(&request.params),
            "agent.input.poll" => self.poll_agent_input(&request.params),
            "agent.input.reconcile" => self.reconcile_claude_input(&request.params),
            "agent.approval.poll" => self.poll_agent_approval(&request.params),
            "agent.request.list" => self
                .pending_agent_requests
                .list()
                .map(|requests| json!(requests))
                .map_err(|error| error.to_string()),
            "agent.request.reply" | "agent.request.cancel" => {
                let request_id = request
                    .params
                    .get("request_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "missing params.request_id".to_owned())?;
                let pending_request = self
                    .pending_agent_requests
                    .get(request_id)
                    .map_err(|error| error.to_string())?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|value| value.as_millis() as u64)
                    .unwrap_or_default();
                let outcome = if request.method == "agent.request.cancel" {
                    self.pending_agent_requests.cancel(
                        request_id,
                        request
                            .params
                            .get("reason")
                            .and_then(Value::as_str)
                            .unwrap_or("cancelled"),
                        now,
                    )
                } else {
                    let resolution = request
                        .params
                        .get("resolution")
                        .cloned()
                        .map(serde_json::from_value::<AgentResolution>)
                        .transpose()
                        .map_err(|error| error.to_string())?
                        .unwrap_or_default();
                    if let Some(form) = self.agent_input_forms.get(request_id) {
                        if pending_request.is_some() && self.poll_agent_input(&json!({"request_id": request_id}))?["status"] != "pending" { return Err("stale input request".into()); }
                        if resolution.approved != Some(false) && resolution.choice_id.as_deref() != Some("deny") {
                            form.response(Some(resolution.text.as_deref().ok_or("input reply requires resolution.text")?))?;
                        }
                    }
                    if resolution.choice_id.as_deref() == Some("strict") && pending_request.as_ref().is_some_and(|request| !request.choices.iter().any(|choice| choice.id == "strict")) { return Err("strict review is not available for this request".into()); }
                    self.pending_agent_requests
                        .resolve(request_id, resolution, now)
                }
                .map_err(|error| error.to_string())?;
                let reply = match outcome {
                    CompleteOutcome::Completed(reply) => {
                        if let Some(request) = pending_request.as_ref() {
                            self.conclude_agent_request_state(request);
                        } else if let Ok(surface) = uuid::Uuid::parse_str(&reply.surface_id) {
                            self.sync_surface_agent_request_state(surface);
                        }
                        reply
                    }
                    CompleteOutcome::AlreadyCompleted(reply) => reply,
                };
                if request_id.starts_with("approval:")
                    && let Some(action) = completed_approval_action(&reply)
                {
                    self.remember_agent_approval_result(request_id.to_owned(), action);
                }
                self.rebuild_scene();
                Ok(json!(reply))
            }
            "agent.codex.claim" => self.claim_codex_launch(&request.params),
            "agent.codex.register" => self.register_codex_launch(&request.params),
            "agent.codex.ready" => self.codex_launch_ready(&request.params),
            "agent.codex.turn" => self.codex_turn_event(&request.params),
            "agent.codex.child" => self.codex_child(&request.params),
            "agent.codex.child-mode" => self.codex_child_mode(&request.params),
            "agent.codex.stopped" => self.codex_launch_stopped(&request.params),
            "agent.update" => {
                let claude_hook = request
                    .params
                    .get("claude_hook")
                    .cloned()
                    .map(serde_json::from_value::<ClaudeHookEvent>)
                    .transpose()
                    .map_err(|error| error.to_string())?;
                let codex_hook = request
                    .params
                    .get("codex_hook")
                    .cloned()
                    .map(serde_json::from_value::<CodexSubagentHookEvent>)
                    .transpose()
                    .map_err(|error| error.to_string())?;
                let codex_session_end = request
                    .params
                    .get("codex_session_end")
                    .map(|value| {
                        value
                            .as_str()
                            .filter(|session| !session.trim().is_empty())
                            .map(str::to_owned)
                            .ok_or_else(|| "invalid codex_session_end".to_owned())
                    })
                    .transpose()?;
                serde_json::from_value::<AgentEvent>(request.params.clone())
                    .map_err(|e| e.to_string())
                    .and_then(|event| {
                    let terminal = if codex_hook.is_some() || codex_session_end.is_some() || request.params.get("codex_tool_hook").is_some() {
                        let (terminal, _) = self.codex_agent_update_route(&request.params)?;
                        self.validate_agent_event_route(terminal, &request.params)?;
                        terminal
                    } else {
                        self.terminal_for_params(&request.params)?
                    };
                    if let Some(hook) = codex_hook.as_ref() {
                        let state = self.record_codex_subagent_hook(terminal, hook)?;
                        self.rebuild_scene();
                        return Ok(json!({
                            "state": state,
                            "codex_subagents": self.codex_subagents.len()
                        }));
                    }
                    if let Some(session_id) = codex_session_end.as_deref() {
                        self.validate_codex_session(terminal, session_id)?;
                    }
                    let mut tool_tracker = None;
                    let event = if request.params.get("codex_tool_hook").is_some() {
                        let Some((tracker, projected)) = self.project_codex_tool_hook(terminal, &request.params, &event)? else {
                            return Ok(json!({"ignored": true}));
                        };
                        tool_tracker = Some(tracker);
                        projected
                    } else {
                        event
                    };
                    if let Some(hook) = claude_hook.as_ref() {
                        validate_claude_hook_directories(hook)?;
                        if hook.agent_id.is_some() && matches!(hook.kind, ClaudeHookKind::SessionStart | ClaudeHookKind::SessionEnd) {
                            return Err("Claude child hooks cannot start or end the root session".into());
                        }
                        if hook.to_agent_event() != event {
                            return Err("Claude hook does not match its projected Agent event".into());
                        }
                        if hook.kind != ClaudeHookKind::SessionStart {
                            self.validate_claude_hook(terminal, hook)?;
                        }
                    }
                    if !self.record_agent_event(terminal, &request.params, &event)? {
                        if let Some(session_id) = codex_session_end.as_deref() {
                            self.end_codex_subagent_session(terminal, session_id)?;
                        }
                        return Ok(json!({"ignored": true}));
                    }
                    if let Some(tracker) = tool_tracker {
                        self.codex_tool_hooks.insert(terminal, tracker);
                    } else if matches!(&event, AgentEvent::SessionStart { .. } | AgentEvent::Stop { .. } | AgentEvent::TurnComplete { .. } | AgentEvent::TurnFailed { .. })
                        || (matches!(&event, AgentEvent::ToolUse { .. }) && request.params["codex_hook_event"] == "UserPromptSubmit")
                    {
                        let session = self.agent_sessions.get(&self.surface_for_terminal(terminal).ok_or("surface not found")?).and_then(|session| session.session_id.as_deref()).unwrap_or_default();
                        let lifecycle = &self.terminals[&terminal].lifecycle_id;
                        let timestamp = request.params["hook_started_at_unix_ns"].as_u64().unwrap_or_default();
                        self.codex_tool_hooks.entry(terminal).or_default().reset(session, lifecycle, timestamp);
                        if matches!(&event, AgentEvent::Stop { .. } | AgentEvent::TurnComplete { .. } | AgentEvent::TurnFailed { .. }) {
                            self.codex_tool_hooks.get_mut(&terminal).unwrap().close();
                        }
                    }
                    if request.params.get("hook_executable").is_some() {
                        self.last_hook_delivery.insert(terminal, json!({
                            "executable": request.params.get("hook_executable"),
                            "started_at_unix_ns": request.params.get("hook_started_at_unix_ns"),
                            "received_at_unix_ms": unix_time_ms(),
                            "event": request.params.get("event")
                        }));
                    }
                    let metadata_only = claude_hook.as_ref().is_some_and(|hook| matches!(hook.kind, ClaudeHookKind::CwdChanged | ClaudeHookKind::DirectoryAdded | ClaudeHookKind::ElicitationResult));
                    let state = if metadata_only {
                        self.state.surfaces.get(&self.surface_for_terminal(terminal).ok_or("surface not found")?).and_then(|surface| surface.agent.as_ref()).map(|agent| agent.state).unwrap_or_default()
                    } else {
                        self.apply_agent_event(terminal, event)
                    };
                    if let Some(hook) = claude_hook.as_ref() {
                        self.record_claude_hook(terminal, hook)?;
                    }
                    if let Some(session_id) = codex_session_end.as_deref() {
                        self.end_codex_subagent_session(terminal, session_id)?;
                    }
                    self.rebuild_scene();
                    Ok(json!({
                        "state": state,
                        "claude_subagents": self.claude_subagents.len(),
                        "codex_subagents": self.codex_subagents.len()
                    }))
                })
            }
            "pane.focus" => request
                .params
                .get("id")
                .and_then(Value::as_str)
                .and_then(|v| uuid::Uuid::parse_str(v).ok())
                .ok_or_else(|| "invalid params.id".to_owned())
                .and_then(|pane| {
                    self.state
                        .execute(AppCommand::Pane(PaneCommand::Focus { id: pane }))
                        .map_err(|e| e.to_string())
                        .map(|_| {
                            self.rebuild_scene();
                            json!({"ok": true})
                        })
                }),
            _ => Err(format!("unknown method: {}", request.method)),
        }
        })();
        match result {
            Ok(value) => Response::success(id, value),
            Err(error) => Response::error(id, -32601, error),
        }
    }
}
