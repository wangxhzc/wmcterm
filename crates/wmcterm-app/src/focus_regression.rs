use super::*;

// Reuse the isolated application test's event loop, as winit permits only one.
pub(super) fn check(app: &mut WmctermApplication) {
    app.window_focused = true;
    app.focus_input_quarantine = false;
    app.workspace_search_focused = false;
    app.prompt = None;
    app.palette_visible = false;
    app.notifications_visible = false;
    app.agent_request_detail = None;
    app.execute_action("terminal.find");
    app.commit_ime_text("query".into());
    assert_eq!(app.search_editor.text(), "query");
    assert!(!app.terminal_accepts_keyboard());

    let (_, rect) = app.pane_rects()[0];
    app.cursor_position = (rect.x + 30.0, rect.y + 60.0);
    app.begin_mouse_action();
    assert!(app.search_visible);
    assert!(!app.search_focused);
    assert!(app.terminal_accepts_keyboard());
    assert_eq!(app.input_focus_terminal, app.active_terminal_id());
    app.mouse_pressed = false;
    app.selection = None;

    let width = 430.0_f32
        .min(app.window_size.width as f32 - 32.0)
        .max(220.0);
    let left = (app.window_size.width as f32 - width - 16.0).max(16.0);
    app.cursor_position = (left + 40.0, 20.0);
    app.begin_mouse_action();
    assert!(app.search_focused);
    assert_eq!(app.input_focus_terminal, None);
    app.mouse_pressed = false;
    app.text_edit_drag = None;

    app.sidebar_context_menu = Some(SidebarContextMenu { anchor: (0.0, 0.0) });
    let query = app.search_editor.text().to_owned();
    app.commit_ime_text("blocked".into());
    app.update_ime_preedit("blocked");
    assert_eq!(app.search_editor.text(), query);
    assert!(!app.search_editor.has_preedit());
    app.sidebar_context_menu = None;

    // A notification panel must not steal ownership from an explicit editor.
    app.notifications_visible = true;
    app.search_editor = TextEditor::default();
    app.commit_ime_text("editor".into());
    assert_eq!(app.search_editor.text(), "editor");
    app.search_visible = false;
    app.search_focused = false;
    app.rebuild_scene();
    assert!(!app.terminal_accepts_keyboard());
    assert_eq!(app.input_focus_terminal, None);
    app.notifications_visible = false;
    app.rebuild_scene();
    assert_eq!(app.input_focus_terminal, app.active_terminal_id());

    #[cfg(unix)]
    check_pty_input(app);
    check_background_tab_scroll(app);
    #[cfg(unix)]
    check_file_drop(app);
}

#[cfg(unix)]
fn check_file_drop(app: &mut WmctermApplication) {
    let original_pane = app.state.active_workspace().unwrap().active_pane;
    let original_terminal = app.active_terminal_id();
    let created = app
        .rpc_split_pane(
            app.state.active_workspace,
            original_pane,
            SplitPlacement {
                axis: SplitAxis::Vertical,
                new_first: false,
            },
            TerminalCreateOptions {
                command: Some("cat".into()),
                ..Default::default()
            },
            false,
        )
        .unwrap();
    let pane: PaneId = serde_json::from_value(created["pane_id"].clone()).unwrap();
    let terminal: TerminalId = serde_json::from_value(created["terminal_id"].clone()).unwrap();
    let (_, rect) = app
        .pane_rects()
        .into_iter()
        .find(|(id, _)| *id == pane)
        .unwrap();
    app.cursor_position = (rect.x + 40.0, rect.y + 70.0);

    app.sidebar_context_menu = Some(SidebarContextMenu { anchor: (0.0, 0.0) });
    app.drop_file_into_terminal(Path::new("/tmp/BLOCKED_DROP"));
    assert_eq!(app.active_terminal_id(), original_terminal);
    app.sidebar_context_menu = None;
    app.cursor_position = (10.0, 10.0);
    app.drop_file_into_terminal(Path::new("/tmp/OUTSIDE_DROP"));
    assert_eq!(app.active_terminal_id(), original_terminal);

    app.cursor_position = (rect.x + 40.0, rect.y + 70.0);
    app.search_visible = true;
    app.search_focused = true;
    app.workspace_search_focused = true;
    app.search_editor.set_preedit("search");
    app.workspace_search.set_preedit("workspace");
    app.focus_input_quarantine = true;
    app.notifications_visible = true;
    app.rebuild_scene();
    let path = Path::new("/tmp/\u{4e2d}\u{6587} file's.txt");
    assert!(app.pointer_over_overlay());
    app.drop_file_into_terminal(Path::new("/tmp/BLOCKED_DROP"));
    assert_eq!(app.active_terminal_id(), original_terminal);
    app.notifications_visible = false;
    app.drop_file_into_terminal(path);
    assert_eq!(app.active_terminal_id(), Some(terminal));
    assert_eq!(app.input_focus_terminal, Some(terminal));
    assert!(!app.search_focused);
    assert!(app.search_visible);
    assert!(!app.workspace_search_focused);
    assert!(!app.search_editor.has_preedit());
    assert!(!app.workspace_search.has_preedit());
    assert!(!app.notifications_visible);
    assert!(!app.focus_input_quarantine);

    app.commit_ime_text("-drop-finished\n".into());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let history = app.terminals[&terminal].session.export_history().unwrap();
        if history.contains("drop-finished") {
            assert!(
                history.contains(&format!("{}-drop-finished", shell_quote_path(path))),
                "{history}"
            );
            assert!(!history.contains("BLOCKED_DROP"), "{history}");
            assert!(!history.contains("OUTSIDE_DROP"), "{history}");
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "file drop PTY input timed out"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(app.remove_pane_by_id(pane));
    app.search_visible = false;
    app.rebuild_scene();
}

#[cfg(unix)]
fn check_pty_input(app: &mut WmctermApplication) {
    let surface = app.new_surface_command(Some("cat".into())).unwrap();
    let terminal = app.active_terminal_id().unwrap();
    let overlays: [fn(&mut WmctermApplication); 8] = [
        |app| app.notifications_visible = true,
        |app| app.agent_request_detail = Some(("focus-test".into(), 0)),
        |app| app.sidebar_context_menu = Some(SidebarContextMenu { anchor: (0.0, 0.0) }),
        |app| {
            app.workspace_context_menu = Some(WorkspaceContextMenu {
                workspace: app.state.active_workspace().unwrap().id,
                anchor: (0.0, 0.0),
            })
        },
        |app| {
            app.notification_context_menu = Some(NotificationContextMenu {
                notification: uuid::Uuid::new_v4(),
                anchor: (0.0, 0.0),
            })
        },
        |app| {
            app.tab_overflow_menu = Some(TabOverflowMenu {
                pane: app.state.active_workspace().unwrap().active_pane,
                anchor: (0.0, 0.0),
                scroll_offset: 0,
            })
        },
        |app| {
            app.tab_context_menu = Some(TabContextMenu {
                pane: app.state.active_workspace().unwrap().active_pane,
                surface: app.active_surface_id().unwrap(),
                anchor: (0.0, 0.0),
            })
        },
        |app| {
            app.terminal_context_menu = Some(TerminalContextMenu {
                pane: app.state.active_workspace().unwrap().active_pane,
                terminal: app.active_terminal_id().unwrap(),
                anchor: (0.0, 0.0),
            })
        },
    ];
    for show in overlays {
        show(app);
        app.rebuild_scene();
        assert_eq!(app.input_focus_terminal, None);
        app.commit_ime_text("LEAKED_INPUT\n".into());
        app.notifications_visible = false;
        app.agent_request_detail = None;
        app.sidebar_context_menu = None;
        app.workspace_context_menu = None;
        app.notification_context_menu = None;
        app.tab_overflow_menu = None;
        app.tab_context_menu = None;
        app.terminal_context_menu = None;
    }
    app.rebuild_scene();
    app.commit_ime_text("\u{4e2d}\u{6587}-focus-finished\n".into());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let history = app.terminals[&terminal].session.export_history().unwrap();
        if history.contains("focus-finished") {
            assert!(!history.contains("LEAKED_INPUT"), "{history}");
            assert!(history.contains("\u{4e2d}\u{6587}"));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "PTY input timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(app.remove_surface_by_id(surface));
}

fn check_background_tab_scroll(app: &mut WmctermApplication) {
    let workspace = app.state.active_workspace().unwrap();
    let original_pane = workspace.active_pane;
    let original_terminal = app.active_terminal_id();
    let background_pane = PaneId::new_v4();
    let first = wmcterm_core::Surface::terminal("first");
    let second = wmcterm_core::Surface::terminal("second");
    let first_id = first.id;
    let second_id = second.id;
    app.state.surfaces.insert(first_id, first);
    app.state.surfaces.insert(second_id, second);
    app.state.panes.insert(
        background_pane,
        Pane {
            id: background_pane,
            surfaces: vec![first_id, second_id],
            active_surface: 0,
            zoomed: false,
        },
    );
    app.state.workspaces[app.state.active_workspace]
        .root
        .split(original_pane, background_pane, SplitAxis::Horizontal, 0.5)
        .unwrap();
    let (_, rect) = app
        .pane_rects()
        .into_iter()
        .find(|(pane, _)| *pane == background_pane)
        .unwrap();
    app.cursor_position = (rect.x + 10.0, rect.y + 10.0);
    assert!(app.scroll_tabs_at_cursor(1));
    assert_eq!(app.state.panes[&background_pane].active_surface, 1);
    assert_eq!(
        app.state.active_workspace().unwrap().active_pane,
        original_pane
    );
    assert_eq!(app.input_focus_terminal, original_terminal);
    app.state.workspaces[app.state.active_workspace].root = app.state.workspaces
        [app.state.active_workspace]
        .root
        .clone()
        .remove(background_pane)
        .unwrap();
    app.state.panes.remove(&background_pane);
    app.state.surfaces.remove(&first_id);
    app.state.surfaces.remove(&second_id);
}
