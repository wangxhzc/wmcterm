mod i18n;
mod system_proxy;
mod text_edit;
mod ui;

use std::collections::{HashMap, HashSet, VecDeque};
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use i18n::Language;
use serde_json::{Value, json};
use text_edit::{TextEditor, TextViewport};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalPosition, LogicalSize, PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, Ime, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::keyboard::{Key, KeyCode, ModifiersState, NamedKey, PhysicalKey};
#[cfg(target_os = "linux")]
use winit::platform::startup_notify::{
    EventLoopExtStartupNotify, WindowAttributesExtStartupNotify, reset_activation_token_env,
};
use winit::window::{CursorIcon, Icon, ResizeDirection, WindowId};
use wmcterm_agent::{
    AgentEvent, AgentKind, AgentProcessIdentity, ClaudeHookEvent, ClaudeHookKind,
    CodexSubagentHookEvent, CodexSubagentHookKind, reduce as reduce_agent,
    validate_agent_process_identity,
};
use wmcterm_config::{
    BellNotificationMode, Config, ConfigReloader, ResolvedKeybindings, Shortcut, ThemeColor,
    ThemeConfig,
};
use wmcterm_core::{
    Action, ActionRegistry, AgentActionRequest, AgentKindId, AgentLaunchInfo, AgentLifecycleEvent,
    AgentLifecyclePhase, AgentRequestKind, AgentRunMode, AgentSession, AgentSessionEvent,
    AgentSessionEventKind, AgentTelemetry, AppCommand, AppState, AttentionState, Notification,
    Pane, PaneCommand, PaneId, PaneNode, SplitAxis, SurfaceId, SurfaceKind, TerminalId,
    WorkspaceCommand, WorkspaceId,
};
use wmcterm_ipc::{
    AgentInteractionRequest, AgentRequestKind as IpcAgentRequestKind, AgentResolution,
    CompleteOutcome, PendingAgentRequests, Request, Response, UnixServer, default_socket_path,
};
use wmcterm_pty::CommandSpec;
use wmcterm_renderer::{GpuRenderer, RectBlock, TextBlock, TextSpan, default_window_attributes};
use wmcterm_session::{
    AgentResumeInfo, SESSION_VERSION, Session, TerminalResume, WindowState, load as load_session,
    save as save_session, state_path,
};
use wmcterm_surface::{
    LogFilter, LogLevel, LogSurface, MarkdownBlock, MarkdownDocument, MarkdownSurface,
    SurfaceBackend,
};
use wmcterm_terminal::{
    CellSnapshot, ClipboardTarget, InputKey, KeyInput, KeyModifiers, MouseActionInput,
    MouseButtonInput, MouseInput, Rgb, ScreenSnapshot, SearchResult, SelectionPoint,
    SessionCommand, SessionEvent, TerminalSession, TerminalSize, TerminalTheme,
};

const TERMINAL_TERM: &str = "xterm-256color";
const TERMINAL_COLORTERM: &str = "truecolor";
const TERMINAL_PROGRAM: &str = "wmcterm";
const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_nanos(16_666_667);
const MAX_FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);
const CURSOR_BLINK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
const SYSTEM_PROXY_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const SYSTEM_PROXY_SPAWN_REFRESH_TIMEOUT: std::time::Duration =
    std::time::Duration::from_millis(500);
const SYSTEM_PROXY_SPAWN_COALESCE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

struct SystemProxyRefresh {
    completed: SyncSender<()>,
}

struct SystemProxyCache {
    current: Arc<Mutex<system_proxy::SystemProxy>>,
    last_attempt: Arc<Mutex<std::time::Instant>>,
    refresh: SyncSender<SystemProxyRefresh>,
}

impl SystemProxyCache {
    fn new() -> Self {
        // Initial discovery happens before the window/event loop starts. Subsequent
        // potentially blocking desktop queries are confined to this worker.
        let initial = system_proxy::detect().unwrap_or_default();
        Self::with_detector(initial, system_proxy::detect)
    }

    fn with_detector(
        initial: system_proxy::SystemProxy,
        mut detect: impl FnMut() -> Result<system_proxy::SystemProxy, ()> + Send + 'static,
    ) -> Self {
        let current = Arc::new(Mutex::new(initial));
        let last_attempt = Arc::new(Mutex::new(std::time::Instant::now()));
        let (refresh, refresh_rx) = mpsc::sync_channel::<SystemProxyRefresh>(1);
        let worker_current = Arc::clone(&current);
        let worker_last_attempt = Arc::clone(&last_attempt);
        thread::Builder::new()
            .name("wmcterm-system-proxy".into())
            .spawn(move || {
                loop {
                    match refresh_rx.recv_timeout(SYSTEM_PROXY_REFRESH_INTERVAL) {
                        Ok(refresh) => {
                            if let Ok(detected) = detect() {
                                *worker_current
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner()) = detected;
                            }
                            *worker_last_attempt
                                .lock()
                                .unwrap_or_else(|error| error.into_inner()) =
                                std::time::Instant::now();
                            let _ = refresh.completed.try_send(());
                        }
                        Err(mpsc::RecvTimeoutError::Timeout) => {
                            if let Ok(detected) = detect() {
                                *worker_current
                                    .lock()
                                    .unwrap_or_else(|error| error.into_inner()) = detected;
                            }
                            *worker_last_attempt
                                .lock()
                                .unwrap_or_else(|error| error.into_inner()) =
                                std::time::Instant::now();
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .expect("failed to spawn system proxy refresh worker");
        Self {
            current,
            last_attempt,
            refresh,
        }
    }

    fn environment_for_child(
        &self,
        explicit: &[(String, String)],
    ) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
        let explicit = explicit.iter().cloned().collect::<HashMap<_, _>>();
        let snapshot_is_fresh = self
            .last_attempt
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .elapsed()
            < SYSTEM_PROXY_SPAWN_COALESCE_INTERVAL;
        if !snapshot_is_fresh {
            let (completed, completed_rx) = mpsc::sync_channel(1);
            if self
                .refresh
                .try_send(SystemProxyRefresh { completed })
                .is_ok()
            {
                let _ = completed_rx.recv_timeout(SYSTEM_PROXY_SPAWN_REFRESH_TIMEOUT);
            }
        }
        let current = self
            .current
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        system_proxy::environment_for_child(&current, &explicit)
    }
}

fn terminal_text_cell_width(text: &str) -> usize {
    UnicodeWidthStr::width(text).max(1)
}

fn needs_grid_positioned_glyph(text: &str) -> bool {
    UnicodeWidthStr::width(text) > 1
}

fn wide_cell_lead(row: &[CellSnapshot], column: usize) -> usize {
    if row.get(column).is_some_and(|cell| cell.text.is_empty())
        && column > 0
        && row
            .get(column - 1)
            .is_some_and(|cell| UnicodeWidthStr::width(cell.text.as_str()) > 1)
    {
        column - 1
    } else {
        column
    }
}

fn wide_cell_end(row: &[CellSnapshot], column: usize) -> usize {
    column
        .saturating_add(
            row.get(column)
                .map(|cell| terminal_text_cell_width(cell.text.as_str()).saturating_sub(1))
                .unwrap_or(0),
        )
        .min(row.len().saturating_sub(1))
}

fn wide_glyph_optical_offset(text: &str, cell_width: f32, font_size: f32) -> (f32, f32) {
    let cells = terminal_text_cell_width(text);
    if cells <= 1 {
        return (0.0, 0.0);
    }
    let centered = (cells as f32 * cell_width - font_size).max(0.0) * 0.5;
    // CJK fallback glyphs have asymmetric visible side/top bearings even though
    // their em box is centered. Scale the correction with the configured size.
    ((centered - font_size * 0.05).max(0.0), font_size * 0.025)
}

fn windows_paths_equal(left: &Path, right: &Path) -> bool {
    let normalize = |path: &Path| {
        path.to_string_lossy()
            .replace('/', "\\")
            .trim_end_matches('\\')
            .to_lowercase()
    };
    normalize(left) == normalize(right)
}

#[cfg(any(windows, test))]
fn safe_windows_startup_cwd(
    process_cwd: PathBuf,
    executable_dir: Option<&Path>,
    user_profile: Option<&Path>,
) -> PathBuf {
    if executable_dir.is_some_and(|dir| windows_paths_equal(&process_cwd, dir))
        && let Some(profile) = user_profile
    {
        return profile.to_path_buf();
    }
    process_cwd
}

fn safe_process_cwd(process_cwd: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        let executable = env::current_exe().ok();
        let executable_dir = executable.as_deref().and_then(Path::parent);
        let user_profile = env::var_os("USERPROFILE").map(PathBuf::from);
        return safe_windows_startup_cwd(
            process_cwd,
            executable_dir,
            user_profile.as_deref().filter(|path| path.is_dir()),
        );
    }
    #[cfg(not(windows))]
    {
        process_cwd
    }
}

fn terminal_context_ids(
    state: &AppState,
    terminal: TerminalId,
) -> Option<(WorkspaceId, PaneId, SurfaceId)> {
    let surface = state
        .surfaces
        .values()
        .find(|surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal))?
        .id;
    let pane = state
        .panes
        .values()
        .find(|pane| pane.surfaces.contains(&surface))?
        .id;
    let workspace = state
        .workspaces
        .iter()
        .find(|workspace| workspace.root.contains(pane))?
        .id;
    Some((workspace, pane, surface))
}

#[derive(Debug)]
enum UserEvent {
    TerminalWake,
    Rpc(Request, SyncSender<Response>),
    WorkspaceMetadata(Vec<WorkspaceMetadata>),
}

const UNMODIFIED_TAB_FOCUS_GRACE: std::time::Duration = std::time::Duration::from_millis(120);
const FOCUS_INPUT_QUARANTINE_GRACE: std::time::Duration = std::time::Duration::from_millis(120);

#[derive(Debug)]
struct WorkspaceMetadata {
    id: wmcterm_core::WorkspaceId,
    git_branch: Option<String>,
    listening_ports: Vec<u16>,
}

#[derive(Debug, Clone)]
struct ClaudeSubagentStatus {
    surface: SurfaceId,
    session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CodexSubagentKey {
    surface: SurfaceId,
    session_id: String,
    agent_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexSubagentState {
    Active,
    Stopped,
}

#[derive(Debug, Default)]
struct CodexSubagentTracker {
    states: HashMap<CodexSubagentKey, CodexSubagentState>,
    stopped_order: HashMap<(SurfaceId, String), VecDeque<String>>,
}

const MAX_CODEX_STOPPED_TOMBSTONES_PER_SESSION: usize = 256;

impl CodexSubagentTracker {
    fn len(&self) -> usize {
        self.states
            .values()
            .filter(|state| **state == CodexSubagentState::Active)
            .count()
    }

    #[cfg(test)]
    fn retained_len(&self) -> usize {
        self.states.len()
    }
}

#[derive(Debug)]
struct TaggedEvent {
    terminal: TerminalId,
    event: SessionEvent,
}

struct TerminalRuntime {
    session: TerminalSession,
    snapshot: ScreenSnapshot,
    snapshot_revision: u64,
    size: TerminalSize,
    cell_metrics_px: (u32, u32),
    lifecycle_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TerminalOpenTarget {
    Url(String),
    Path(PathBuf),
}

#[derive(Debug, Clone, Default)]
struct TerminalCreateOptions {
    command: Option<String>,
    cwd: Option<std::path::PathBuf>,
    environment: Vec<(String, String)>,
    title: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SplitPlacement {
    axis: SplitAxis,
    new_first: bool,
}

#[derive(Clone, Copy)]
struct Rect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[derive(Clone)]
struct SplitDrag {
    path: Vec<bool>,
    axis: SplitAxis,
    container: Rect,
    center: f32,
}

#[derive(Clone, Copy)]
struct ScrollbarDrag {
    pane: PaneId,
    terminal: TerminalId,
    grab_y: f32,
    last_offset: usize,
}

#[derive(Clone, Copy)]
struct AgentRequestDetailScrollbarDrag {
    grab_y: f32,
}

#[derive(Clone, Copy)]
struct WorkspaceDrag {
    workspace: WorkspaceId,
    source_index: usize,
    source_row: usize,
    target_index: usize,
    press_y: f32,
    grab_y: f32,
    active: bool,
}

#[derive(Clone, Copy)]
struct AgentRequestDetailLayout {
    panel: ui::Bounds,
    body: ui::Bounds,
    total_lines: usize,
    visible_lines: usize,
    max_scroll: usize,
    scrollbar: Option<(ui::Bounds, ui::Bounds)>,
}

#[derive(Clone, Copy)]
struct MouseSelection {
    pane: PaneId,
    terminal: TerminalId,
    /// Full-screen coordinates, including scrollback, rather than viewport rows.
    start: (usize, usize),
    end: (usize, usize),
}

#[derive(Clone, Copy)]
struct TerminalClick {
    at: std::time::Instant,
    pane: PaneId,
    terminal: TerminalId,
    cell: (usize, usize),
}

#[derive(Clone, Copy)]
enum PromptKind {
    RenameWorkspace,
    DescribeWorkspace,
    ChangeWorkspaceDirectory,
    FindDirectory,
    FilterLogs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ManagedAgent {
    Claude,
    Codex,
}

impl ManagedAgent {
    const fn name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    const fn full_access_command(self) -> &'static str {
        match self {
            Self::Claude => "claude --dangerously-skip-permissions",
            Self::Codex => "codex --dangerously-bypass-approvals-and-sandbox",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextEditShortcut {
    SelectAll,
    Copy,
    Cut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextEditTarget {
    WorkspaceSearch,
    Prompt,
    Palette,
    Search,
}

struct TextPrompt {
    kind: PromptKind,
    editor: TextEditor,
    error: Option<String>,
}

#[derive(Clone)]
enum CloseTarget {
    Surfaces(Vec<SurfaceId>),
    Workspace(uuid::Uuid),
    Application,
    FullAccess(ManagedAgent),
}

struct CloseConfirmation {
    target: CloseTarget,
    title: String,
    message: String,
}

struct CodexTrustConfirmation {
    project_root: std::path::PathBuf,
    error: Option<String>,
}

#[derive(Clone, Copy)]
struct TabContextMenu {
    pane: PaneId,
    surface: SurfaceId,
    anchor: (f32, f32),
}

#[derive(Clone, Copy)]
struct TabOverflowMenu {
    pane: PaneId,
    anchor: (f32, f32),
    scroll_offset: usize,
}

#[derive(Clone, Copy)]
struct WorkspaceContextMenu {
    workspace: uuid::Uuid,
    anchor: (f32, f32),
}

#[derive(Clone, Copy)]
struct SidebarContextMenu {
    anchor: (f32, f32),
}

#[derive(Clone, Copy)]
struct TerminalContextMenu {
    pane: PaneId,
    terminal: TerminalId,
    anchor: (f32, f32),
}

#[derive(Clone, Copy)]
struct NotificationContextMenu {
    notification: uuid::Uuid,
    anchor: (f32, f32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NavigationHover {
    ConfirmationCancel,
    ConfirmationConfirm,
    ConfirmationContinue,
    Workspace(usize),
    NewWorkspace,
    SidebarScrollbar,
    SidebarResize,
    PaneDivider(usize),
    Tab(PaneId, usize),
    TabClose(PaneId, usize),
    TabOverflow(PaneId),
    TabNew(PaneId),
    Notification(usize),
    NotificationClose(usize),
    NotificationMarkAllRead,
    AgentApproval(usize, usize),
    ContextAction(usize),
    TitlebarButton(usize),
    EmptyPaneNew(PaneId),
    EmptyWorkspaceNew,
}

#[derive(Clone, Copy)]
enum TitlebarAction {
    ToggleSidebar,
    ToggleNotifications,
    Minimize,
    ToggleMaximize,
    Close,
}

struct TitlebarButton {
    action: TitlebarAction,
    label: String,
    bounds: ui::Bounds,
    active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NotificationDestination {
    workspace_index: usize,
    pane: Option<(PaneId, usize)>,
    surface: SurfaceId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NotificationOverlayPress {
    Inactive,
    Inside,
    DismissAndPropagate,
}

#[derive(Clone, Copy)]
enum TabContextAction {
    Close,
    CloseOthers,
    NewSurface,
}

#[derive(Clone, Copy)]
enum WorkspaceContextAction {
    NewSurface,
    OpenFileManager,
    OpenTerminal,
    Rename,
    Describe,
    ChangeDirectory,
    Close,
}

#[derive(Clone, Copy)]
enum TerminalContextAction {
    Copy,
    Paste,
    CopyAll,
    ClearScrollback,
}

#[derive(Clone, Copy)]
enum NotificationContextAction {
    Open,
    ToggleRead,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentApprovalAction {
    Deny,
    AllowOnce,
    AlwaysAllow,
}

const TAB_CONTEXT_ACTIONS: [TabContextAction; 3] = [
    TabContextAction::Close,
    TabContextAction::CloseOthers,
    TabContextAction::NewSurface,
];
const TERMINAL_CONTEXT_ACTIONS: [TerminalContextAction; 4] = [
    TerminalContextAction::Copy,
    TerminalContextAction::Paste,
    TerminalContextAction::CopyAll,
    TerminalContextAction::ClearScrollback,
];
const NOTIFICATION_CONTEXT_ACTIONS: [NotificationContextAction; 3] = [
    NotificationContextAction::Open,
    NotificationContextAction::ToggleRead,
    NotificationContextAction::Clear,
];

const WORKSPACE_CONTEXT_ACTIONS: [WorkspaceContextAction; 7] = [
    WorkspaceContextAction::NewSurface,
    WorkspaceContextAction::OpenFileManager,
    WorkspaceContextAction::OpenTerminal,
    WorkspaceContextAction::Rename,
    WorkspaceContextAction::Describe,
    WorkspaceContextAction::ChangeDirectory,
    WorkspaceContextAction::Close,
];

#[derive(Debug, Clone)]
enum TerminalNotice {
    Exited(String),
    Warning(String),
    Error(String),
    SpawnFailed(String),
}

fn terminal_notice_attention(notice: &TerminalNotice) -> Option<AttentionState> {
    match notice {
        TerminalNotice::Exited(_) => Some(AttentionState::Activity),
        TerminalNotice::Warning(_) => None,
        TerminalNotice::Error(_) | TerminalNotice::SpawnFailed(_) => Some(AttentionState::Error),
    }
}

fn terminal_notice_is_error(notice: &TerminalNotice) -> bool {
    matches!(
        notice,
        TerminalNotice::Error(_) | TerminalNotice::SpawnFailed(_)
    )
}

fn clear_recoverable_terminal_notice(
    notices: &mut HashMap<TerminalId, TerminalNotice>,
    terminal: TerminalId,
) -> bool {
    if matches!(notices.get(&terminal), Some(TerminalNotice::Warning(_))) {
        notices.remove(&terminal);
        true
    } else {
        false
    }
}

pub fn run() -> Result<()> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let config = wmcterm_config::load_or_default().context("loading wmcterm configuration")?;
    let mut restored = state_path()
        .filter(|path| path.exists())
        .and_then(|path| load_session(&path).ok());
    if let Some(session) = &mut restored {
        discard_unavailable_codex_resumes(session);
    }
    let restored_commands = restored
        .as_ref()
        .map(|session| {
            session
                .terminals
                .iter()
                .filter_map(|terminal| {
                    restored_terminal_command(terminal)
                        .map(|command| (terminal.terminal_id, command))
                })
                .collect()
        })
        .unwrap_or_default();
    let restored_history = restored
        .as_ref()
        .map(|session| {
            session
                .terminals
                .iter()
                .filter_map(|terminal| {
                    terminal
                        .scrollback
                        .clone()
                        .map(|history| (terminal.terminal_id, history))
                })
                .collect()
        })
        .unwrap_or_default();
    let restored_window = restored.as_ref().and_then(|session| session.window);
    let restored_agent_sessions = restored
        .as_ref()
        .map(|session| {
            session
                .agent_sessions
                .iter()
                .cloned()
                .map(|session| (session.surface_id, session))
                .collect()
        })
        .unwrap_or_default();
    let restored_state = restored.map(|mut session| {
        reconcile_restored_agent_modes(&mut session.app, &session.terminals);
        session.app
    });
    let mut app = WmctermApplication::new(
        event_loop.create_proxy(),
        config,
        restored_state,
        restored_commands,
        restored_history,
        restored_window,
        restored_agent_sessions,
    )?;
    event_loop.run_app(&mut app)?;
    if let Some(path) = state_path() {
        let terminals = app
            .terminals
            .iter()
            .map(|(terminal_id, runtime)| {
                let resume = resume_info(
                    &app.state,
                    &app.agent_sessions,
                    &app.foreground_agents,
                    *terminal_id,
                );
                TerminalResume {
                    terminal_id: *terminal_id,
                    cwd: app
                        .cwd_for_terminal(*terminal_id)
                        .unwrap_or_else(|| env::current_dir().unwrap_or_default()),
                    command: match resume.as_ref() {
                        Some(resume) if resume.managed => None,
                        Some(resume) => Some(manual_agent_resume_command(&resume.agent.command())),
                        None => app
                            .commands
                            .get(terminal_id)
                            .filter(|command| !command.contains(MANUAL_AGENT_RESUME_MARKER))
                            .cloned(),
                    },
                    agent: resume.map(|resume| resume.agent),
                    scrollback: runtime.session.export_history(),
                }
            })
            .collect();
        let mut session = Session {
            version: SESSION_VERSION,
            app: app.state.clone(),
            terminals,
            agent_sessions: app.agent_sessions.into_values().collect(),
            window: Some(WindowState {
                width: app.window_size.width.max(600),
                height: app.window_size.height.max(360),
            }),
        };
        discard_unavailable_codex_resumes(&mut session);
        save_session(&path, &session).context("saving wmcterm session")?;
    }
    Ok(())
}

// Called only at save/load, never in terminal actors or the render loop.
fn codex_rollout_exists(home: &std::path::Path, session_id: &str) -> bool {
    if uuid::Uuid::parse_str(session_id).is_err() {
        return false;
    }
    let suffix = format!("-{session_id}.jsonl");
    let mut directories = vec![
        (home.join("sessions"), 0),
        (home.join("archived_sessions"), 0),
    ];
    while let Some((directory, depth)) = directories.pop() {
        let Ok(entries) = std::fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() && depth < 3 {
                directories.push((entry.path(), depth + 1));
            } else if kind.is_file() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("rollout-")
                    && name.ends_with(&suffix)
                    && entry.metadata().is_ok_and(|metadata| metadata.len() > 0)
                {
                    return true;
                }
            }
        }
    }
    false
}

fn discard_unavailable_codex_resumes(session: &mut Session) {
    let home = wmcterm_codex::user_config_path()
        .and_then(|path| path.parent().map(std::path::Path::to_owned));
    discard_codex_resumes_without_record(session, |id| {
        home.as_ref()
            .is_some_and(|home| codex_rollout_exists(home, id))
    });
}

fn discard_codex_resumes_without_record(session: &mut Session, exists: impl Fn(&str) -> bool) {
    let mut cleared = HashSet::new();
    for terminal in &mut session.terminals {
        let discard = match &terminal.agent {
            Some(agent @ AgentResumeInfo::Codex { session_id }) => {
                let generated = terminal.command.as_ref().is_none_or(|command| {
                    command == &agent.command()
                        || is_manual_agent_resume_command(command)
                        || is_stale_codex_teams_child_command(command, session_id)
                });
                generated && !exists(session_id)
            }
            None if terminal.command.is_none() => session.app.surfaces.values().any(|surface| {
                matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal.terminal_id)
                    && surface
                        .agent
                        .as_ref()
                        .is_some_and(|agent| agent.name.eq_ignore_ascii_case("codex"))
            }),
            _ => false,
        };
        if !discard {
            continue;
        }
        terminal.command = None;
        terminal.agent = None;
        for surface in session.app.surfaces.values_mut().filter(|surface| {
            matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal.terminal_id)
        }) {
            surface.agent = None;
            surface.attention = AttentionState::None;
            cleared.insert(surface.id);
        }
    }
    session
        .agent_sessions
        .retain(|agent| !cleared.contains(&agent.surface_id));
    for workspace in &mut session.app.workspaces {
        let mut panes = Vec::new();
        workspace.root.leaves(&mut panes);
        let surfaces: Vec<_> = panes
            .iter()
            .filter_map(|id| session.app.panes.get(id))
            .flat_map(|pane| &pane.surfaces)
            .filter_map(|id| session.app.surfaces.get(id))
            .collect();
        if surfaces.iter().any(|surface| cleared.contains(&surface.id))
            && surfaces
                .iter()
                .all(|surface| surface.agent.is_none() && surface.attention == AttentionState::None)
        {
            workspace.status = wmcterm_core::WorkspaceStatus::Idle;
        }
    }
}

fn reconcile_restored_agent_modes(state: &mut AppState, terminals: &[TerminalResume]) {
    for terminal in terminals {
        let Some(agent) = &terminal.agent else {
            continue;
        };
        let Some(command) = restored_terminal_command(terminal) else {
            continue;
        };
        // These generated resume commands intentionally omit elevated modes.
        // Preserve metadata for explicit launch commands that are replayed as-is.
        if command != agent.command() && !is_manual_agent_resume_command(&command) {
            continue;
        }
        for surface in state.surfaces.values_mut().filter(|surface| {
            matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal.terminal_id)
        }) {
            if let Some(metadata) = &mut surface.agent {
                metadata.run_mode = AgentRunMode::Interactive;
            }
        }
    }
}

fn sync_detected_agent_mode(
    metadata: &mut wmcterm_core::AgentMetadata,
    agent: AgentKind,
    run_mode: AgentRunMode,
) -> bool {
    if AgentKind::from_process_command(&metadata.name) != Some(agent)
        || metadata.run_mode == run_mode
    {
        return false;
    }
    metadata.run_mode = run_mode;
    true
}

fn restored_terminal_command(terminal: &TerminalResume) -> Option<String> {
    match (&terminal.command, &terminal.agent) {
        (Some(command), Some(agent @ AgentResumeInfo::Codex { session_id }))
            if is_stale_codex_teams_child_command(command, session_id) =>
        {
            Some(agent.command())
        }
        (Some(command), Some(agent)) if *command == agent.command() => {
            Some(manual_agent_resume_command(command))
        }
        (Some(command), _) => Some(command.clone()),
        (None, Some(agent)) => Some(agent.command()),
        (None, None) => None,
    }
}

/// `codex-teams` child panes attach to a private app-server on a loopback port.
/// The server and port die with the owning wmcterm process, so replaying this
/// expanded command after session restore can only fail. Match the exact tail
/// emitted by `wmcterm-codex` and require its thread ID to agree with the Agent
/// metadata before degrading it to a standalone `codex resume` command.
fn is_stale_codex_teams_child_command(command: &str, session_id: &str) -> bool {
    let mut parts = command.split_whitespace().rev();
    let Some(thread_id) = parts.next() else {
        return false;
    };
    let Some(endpoint) = parts.next() else {
        return false;
    };
    let Some(remote) = parts.next() else {
        return false;
    };
    let Some(resume) = parts.next() else {
        return false;
    };
    let endpoint = endpoint.trim_end_matches('/');
    let Some(port) = endpoint.strip_prefix("ws://127.0.0.1:") else {
        return false;
    };
    thread_id == session_id
        && remote == "--remote"
        && resume == "resume"
        && port.parse::<u16>().is_ok_and(|port| port != 0)
}

struct WmctermApplication {
    renderer: Option<GpuRenderer>,
    state: AppState,
    terminals: HashMap<TerminalId, TerminalRuntime>,
    pending: Arc<Mutex<VecDeque<TaggedEvent>>>,
    wake_pending: Arc<AtomicBool>,
    proxy: EventLoopProxy<UserEvent>,
    modifiers: ModifiersState,
    physical_alt_pressed: bool,
    pending_unmodified_tab: Option<(KeyEvent, std::time::Instant)>,
    focus_input_quarantine: bool,
    palette_visible: bool,
    palette_index: usize,
    palette_scroll: usize,
    palette_query: TextEditor,
    notifications_visible: bool,
    notification_scroll: usize,
    notification_selection: usize,
    agent_request_scroll: usize,
    agent_request_selection: usize,
    agent_request_action: usize,
    agent_request_detail: Option<(String, usize)>,
    agent_request_detail_scrollbar_drag: Option<AgentRequestDetailScrollbarDrag>,
    sidebar_visible: bool,
    sidebar_scroll: usize,
    workspace_search: TextEditor,
    workspace_search_focused: bool,
    /// Logical UI size. The renderer converts the scene to physical pixels.
    window_size: PhysicalSize<u32>,
    scale_factor: f64,
    _ipc: Option<UnixServer>,
    control_socket_path: Option<std::path::PathBuf>,
    config: Config,
    system_proxy: SystemProxyCache,
    cursor_position: (f32, f32),
    commands: HashMap<TerminalId, String>,
    clipboard: Option<arboard::Clipboard>,
    quit_requested: bool,
    selection: Option<MouseSelection>,
    last_terminal_click: Option<TerminalClick>,
    selection_last_redraw: Option<std::time::Instant>,
    selection_autoscroll_last: Option<std::time::Instant>,
    split_drag: Option<SplitDrag>,
    sidebar_drag: bool,
    sidebar_scrollbar_drag: Option<f32>,
    workspace_drag: Option<WorkspaceDrag>,
    scrollbar_drag: Option<ScrollbarDrag>,
    mouse_pressed: bool,
    text_edit_drag: Option<TextEditTarget>,
    terminal_mouse_capture: Option<(PaneId, TerminalId)>,
    search_visible: bool,
    search_editor: TextEditor,
    search_result: Option<SearchResult>,
    terminal_font_size: f32,
    prompt: Option<TextPrompt>,
    close_confirmation: Option<CloseConfirmation>,
    codex_trust_confirmation: Option<CodexTrustConfirmation>,
    tab_context_menu: Option<TabContextMenu>,
    tab_overflow_menu: Option<TabOverflowMenu>,
    workspace_context_menu: Option<WorkspaceContextMenu>,
    sidebar_context_menu: Option<SidebarContextMenu>,
    terminal_context_menu: Option<TerminalContextMenu>,
    notification_context_menu: Option<NotificationContextMenu>,
    restored_history: HashMap<TerminalId, String>,
    keybindings: ResolvedKeybindings,
    surface_backends: HashMap<SurfaceId, Box<dyn SurfaceBackend>>,
    config_reloader: Option<ConfigReloader>,
    last_metadata_refresh: std::time::Instant,
    language: Language,
    palette_hover: Option<usize>,
    navigation_hover: Option<NavigationHover>,
    notification_hover_started: Option<(usize, std::time::Instant)>,
    notification_marquee_line: usize,
    last_titlebar_click: Option<(std::time::Instant, (f32, f32))>,
    document_scroll: HashMap<SurfaceId, usize>,
    metadata_refresh_in_flight: bool,
    terminal_notices: HashMap<TerminalId, TerminalNotice>,
    terminal_processes: HashMap<TerminalId, Vec<String>>,
    foreground_agents: HashMap<TerminalId, (AgentKind, AgentRunMode)>,
    automatic_agent_status: HashSet<TerminalId>,
    foreground_agent_input: HashMap<TerminalId, String>,
    scene_dirty: bool,
    last_scene_rebuild: Option<std::time::Instant>,
    frame_work_estimate: std::time::Duration,
    pending_scene_build_duration: std::time::Duration,
    cursor_blink_visible: bool,
    cursor_blink_last_toggle: std::time::Instant,
    pixel_scroll_remainder: f64,
    window_focused: bool,
    window_focus_gained_at: std::time::Instant,
    last_bell_notifications: HashMap<TerminalId, std::time::Instant>,
    agent_sessions: HashMap<SurfaceId, AgentSession>,
    pending_agent_requests: PendingAgentRequests,
    agent_approval_results: HashMap<String, AgentApprovalAction>,
    agent_approval_result_order: VecDeque<String>,
    claude_subagents: HashMap<String, ClaudeSubagentStatus>,
    codex_subagents: CodexSubagentTracker,
}

impl WmctermApplication {
    fn new(
        proxy: EventLoopProxy<UserEvent>,
        config: Config,
        restored: Option<AppState>,
        commands: HashMap<TerminalId, String>,
        restored_history: HashMap<TerminalId, String>,
        restored_window: Option<WindowState>,
        restored_agent_sessions: HashMap<SurfaceId, AgentSession>,
    ) -> Result<Self> {
        Self::new_configured(
            proxy,
            config,
            restored,
            commands,
            restored_history,
            restored_window,
            restored_agent_sessions,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn new_configured(
        proxy: EventLoopProxy<UserEvent>,
        config: Config,
        restored: Option<AppState>,
        mut commands: HashMap<TerminalId, String>,
        restored_history: HashMap<TerminalId, String>,
        restored_window: Option<WindowState>,
        restored_agent_sessions: HashMap<SurfaceId, AgentSession>,
        enable_ipc: bool,
    ) -> Result<Self> {
        let raw_process_cwd = env::current_dir()?;
        let process_cwd = safe_process_cwd(raw_process_cwd.clone());
        let cwd = config
            .workspace
            .default_cwd
            .clone()
            .unwrap_or_else(|| process_cwd.clone());
        let name = cwd
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("wmcterm")
            .to_owned();
        let mut state = restored.unwrap_or_else(|| AppState::new(name, cwd.clone()));
        for workspace in &mut state.workspaces {
            workspace.cwd =
                wmcterm_config::normalize_windows_path(std::mem::take(&mut workspace.cwd));
        }
        if config.workspace.default_cwd.is_none() && process_cwd != raw_process_cwd {
            for workspace in &mut state.workspaces {
                if windows_paths_equal(&workspace.cwd, &raw_process_cwd) {
                    workspace.cwd = process_cwd.clone();
                }
            }
        }
        if commands.is_empty()
            && let Some(project) = wmcterm_config::load_project(&cwd)?
            && let Some(workspace) = state.workspaces.first_mut()
        {
            if let Some(name) = project.workspace.name {
                workspace.name = name;
            }
            let pane_id = workspace.active_pane;
            for item in project.commands {
                let surface = wmcterm_core::Surface::terminal(item.name);
                let terminal = match surface.kind {
                    SurfaceKind::Terminal(id) => id,
                    _ => continue,
                };
                commands.insert(terminal, item.command);
                let surface_id = surface.id;
                state.surfaces.insert(surface_id, surface);
                if let Some(pane) = state.panes.get_mut(&pane_id) {
                    pane.surfaces.push(surface_id);
                }
            }
        }
        let pending = Arc::new(Mutex::new(VecDeque::new()));
        let wake_pending = Arc::new(AtomicBool::new(false));
        let control_socket_path = enable_ipc.then(application_socket_path).flatten();
        let ipc = control_socket_path
            .as_ref()
            .map(|path| {
                let rpc_proxy = proxy.clone();
                UnixServer::spawn(path, move |request| {
                    let id = request.id.clone();
                    let (tx, rx) = mpsc::sync_channel(1);
                    if rpc_proxy.send_event(UserEvent::Rpc(request, tx)).is_err() {
                        return Response::error(id, -32000, "wmcterm event loop is closed");
                    }
                    rx.recv_timeout(std::time::Duration::from_secs(3))
                        .unwrap_or_else(|_| Response::error(id, -32001, "wmcterm RPC timed out"))
                })
            })
            .transpose()?;
        let terminal_font_size = config.terminal.font_size.clamp(8.0, 40.0);
        let keybindings = config
            .resolved_keybindings()
            .map_err(|error| anyhow::anyhow!("invalid keybindings: {error}"))?;
        let config_reloader =
            wmcterm_config::config_path().map(|path| ConfigReloader::new(vec![path]));
        let language = Language::resolve(config.ui.language);
        let mut app = Self {
            renderer: None,
            state,
            terminals: HashMap::new(),
            pending,
            wake_pending,
            proxy,
            modifiers: ModifiersState::empty(),
            physical_alt_pressed: false,
            pending_unmodified_tab: None,
            focus_input_quarantine: false,
            palette_visible: false,
            palette_index: 0,
            palette_scroll: 0,
            palette_query: TextEditor::default(),
            notifications_visible: false,
            notification_scroll: 0,
            notification_selection: 0,
            agent_request_scroll: 0,
            agent_request_selection: 0,
            agent_request_action: 0,
            agent_request_detail: None,
            agent_request_detail_scrollbar_drag: None,
            sidebar_visible: true,
            sidebar_scroll: 0,
            workspace_search: TextEditor::default(),
            workspace_search_focused: false,
            window_size: restored_window
                .map(|window| PhysicalSize::new(window.width.max(600), window.height.max(360)))
                .unwrap_or_else(|| PhysicalSize::new(1000, 650)),
            scale_factor: 1.0,
            _ipc: ipc,
            control_socket_path,
            config,
            system_proxy: SystemProxyCache::new(),
            cursor_position: (0.0, 0.0),
            commands,
            clipboard: arboard::Clipboard::new().ok(),
            quit_requested: false,
            selection: None,
            last_terminal_click: None,
            selection_last_redraw: None,
            selection_autoscroll_last: None,
            split_drag: None,
            sidebar_drag: false,
            sidebar_scrollbar_drag: None,
            workspace_drag: None,
            scrollbar_drag: None,
            mouse_pressed: false,
            text_edit_drag: None,
            terminal_mouse_capture: None,
            search_visible: false,
            search_editor: TextEditor::default(),
            search_result: None,
            terminal_font_size,
            prompt: None,
            close_confirmation: None,
            codex_trust_confirmation: None,
            tab_context_menu: None,
            tab_overflow_menu: None,
            workspace_context_menu: None,
            sidebar_context_menu: None,
            terminal_context_menu: None,
            notification_context_menu: None,
            restored_history,
            keybindings,
            surface_backends: HashMap::new(),
            config_reloader,
            last_metadata_refresh: std::time::Instant::now(),
            language,
            palette_hover: None,
            navigation_hover: None,
            notification_hover_started: None,
            notification_marquee_line: 0,
            last_titlebar_click: None,
            document_scroll: HashMap::new(),
            metadata_refresh_in_flight: false,
            terminal_notices: HashMap::new(),
            terminal_processes: HashMap::new(),
            foreground_agents: HashMap::new(),
            automatic_agent_status: HashSet::new(),
            foreground_agent_input: HashMap::new(),
            scene_dirty: true,
            last_scene_rebuild: None,
            frame_work_estimate: std::time::Duration::ZERO,
            pending_scene_build_duration: std::time::Duration::ZERO,
            cursor_blink_visible: true,
            cursor_blink_last_toggle: std::time::Instant::now(),
            pixel_scroll_remainder: 0.0,
            window_focused: true,
            window_focus_gained_at: std::time::Instant::now(),
            last_bell_notifications: HashMap::new(),
            agent_sessions: restored_agent_sessions,
            pending_agent_requests: PendingAgentRequests::new(256)
                .expect("pending Agent request capacity is non-zero"),
            agent_approval_results: HashMap::new(),
            agent_approval_result_order: VecDeque::new(),
            claude_subagents: HashMap::new(),
            codex_subagents: CodexSubagentTracker::default(),
        };
        for surface in app.state.surfaces.values() {
            let Some(path) = surface.resource.as_deref().map(std::path::Path::new) else {
                continue;
            };
            let backend: Option<Box<dyn SurfaceBackend>> = match surface.kind {
                SurfaceKind::Markdown(_) => MarkdownSurface::open(path)
                    .ok()
                    .map(|surface| Box::new(surface) as Box<dyn SurfaceBackend>),
                SurfaceKind::Logs(_) => LogSurface::open(path, 10_000)
                    .ok()
                    .map(|surface| Box::new(surface) as Box<dyn SurfaceBackend>),
                _ => None,
            };
            if let Some(backend) = backend {
                app.surface_backends.insert(surface.id, backend);
            }
        }
        app.refresh_workspace_metadata();
        let terminal_ids: Vec<_> = app
            .state
            .surfaces
            .values()
            .filter_map(|surface| {
                if let SurfaceKind::Terminal(id) = surface.kind {
                    Some(id)
                } else {
                    None
                }
            })
            .collect();
        for id in terminal_ids {
            if let Err(error) = app.spawn_terminal(id) {
                tracing::error!(terminal=%id, %error, "spawning restored terminal failed");
                app.report_terminal_notice(id, TerminalNotice::SpawnFailed(error.to_string()));
            }
        }
        Ok(app)
    }

    fn spawn_terminal(&mut self, id: TerminalId) -> Result<()> {
        self.spawn_terminal_command(id, self.commands.get(&id).cloned())
    }

    fn spawn_terminal_command(&mut self, id: TerminalId, command: Option<String>) -> Result<()> {
        self.spawn_terminal_with_options(
            id,
            TerminalCreateOptions {
                command,
                ..TerminalCreateOptions::default()
            },
        )
    }

    fn spawn_terminal_with_options(
        &mut self,
        id: TerminalId,
        options: TerminalCreateOptions,
    ) -> Result<()> {
        if let Some(surface) = self.state.surfaces.values_mut().find(
            |surface| matches!(surface.kind, SurfaceKind::Terminal(terminal) if terminal == id),
        ) && matches!(surface.title.as_str(), "Shell" | "终端")
        {
            surface.title = self.language.shell().into();
        }
        if let Some(title) = &options.title
            && let Some(surface) = self.state.surfaces.values_mut().find(
                |surface| matches!(surface.kind, SurfaceKind::Terminal(terminal) if terminal == id),
            )
        {
            surface.title.clone_from(title);
        }
        let size = TerminalSize::new(80, 24)?;
        let lifecycle_id = uuid::Uuid::new_v4().to_string();
        let mut spec = terminal_command_spec(options.command.as_deref())
            .cwd(
                options
                    .cwd
                    .clone()
                    .or_else(|| self.cwd_for_terminal(id))
                    .unwrap_or_else(|| env::current_dir().unwrap_or_default()),
            )
            .env("TERM", TERMINAL_TERM)
            .env("COLORTERM", TERMINAL_COLORTERM)
            .env("TERM_PROGRAM", TERMINAL_PROGRAM)
            .env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"))
            .env("WMCTERM", "1")
            .env("WMCTERM_TERMINAL_LIFECYCLE_ID", &lifecycle_id)
            .env("WMCTERM_LIFECYCLE_ID", &lifecycle_id)
            .env(
                "COLORFGBG",
                if terminal_background_is_light(self.config.theme.background) {
                    "0;15"
                } else {
                    "15;0"
                },
            );
        if let Some((workspace, pane, surface)) = terminal_context_ids(&self.state, id) {
            spec = spec
                .env("WMCTERM_WORKSPACE_ID", workspace.to_string())
                .env("WMCTERM_PANE_ID", pane.to_string())
                .env("WMCTERM_SURFACE_ID", surface.to_string());
        }
        if let Some(path) = &self.control_socket_path {
            spec = spec
                .env("WMCTERM_CONTROL_SOCKET", path.as_os_str())
                .env("WMCTERM_SOCKET", path.as_os_str());
        }
        for (key, value) in self
            .system_proxy
            .environment_for_child(&options.environment)
        {
            spec = spec.env(key, value);
        }
        for (key, value) in &options.environment {
            spec = spec.env(key, value);
        }
        let history = self.restored_history.remove(&id);
        let session = TerminalSession::spawn_with_scrollback_limits_history_and_theme(
            spec,
            size,
            self.config.terminal.scrollback,
            self.config.terminal.scrollback_lines,
            history,
            Some(terminal_theme(&self.config.theme)),
        )?;
        self.terminal_notices.remove(&id);
        self.terminal_processes.remove(&id);
        self.last_bell_notifications.remove(&id);
        self.foreground_agents.remove(&id);
        self.automatic_agent_status.remove(&id);
        spawn_event_bridge(
            id,
            session.events().clone(),
            Arc::clone(&self.pending),
            Arc::clone(&self.wake_pending),
            self.proxy.clone(),
        );
        self.terminals.insert(
            id,
            TerminalRuntime {
                session,
                snapshot: ScreenSnapshot {
                    cols: 80,
                    rows: 24,
                    lines: vec![],
                    cells: vec![],
                    cursor: None,
                    mouse_tracking: false,
                    scrollback_rows: 0,
                    viewport_offset: 0,
                    screen_row_origin: 0,
                },
                snapshot_revision: 0,
                size,
                cell_metrics_px: (0, 0),
                lifecycle_id,
            },
        );
        if let Some(command) = options.command {
            self.commands.insert(id, command);
        }
        Ok(())
    }

    fn active_terminal_id(&self) -> Option<TerminalId> {
        let workspace = self.state.active_workspace()?;
        let pane = self.state.panes.get(&workspace.active_pane)?;
        let surface = self.state.surfaces.get(&pane.active_surface()?)?;
        match surface.kind {
            SurfaceKind::Terminal(id) => Some(id),
            _ => None,
        }
    }

    fn active_surface_id(&self) -> Option<SurfaceId> {
        let workspace = self.state.active_workspace()?;
        self.state
            .panes
            .get(&workspace.active_pane)?
            .active_surface()
    }

    fn cwd_for_terminal(&self, terminal: TerminalId) -> Option<std::path::PathBuf> {
        let surface = self.surface_for_terminal(terminal)?;
        let pane = self
            .state
            .panes
            .values()
            .find(|pane| pane.surfaces.contains(&surface))?
            .id;
        self.state
            .workspaces
            .iter()
            .find(|workspace| workspace.root.contains(pane))
            .map(|workspace| workspace.cwd.clone())
    }

    fn surface_for_terminal(&self, terminal: TerminalId) -> Option<SurfaceId> {
        self.state
            .surfaces
            .values()
            .find(|surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal))?
            .id
            .into()
    }

    fn workspace_for_terminal(&self, terminal: TerminalId) -> Option<usize> {
        let surface = self.surface_for_terminal(terminal)?;
        let pane = self
            .state
            .panes
            .values()
            .find(|pane| pane.surfaces.contains(&surface))?
            .id;
        self.state
            .workspaces
            .iter()
            .position(|workspace| workspace.root.contains(pane))
    }

    fn terminal_for_params(&self, params: &Value) -> Result<TerminalId, String> {
        let (requested_terminal, requested_surface) = explicit_terminal_target(params)?;
        let surface_terminal = requested_surface
            .map(|id| {
                let surface = self
                    .state
                    .surfaces
                    .get(&id)
                    .ok_or_else(|| "target surface not found".to_owned())?;
                match surface.kind {
                    SurfaceKind::Terminal(terminal) => Ok(terminal),
                    _ => Err("target surface is not a terminal".to_owned()),
                }
            })
            .transpose()?;
        if let (Some(terminal), Some(surface_terminal)) = (requested_terminal, surface_terminal)
            && terminal != surface_terminal
        {
            return Err("target terminal does not match target surface".into());
        }
        let terminal = requested_terminal
            .or(surface_terminal)
            .ok_or_else(|| "no terminal target".to_owned())?;
        if !self.terminals.contains_key(&terminal) {
            return Err("target terminal is not running".into());
        }
        Ok(terminal)
    }

    fn codex_agent_update_route(&self, params: &Value) -> Result<(TerminalId, SurfaceId), String> {
        let surface = params
            .get("surface_id")
            .and_then(Value::as_str)
            .filter(|surface| !surface.trim().is_empty())
            .ok_or_else(|| "Codex agent.update requires explicit params.surface_id".to_owned())?
            .parse::<SurfaceId>()
            .map_err(|_| "invalid params.surface_id".to_owned())?;
        if let Some(alias) = optional_uuid_param(params, "surface")?
            && alias != surface
        {
            return Err("params.surface does not match params.surface_id".into());
        }
        let terminal = match self
            .state
            .surfaces
            .get(&surface)
            .ok_or_else(|| "target surface not found".to_owned())?
            .kind
        {
            SurfaceKind::Terminal(terminal) => terminal,
            _ => return Err("target surface is not a terminal".into()),
        };
        if let Some(requested_terminal) = optional_uuid_param(params, "terminal")?
            && requested_terminal != terminal
        {
            return Err("target terminal does not match params.surface_id".into());
        }
        let runtime = self
            .terminals
            .get(&terminal)
            .ok_or_else(|| "target terminal is not running".to_owned())?;
        let lifecycle = params
            .get("lifecycle_id")
            .and_then(Value::as_str)
            .filter(|lifecycle| !lifecycle.trim().is_empty())
            .ok_or_else(|| {
                "Codex agent.update requires non-empty params.lifecycle_id".to_owned()
            })?;
        if lifecycle != runtime.lifecycle_id {
            return Err("stale agent event for a previous terminal lifecycle".into());
        }
        Ok((terminal, surface))
    }

    fn reload_config_if_changed(&mut self) {
        let Some(reloader) = &mut self.config_reloader else {
            return;
        };
        match reloader.reload_if_changed() {
            Ok(Some(config)) => match config.resolved_keybindings() {
                Ok(keybindings) => {
                    self.terminal_font_size = config.terminal.font_size.clamp(8.0, 40.0);
                    self.language = Language::resolve(config.ui.language);
                    self.config = config;
                    self.keybindings = keybindings;
                    if let Some(renderer) = &mut self.renderer {
                        renderer.set_transparent(self.config.window.opacity < 1.0);
                    }
                    let theme = terminal_theme(&self.config.theme);
                    for runtime in self.terminals.values() {
                        let _ = runtime
                            .session
                            .send(SessionCommand::SetTheme(theme.clone()));
                    }
                    self.resize_terminals();
                    self.rebuild_scene();
                }
                Err(error) => tracing::warn!(%error, "ignoring invalid reloaded keybindings"),
            },
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "reloading configuration failed"),
        }
    }

    fn drain_terminal_events(&mut self, _event_loop: &ActiveEventLoop) {
        self.wake_pending.store(false, Ordering::Release);
        let active_terminal = self.active_terminal_id();
        let visible_terminals: HashSet<_> = self
            .pane_rects()
            .into_iter()
            .filter_map(|(pane, _)| {
                self.state
                    .panes
                    .get(&pane)
                    .and_then(|pane| pane.active_surface())
                    .and_then(|surface| self.state.surfaces.get(&surface))
                    .and_then(|surface| match surface.kind {
                        SurfaceKind::Terminal(terminal) => Some(terminal),
                        _ => None,
                    })
            })
            .collect();
        let events: Vec<_> = self
            .pending
            .lock()
            .expect("PTY event queue poisoned")
            .drain(..)
            .collect();
        let mut changed = false;
        let mut saw_snapshot = false;
        for tagged in events {
            let terminal_id = tagged.terminal;
            match tagged.event {
                SessionEvent::Snapshot(snapshot) => {
                    if let Some(runtime) = self.terminals.get_mut(&terminal_id) {
                        runtime.snapshot = snapshot;
                        runtime.snapshot_revision = runtime.snapshot_revision.wrapping_add(1);
                        changed |= clear_recoverable_terminal_notice(
                            &mut self.terminal_notices,
                            terminal_id,
                        );
                        if visible_terminals.contains(&terminal_id) {
                            changed = true;
                            saw_snapshot |= active_terminal == Some(terminal_id);
                        }
                    }
                }
                SessionEvent::Exited(status) => {
                    tracing::info!(terminal=%tagged.terminal, %status, "terminal exited");
                    self.close_agent_session_for_terminal(terminal_id, status.clone());
                    self.clear_manual_agent_resume_after_terminal_exit(terminal_id);
                    self.report_terminal_notice(terminal_id, TerminalNotice::Exited(status));
                    changed = true;
                }
                SessionEvent::Error(error) => {
                    tracing::error!(terminal=%tagged.terminal, %error, "PTY error");
                    self.report_terminal_notice(terminal_id, TerminalNotice::Error(error));
                    changed = true;
                }
                SessionEvent::Warning(warning) => {
                    tracing::warn!(terminal=%tagged.terminal, %warning, "recoverable terminal error");
                    self.report_terminal_notice(terminal_id, TerminalNotice::Warning(warning));
                    changed = true;
                }
                SessionEvent::TitleChanged(title) => { if let Some(surface) = self.state.surfaces.values_mut().find(|surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal_id)) { surface.title = title; changed = true; } }
                SessionEvent::Bell => {
                    changed |= self.handle_terminal_bell(terminal_id);
                }
                SessionEvent::CwdChanged(cwd) => {
                    if let Some(index) = self.workspace_for_terminal(terminal_id)
                        && let Some(workspace) = self.state.workspaces.get_mut(index)
                    {
                        workspace.cwd = cwd.into();
                    }
                    self.refresh_workspace_metadata();
                    changed = true;
                }
                SessionEvent::ForegroundProcess(commands) => {
                    changed |= self.update_foreground_agent(terminal_id, &commands);
                }
                SessionEvent::ChildProcesses(commands) => {
                    if commands.is_empty() {
                        changed |= self.terminal_processes.remove(&terminal_id).is_some();
                    } else if self.terminal_processes.get(&terminal_id) != Some(&commands) {
                        self.terminal_processes.insert(terminal_id, commands);
                        changed = true;
                    }
                }
                SessionEvent::Agent(event) => {
                    let lifecycle = self
                        .terminals
                        .get(&terminal_id)
                        .map(|runtime| runtime.lifecycle_id.clone());
                    let surface = self.surface_for_terminal(terminal_id);
                    let params = json!({
                        "surface": surface,
                        "lifecycle_id": lifecycle,
                    });
                    if self
                        .record_agent_event(terminal_id, &params, &event)
                        .unwrap_or(false)
                    {
                        self.apply_agent_event(terminal_id, event);
                    }
                    changed = true;
                }
                SessionEvent::Notification(message) => { self.notify_terminal(terminal_id, &message); changed = true; }
                SessionEvent::ClipboardText { text, target } => {
                    if let Err(error) = self.set_clipboard_text(text, target) {
                        tracing::warn!(%error, "copying terminal text to the clipboard failed");
                    }
                }
                SessionEvent::SearchResult(result) => { self.search_result = Some(result); changed = true; }
            }
        }
        if saw_snapshot {
            self.reset_cursor_blink();
        }
        if changed {
            self.rebuild_scene();
        }
    }

    fn split_active(&mut self, axis: SplitAxis) {
        self.split_active_command(axis, None);
    }

    fn split_active_command(&mut self, axis: SplitAxis, command: Option<String>) {
        match self.state.split_active(axis, 0.5) {
            Ok(pane) => {
                let terminal = self
                    .state
                    .panes
                    .get(&pane)
                    .and_then(|p| p.active_surface())
                    .and_then(|id| self.state.surfaces.get(&id))
                    .and_then(|s| match s.kind {
                        SurfaceKind::Terminal(id) => Some(id),
                        _ => None,
                    });
                if let Some(id) = terminal {
                    if let Some(command) = command {
                        self.commands.insert(id, command);
                    }
                    if let Err(error) = self.spawn_terminal(id) {
                        tracing::error!(%error, "spawning split terminal failed");
                        self.report_terminal_notice(
                            id,
                            TerminalNotice::SpawnFailed(error.to_string()),
                        );
                    }
                }
                self.resize_terminals();
                self.rebuild_scene();
            }
            Err(error) => tracing::error!(%error, "splitting pane failed"),
        }
    }

    fn notify_terminal(&mut self, terminal: TerminalId, body: &str) {
        let Some(surface) = self
            .state
            .surfaces
            .values()
            .find(|surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal))
            .map(|surface| surface.id)
        else {
            return;
        };
        let Some(workspace) = self
            .workspace_for_terminal(terminal)
            .and_then(|index| self.state.workspaces.get(index))
            .map(|workspace| workspace.id)
        else {
            return;
        };
        let created_at_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_millis() as u64)
            .unwrap_or_default();
        let read =
            notification_arrives_read(self.window_focused, self.active_terminal_id(), terminal);
        self.state.push_notification(Notification {
            id: uuid::Uuid::new_v4(),
            workspace,
            surface,
            title: "wmcterm".into(),
            body: body.into(),
            attention: AttentionState::NeedsInput,
            created_at_unix_ms,
            read,
        });
        if read {
            self.sync_notification_attention(surface);
        }
        self.notification_scroll = 0;
        self.notification_selection = 0;
        if self.config.agent.notifications && !read {
            show_native_notification("wmcterm", body);
        }
    }

    fn handle_terminal_bell(&mut self, terminal: TerminalId) -> bool {
        let now = std::time::Instant::now();
        let since_last = self
            .last_bell_notifications
            .get(&terminal)
            .map(|last| now.saturating_duration_since(*last));
        if !should_notify_terminal_bell(
            self.config.terminal.bell_notifications,
            self.window_focused,
            self.active_terminal_id() == Some(terminal),
            since_last,
            std::time::Duration::from_millis(self.config.terminal.bell_debounce_ms),
        ) {
            return false;
        }
        self.last_bell_notifications.insert(terminal, now);
        self.notify_terminal(terminal, self.language.terminal_bell());
        true
    }

    fn report_terminal_notice(&mut self, terminal: TerminalId, notice: TerminalNotice) {
        if matches!(notice, TerminalNotice::Warning(_))
            && self
                .terminal_notices
                .get(&terminal)
                .is_some_and(terminal_notice_is_error)
        {
            return;
        }
        if let Some(attention) = terminal_notice_attention(&notice)
            && let Some(surface) =
                self.state.surfaces.values_mut().find(
                    |surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal),
                )
        {
            surface.attention = attention;
        }
        if terminal_notice_is_error(&notice)
            && let Some(index) = self.workspace_for_terminal(terminal)
            && let Some(workspace) = self.state.workspaces.get_mut(index)
        {
            workspace.status = wmcterm_core::WorkspaceStatus::Error;
        }
        self.terminal_notices.insert(terminal, notice);
    }

    fn apply_agent_event(
        &mut self,
        terminal: TerminalId,
        event: AgentEvent,
    ) -> wmcterm_core::AgentState {
        self.automatic_agent_status.remove(&terminal);
        let run_mode = self
            .surface_for_terminal(terminal)
            .and_then(|surface| self.state.surfaces.get(&surface))
            .and_then(|surface| surface.agent.as_ref())
            .map(|agent| agent.run_mode)
            .unwrap_or_default();
        let transition = agent_transition_for_run_mode(&event, run_mode);
        let suppress_notification = non_actionable_agent_exit(&event);
        let announced = match &event {
            AgentEvent::SessionStart { agent, session_id } => {
                Some((agent.clone(), session_id.clone()))
            }
            _ => None,
        };
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
            surface.agent = Some(wmcterm_core::AgentMetadata {
                name,
                state: transition.state,
                session_id,
                run_mode,
            });
            surface.attention = transition.attention;
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
        transition.state
    }

    fn record_agent_event(
        &mut self,
        terminal: TerminalId,
        params: &Value,
        event: &AgentEvent,
    ) -> Result<bool, String> {
        let (surface, effective_lifecycle) = self.validate_agent_event_route(terminal, params)?;
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
            || session.lifecycle.external_id.as_deref() != Some(effective_lifecycle.as_str());
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

    fn validate_agent_event_route(
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

    fn resolve_agent_session_request(&mut self, request_id: &str) -> Result<(), String> {
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

    fn conclude_agent_request_state(&mut self, request: &AgentInteractionRequest) {
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

    fn record_claude_hook(
        &mut self,
        terminal: TerminalId,
        hook: &ClaudeHookEvent,
    ) -> Result<(), String> {
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
        apply_claude_subagent_status(&mut self.claude_subagents, surface, hook);
        Ok(())
    }

    fn validate_codex_session(
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

    fn record_codex_subagent_hook(
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

    fn end_codex_subagent_session(
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

    fn expire_agent_requests(&mut self) -> bool {
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

    fn close_agent_session_for_terminal(&mut self, terminal: TerminalId, message: String) {
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
            agent_pid: session.pid,
            pid_start_time: session.process_started_at_unix_ms,
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

    fn clear_manual_agent_resume_after_terminal_exit(&mut self, terminal: TerminalId) {
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

    fn update_foreground_agent(&mut self, terminal: TerminalId, commands: &[String]) -> bool {
        let detected = commands.iter().find_map(|command| {
            AgentKind::from_process_command(command)
                .map(|agent| (agent, agent.run_mode_from_process_command(command)))
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

    fn mark_foreground_agent_running(&mut self, terminal: TerminalId) {
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

    fn mark_foreground_agent_idle(&mut self, terminal: TerminalId) {
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

    fn observe_agent_session_state(&mut self, surface: SurfaceId, state: wmcterm_core::AgentState) {
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

    fn should_quarantine_focus_input(&mut self) -> bool {
        if !self.window_focused {
            return true;
        }
        if !self.focus_input_quarantine {
            return false;
        }
        if focus_input_should_remain_quarantined(
            self.window_focus_gained_at.elapsed(),
            platform_keyboard_has_pressed_keys(),
        ) {
            return true;
        }
        self.focus_input_quarantine = false;
        false
    }

    fn send_key(&mut self, event: &KeyEvent) {
        if matches!(
            event.physical_key,
            PhysicalKey::Code(KeyCode::AltLeft | KeyCode::AltRight)
        ) {
            self.physical_alt_pressed = event.state == ElementState::Pressed;
        }
        if self.should_quarantine_focus_input() || event.state != ElementState::Pressed {
            return;
        }
        self.reset_cursor_blink();
        // Focus transitions are allowed to race keyboard delivery on several
        // desktop backends. Never route a late key press from an unfocused
        // window, and consume the physical Tab from Alt/Super+Tab before any
        // overlay, editor, or terminal gets a chance to interpret it.
        if should_suppress_focus_transition_key(
            self.window_focused,
            &event.logical_key,
            event.physical_key,
            self.modifiers,
            self.physical_alt_pressed,
        ) {
            return;
        }
        if self.agent_request_detail.is_some() {
            match event.logical_key {
                Key::Named(NamedKey::Escape) | Key::Named(NamedKey::Enter) => {
                    self.agent_request_detail = None;
                    self.agent_request_detail_scrollbar_drag = None;
                }
                Key::Named(NamedKey::ArrowUp) => self.scroll_agent_request_detail(-1),
                Key::Named(NamedKey::ArrowDown) => self.scroll_agent_request_detail(1),
                Key::Named(NamedKey::PageUp) => {
                    let page = self.agent_request_detail_page_size() as isize;
                    self.scroll_agent_request_detail(-page);
                }
                Key::Named(NamedKey::PageDown) => {
                    let page = self.agent_request_detail_page_size() as isize;
                    self.scroll_agent_request_detail(page);
                }
                Key::Named(NamedKey::Home) => self.set_agent_request_detail_scroll(0),
                Key::Named(NamedKey::End) => {
                    let maximum = self.agent_request_detail_max_scroll();
                    self.set_agent_request_detail_scroll(maximum);
                }
                _ => return,
            }
            self.rebuild_scene();
            return;
        }
        if self.codex_trust_confirmation.is_some() {
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.codex_trust_confirmation = None;
                    self.update_navigation_hover();
                    self.update_cursor_icon();
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::Enter) => self.continue_untrusted_codex(),
                Key::Character(character) if character.eq_ignore_ascii_case("t") => {
                    self.trust_and_start_unattended_codex();
                }
                _ => {}
            }
            return;
        }
        if self.close_confirmation.is_some() {
            match event.logical_key {
                Key::Named(NamedKey::Enter) => self.confirm_close(),
                Key::Named(NamedKey::Escape) => {
                    self.close_confirmation = None;
                    self.update_navigation_hover();
                    self.update_cursor_icon();
                    self.rebuild_scene();
                }
                _ => {}
            }
            return;
        }
        if self.tab_overflow_menu.is_some()
            || self.tab_context_menu.is_some()
            || self.workspace_context_menu.is_some()
            || self.sidebar_context_menu.is_some()
            || self.terminal_context_menu.is_some()
            || self.notification_context_menu.is_some()
        {
            self.tab_overflow_menu = None;
            self.tab_context_menu = None;
            self.workspace_context_menu = None;
            self.sidebar_context_menu = None;
            self.terminal_context_menu = None;
            self.notification_context_menu = None;
            self.rebuild_scene();
            return;
        }
        // Approval state can change asynchronously. A floating notification
        // panel must never take keyboard ownership from an editor that the
        // user explicitly opened.
        if self.notifications_visible && !self.text_editor_active() {
            let pending = self.pending_agent_request_list();
            if !pending.is_empty() {
                match event.logical_key {
                    Key::Named(NamedKey::Escape) => {
                        self.notifications_visible = false;
                        self.agent_request_scroll = 0;
                        self.agent_request_selection = 0;
                        self.agent_request_action = 0;
                    }
                    Key::Named(NamedKey::ArrowUp) => self.move_agent_request_selection(-1),
                    Key::Named(NamedKey::ArrowDown) => self.move_agent_request_selection(1),
                    Key::Named(NamedKey::ArrowLeft) => self.move_agent_request_action(-1),
                    Key::Named(NamedKey::ArrowRight) => self.move_agent_request_action(1),
                    Key::Named(NamedKey::Enter) => self.resolve_selected_agent_request(),
                    _ => return,
                }
                self.rebuild_scene();
                return;
            }
            match event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.notifications_visible = false;
                    self.notification_scroll = 0;
                    self.notification_selection = 0;
                }
                Key::Named(NamedKey::ArrowUp) => self.move_notification_selection(-1),
                Key::Named(NamedKey::ArrowDown) => self.move_notification_selection(1),
                Key::Named(NamedKey::Enter) => {
                    if let Some(notification) = self.selected_notification_id() {
                        self.focus_notification(notification);
                    }
                }
                Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) => {
                    if let Some(notification) = self.selected_notification_id() {
                        self.clear_notification(notification);
                    }
                }
                _ => return,
            }
            self.rebuild_scene();
            return;
        }
        if self.workspace_search_focused {
            if let Some(shortcut) = text_edit_shortcut(event, self.modifiers) {
                let copied = apply_text_edit_shortcut(&mut self.workspace_search, shortcut);
                if let (Some(clipboard), Some(text)) = (&mut self.clipboard, copied) {
                    let _ = clipboard.set_text(text);
                }
                self.sidebar_scroll = 0;
                self.rebuild_scene();
                return;
            }
            let paste = matches!(&event.logical_key, Key::Character(value) if value.eq_ignore_ascii_case("v"))
                && (self.modifiers.control_key() || self.modifiers.super_key());
            if paste {
                if let Some(text) = self
                    .clipboard
                    .as_mut()
                    .and_then(|clipboard| clipboard.get_text().ok())
                {
                    self.workspace_search.insert(&text);
                    self.sidebar_scroll = 0;
                    self.rebuild_scene();
                }
                return;
            }
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.workspace_search_focused = false;
                }
                Key::Named(NamedKey::Enter) => {
                    if let Some(index) = self.filtered_workspace_indices().first().copied() {
                        self.state.active_workspace = index;
                        self.resize_terminals();
                    }
                    self.workspace_search_focused = false;
                }
                Key::Named(NamedKey::Backspace) => self.workspace_search.backspace(),
                Key::Named(NamedKey::Delete) => self.workspace_search.delete(),
                Key::Named(NamedKey::ArrowLeft) => self
                    .workspace_search
                    .move_left_with_selection(self.modifiers.shift_key()),
                Key::Named(NamedKey::ArrowRight) => self
                    .workspace_search
                    .move_right_with_selection(self.modifiers.shift_key()),
                Key::Named(NamedKey::Home) => self
                    .workspace_search
                    .move_home_with_selection(self.modifiers.shift_key()),
                Key::Named(NamedKey::End) => self
                    .workspace_search
                    .move_end_with_selection(self.modifiers.shift_key()),
                Key::Named(NamedKey::Space)
                    if !self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key()
                        && !self.workspace_search.has_preedit() =>
                {
                    self.workspace_search.insert(" ");
                }
                Key::Character(value)
                    if !self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key()
                        && !self.workspace_search.has_preedit() =>
                {
                    self.workspace_search.insert(value);
                }
                _ => {}
            }
            self.sidebar_scroll = 0;
            self.rebuild_scene();
            return;
        }
        if self.prompt.is_some() {
            if let Some(shortcut) = text_edit_shortcut(event, self.modifiers) {
                let copied = self
                    .prompt
                    .as_mut()
                    .and_then(|prompt| apply_text_edit_shortcut(&mut prompt.editor, shortcut));
                if let (Some(clipboard), Some(text)) = (&mut self.clipboard, copied) {
                    let _ = clipboard.set_text(text);
                }
                self.rebuild_scene();
                return;
            }
            let paste = matches!(&event.logical_key, Key::Character(value) if value.eq_ignore_ascii_case("v"))
                && (self.modifiers.control_key() || self.modifiers.super_key());
            if paste {
                let text = self
                    .clipboard
                    .as_mut()
                    .and_then(|clipboard| clipboard.get_text().ok());
                if let (Some(prompt), Some(text)) = (&mut self.prompt, text) {
                    prompt.editor.insert(&text);
                    prompt.error = None;
                    self.rebuild_scene();
                }
                return;
            }
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.prompt = None;
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::Enter) => self.submit_prompt(),
                Key::Named(NamedKey::Backspace) => {
                    if let Some(prompt) = &mut self.prompt {
                        prompt.editor.backspace();
                        prompt.error = None;
                    }
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::Delete) => {
                    if let Some(prompt) = &mut self.prompt {
                        prompt.editor.delete();
                        prompt.error = None;
                    }
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::ArrowLeft) => {
                    if let Some(prompt) = &mut self.prompt {
                        prompt
                            .editor
                            .move_left_with_selection(self.modifiers.shift_key());
                    }
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::ArrowRight) => {
                    if let Some(prompt) = &mut self.prompt {
                        prompt
                            .editor
                            .move_right_with_selection(self.modifiers.shift_key());
                    }
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::Home) => {
                    if let Some(prompt) = &mut self.prompt {
                        prompt
                            .editor
                            .move_home_with_selection(self.modifiers.shift_key());
                    }
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::End) => {
                    if let Some(prompt) = &mut self.prompt {
                        prompt
                            .editor
                            .move_end_with_selection(self.modifiers.shift_key());
                    }
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::Space)
                    if !self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key() =>
                {
                    if let Some(prompt) = &mut self.prompt {
                        if prompt.editor.has_preedit() {
                            return;
                        }
                        prompt.editor.insert(" ");
                        prompt.error = None;
                    }
                    self.rebuild_scene();
                }
                Key::Character(value)
                    if !self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key() =>
                {
                    if let Some(prompt) = &mut self.prompt {
                        // While an IME preedit is active, only Ime::Commit owns
                        // text insertion. This avoids committing composition twice.
                        if prompt.editor.has_preedit() {
                            return;
                        }
                        prompt.editor.insert(value);
                        prompt.error = None;
                    }
                    self.rebuild_scene();
                }
                _ => {}
            }
            return;
        }
        if self.search_visible {
            if let Some(shortcut) = text_edit_shortcut(event, self.modifiers) {
                let copied = apply_text_edit_shortcut(&mut self.search_editor, shortcut);
                if let (Some(clipboard), Some(text)) = (&mut self.clipboard, copied) {
                    let _ = clipboard.set_text(text);
                }
                self.search_active(1);
                return;
            }
            let paste = matches!(&event.logical_key, Key::Character(value) if value.eq_ignore_ascii_case("v"))
                && (self.modifiers.control_key() || self.modifiers.super_key());
            if paste {
                if let Some(text) = self
                    .clipboard
                    .as_mut()
                    .and_then(|clipboard| clipboard.get_text().ok())
                {
                    self.search_editor.insert(&text);
                    self.search_active(1);
                }
                return;
            }
            match &event.logical_key {
                Key::Named(NamedKey::Escape) => {
                    self.search_visible = false;
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::Enter) => {
                    self.search_active(if self.modifiers.shift_key() { -1 } else { 1 });
                }
                Key::Named(NamedKey::Backspace) => {
                    self.search_editor.backspace();
                    self.search_active(1);
                }
                Key::Named(NamedKey::Delete) => {
                    self.search_editor.delete();
                    self.search_active(1);
                }
                Key::Named(NamedKey::ArrowLeft) => {
                    self.search_editor
                        .move_left_with_selection(self.modifiers.shift_key());
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.search_editor
                        .move_right_with_selection(self.modifiers.shift_key());
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::Home) => {
                    self.search_editor
                        .move_home_with_selection(self.modifiers.shift_key());
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::End) => {
                    self.search_editor
                        .move_end_with_selection(self.modifiers.shift_key());
                    self.rebuild_scene();
                }
                Key::Named(NamedKey::Space)
                    if !self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key() =>
                {
                    if self.search_editor.has_preedit() {
                        return;
                    }
                    self.search_editor.insert(" ");
                    self.search_active(1);
                }
                Key::Character(value)
                    if !self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key() =>
                {
                    if self.search_editor.has_preedit() {
                        return;
                    }
                    self.search_editor.insert(value);
                    self.search_active(1);
                }
                _ => {}
            }
            return;
        }
        if self.palette_visible {
            if let Some(shortcut) = text_edit_shortcut(event, self.modifiers) {
                let copied = apply_text_edit_shortcut(&mut self.palette_query, shortcut);
                if let (Some(clipboard), Some(text)) = (&mut self.clipboard, copied) {
                    let _ = clipboard.set_text(text);
                }
                self.palette_index = 0;
                self.palette_scroll = 0;
                self.palette_hover = None;
                self.rebuild_scene();
                return;
            }
            let paste = matches!(&event.logical_key, Key::Character(value) if value.eq_ignore_ascii_case("v"))
                && (self.modifiers.control_key() || self.modifiers.super_key());
            if paste {
                if let Some(text) = self
                    .clipboard
                    .as_mut()
                    .and_then(|clipboard| clipboard.get_text().ok())
                {
                    self.palette_query.insert(&text);
                    self.palette_index = 0;
                    self.palette_scroll = 0;
                    self.palette_hover = None;
                    self.rebuild_scene();
                }
                return;
            }
            match event.logical_key {
                Key::Named(NamedKey::ArrowUp) => {
                    self.palette_hover = None;
                    self.palette_index = self.palette_index.saturating_sub(1);
                    let registry = ActionRegistry::default();
                    let count = self.palette_actions(&registry).len();
                    self.ensure_palette_selection_visible(count);
                    self.rebuild_scene();
                    return;
                }
                Key::Named(NamedKey::ArrowDown) => {
                    self.palette_hover = None;
                    let registry = ActionRegistry::default();
                    let count = self.palette_actions(&registry).len();
                    self.palette_index = (self.palette_index + 1).min(count.saturating_sub(1));
                    self.ensure_palette_selection_visible(count);
                    self.rebuild_scene();
                    return;
                }
                Key::Named(NamedKey::Enter) => {
                    self.execute_palette_action();
                    return;
                }
                Key::Named(NamedKey::Escape) => {
                    self.palette_visible = false;
                    self.palette_hover = None;
                    self.rebuild_scene();
                    return;
                }
                Key::Named(NamedKey::Backspace) => {
                    self.palette_query.backspace();
                    self.palette_index = 0;
                    self.palette_scroll = 0;
                    self.palette_hover = None;
                    self.rebuild_scene();
                    return;
                }
                Key::Named(NamedKey::Delete) => {
                    self.palette_query.delete();
                    self.palette_index = 0;
                    self.palette_scroll = 0;
                    self.palette_hover = None;
                    self.rebuild_scene();
                    return;
                }
                Key::Named(NamedKey::ArrowLeft) => {
                    self.palette_query
                        .move_left_with_selection(self.modifiers.shift_key());
                    self.rebuild_scene();
                    return;
                }
                Key::Named(NamedKey::ArrowRight) => {
                    self.palette_query
                        .move_right_with_selection(self.modifiers.shift_key());
                    self.rebuild_scene();
                    return;
                }
                Key::Named(NamedKey::Home) => {
                    self.palette_query
                        .move_home_with_selection(self.modifiers.shift_key());
                    self.rebuild_scene();
                    return;
                }
                Key::Named(NamedKey::End) => {
                    self.palette_query
                        .move_end_with_selection(self.modifiers.shift_key());
                    self.rebuild_scene();
                    return;
                }
                Key::Named(NamedKey::Space)
                    if !self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key() =>
                {
                    if self.palette_query.has_preedit() {
                        return;
                    }
                    self.palette_query.insert(" ");
                    self.palette_index = 0;
                    self.palette_scroll = 0;
                    self.palette_hover = None;
                    self.rebuild_scene();
                    return;
                }
                Key::Character(ref value)
                    if !self.modifiers.control_key()
                        && !self.modifiers.super_key()
                        && !self.modifiers.alt_key() =>
                {
                    if self.palette_query.has_preedit() {
                        return;
                    }
                    self.palette_query.insert(value);
                    self.palette_index = 0;
                    self.palette_scroll = 0;
                    self.palette_hover = None;
                    self.rebuild_scene();
                    return;
                }
                _ => {}
            }
            return;
        }
        if self.handle_exited_terminal_key(event) {
            return;
        }
        if let Some(shortcut) = shortcut_for_event(event, self.modifiers)
            && let Some(action) = self.keybindings.action_for(&shortcut).map(str::to_owned)
        {
            self.execute_action(&action);
            return;
        }
        if self.config.ui.legacy_shortcuts {
            if matches!(event.logical_key, Key::Named(NamedKey::F2))
                && !self.modifiers.control_key()
                && !self.modifiers.alt_key()
                && !self.modifiers.super_key()
            {
                self.open_workspace_prompt(PromptKind::RenameWorkspace);
                return;
            }
            if self.modifiers.control_key() && self.modifiers.shift_key() {
                match physical_ascii_character(event.physical_key) {
                    Some('d') => {
                        self.split_active(SplitAxis::Vertical);
                        return;
                    }
                    Some('e') => {
                        self.split_active(SplitAxis::Horizontal);
                        return;
                    }
                    Some('p') => {
                        self.palette_visible = !self.palette_visible;
                        self.palette_query.clear();
                        self.palette_index = 0;
                        self.palette_scroll = 0;
                        self.palette_hover = None;
                        self.rebuild_scene();
                        return;
                    }
                    Some('w') => {
                        self.close_active_pane();
                        return;
                    }
                    Some('x') => {
                        self.request_close_active_surface();
                        return;
                    }
                    Some('t') => {
                        self.new_surface();
                        return;
                    }
                    Some('a') => {
                        self.new_workspace();
                        return;
                    }
                    Some('b') => {
                        self.sidebar_visible = !self.sidebar_visible;
                        self.resize_terminals();
                        self.rebuild_scene();
                        return;
                    }
                    Some('n') => {
                        self.notifications_visible = !self.notifications_visible;
                        if self.notifications_visible {
                            self.notification_scroll = 0;
                            self.notification_selection = 0;
                        }
                        self.rebuild_scene();
                        return;
                    }
                    Some('k') => {
                        self.clear_scrollback();
                        return;
                    }
                    Some('z') => {
                        self.toggle_pane_zoom();
                        return;
                    }
                    Some('r') => {
                        self.open_workspace_prompt(PromptKind::RenameWorkspace);
                        return;
                    }
                    Some('u') => {
                        self.jump_latest_unread();
                        return;
                    }
                    Some('l') => {
                        self.open_log_filter_prompt();
                        return;
                    }
                    _ if matches!(event.logical_key, Key::Named(NamedKey::PageUp)) => {
                        self.move_workspace(-1);
                        return;
                    }
                    _ if matches!(event.logical_key, Key::Named(NamedKey::PageDown)) => {
                        self.move_workspace(1);
                        return;
                    }
                    Some('c') => {
                        if !self.copy_selection(ClipboardTarget::Clipboard)
                            && let Some(terminal) = self.active_terminal_id()
                        {
                            self.copy_all_to_clipboard(terminal);
                        }
                        return;
                    }
                    Some('v') => {
                        self.paste_clipboard(ClipboardTarget::Clipboard);
                        return;
                    }
                    _ => {}
                }
            }
            if self.modifiers.control_key() && !self.modifiers.shift_key() {
                match &event.logical_key {
                    Key::Character(value) if value.eq_ignore_ascii_case("f") => {
                        self.search_visible = true;
                        self.search_editor = TextEditor::default();
                        self.search_result = None;
                        self.rebuild_scene();
                        return;
                    }
                    Key::Character(value) if value.eq_ignore_ascii_case("g") => {
                        if !self.search_editor.text().is_empty() {
                            self.search_active(1);
                        }
                        return;
                    }
                    Key::Character(value) if value.as_str() == "+" || value.as_str() == "=" => {
                        self.adjust_font_size(1.0);
                        return;
                    }
                    Key::Character(value) if value.as_str() == "-" => {
                        self.adjust_font_size(-1.0);
                        return;
                    }
                    Key::Character(value) if value.as_str() == "0" => {
                        self.reset_font_size();
                        return;
                    }
                    _ => {}
                }
            }
            if self.modifiers.alt_key() {
                let direction = match event.logical_key {
                    Key::Named(NamedKey::ArrowLeft) => Some((-1.0, 0.0)),
                    Key::Named(NamedKey::ArrowRight) => Some((1.0, 0.0)),
                    Key::Named(NamedKey::ArrowUp) => Some((0.0, -1.0)),
                    Key::Named(NamedKey::ArrowDown) => Some((0.0, 1.0)),
                    _ => None,
                };
                if let Some(direction) = direction {
                    self.focus_pane_direction(direction);
                    return;
                }
                if let Key::Character(value) = &event.logical_key
                    && let Ok(number) = value.parse::<usize>()
                    && (1..=9).contains(&number)
                {
                    self.activate_surface_number(number - 1);
                    return;
                }
            }
            if self.modifiers.control_key()
                && !self.modifiers.shift_key()
                && let Key::Character(value) = &event.logical_key
                && let Ok(number) = value.parse::<usize>()
                && (1..=9).contains(&number)
            {
                if number <= self.state.workspaces.len() {
                    self.state.active_workspace = number - 1;
                    self.resize_terminals();
                    self.rebuild_scene();
                }
                return;
            }
            if self.modifiers.control_key()
                && matches!(event.logical_key, Key::Named(NamedKey::Tab))
            {
                self.cycle_surface(if self.modifiers.shift_key() { -1 } else { 1 });
                return;
            }
        }
        let physical_character = physical_ascii_character(event.physical_key);
        let key = terminal_input_key(&event.logical_key, physical_character);
        if let (Some(id), Some(key)) = (self.active_terminal_id(), key) {
            let notifications_changed = self.mark_terminal_notifications_read(id);
            let control_c = is_agent_interrupt(event.physical_key, self.modifiers);
            if control_c && self.foreground_agents.contains_key(&id) {
                self.foreground_agent_input.remove(&id);
                self.mark_foreground_agent_idle(id);
                self.rebuild_scene();
            } else if matches!(key, InputKey::Enter)
                && !self.modifiers.control_key()
                && !self.modifiers.alt_key()
                && !self.modifiers.super_key()
            {
                let input = self.foreground_agent_input.remove(&id).unwrap_or_default();
                if agent_submission_starts_turn(&input) {
                    self.mark_foreground_agent_running(id);
                    self.rebuild_scene();
                } else if notifications_changed {
                    self.rebuild_scene();
                }
            } else if notifications_changed {
                self.rebuild_scene();
            }
            if self.foreground_agents.contains_key(&id)
                && !self.modifiers.control_key()
                && !self.modifiers.alt_key()
                && !self.modifiers.super_key()
            {
                match key {
                    InputKey::Character(character) => {
                        self.foreground_agent_input
                            .entry(id)
                            .or_default()
                            .push(character);
                    }
                    InputKey::Space => {
                        self.foreground_agent_input.entry(id).or_default().push(' ');
                    }
                    InputKey::Backspace => {
                        if let Some(input) = self.foreground_agent_input.get_mut(&id) {
                            input.pop();
                        }
                    }
                    _ => {}
                }
            }
            let Some(runtime) = self.terminals.get_mut(&id) else {
                return;
            };
            let text = event
                .text
                .as_ref()
                .filter(|text| !text.chars().any(char::is_control))
                .map(ToString::to_string)
                .or_else(|| match event.logical_key.clone() {
                    Key::Character(value) if !value.chars().any(char::is_control) => {
                        Some(value.to_string())
                    }
                    Key::Named(NamedKey::Space) => Some(" ".into()),
                    _ => physical_character.map(|character| character.to_string()),
                });
            let input = KeyInput {
                key,
                text,
                modifiers: KeyModifiers {
                    shift: self.modifiers.shift_key(),
                    alt: self.modifiers.alt_key(),
                    control: self.modifiers.control_key(),
                    super_key: self.modifiers.super_key(),
                },
                repeat: event.repeat,
            };
            let _ = runtime.session.send(SessionCommand::Key(input));
        }
    }

    fn flush_pending_unmodified_tab(&mut self) {
        let Some((event, queued_at)) = self.pending_unmodified_tab.take() else {
            return;
        };
        if !self.window_focused {
            return;
        }
        if queued_at.elapsed() >= UNMODIFIED_TAB_FOCUS_GRACE {
            self.send_key(&event);
        } else {
            self.pending_unmodified_tab = Some((event, queued_at));
        }
    }

    fn commit_ime_text(&mut self, text: String) {
        if text.is_empty()
            || self.should_quarantine_focus_input()
            || self.close_confirmation.is_some()
            || self.codex_trust_confirmation.is_some()
        {
            return;
        }
        self.reset_cursor_blink();
        if self.workspace_search_focused {
            self.workspace_search.insert(&text);
            self.sidebar_scroll = 0;
            self.rebuild_scene();
        } else if let Some(prompt) = &mut self.prompt {
            prompt.editor.insert(&text);
            prompt.error = None;
            self.rebuild_scene();
        } else if self.search_visible {
            self.search_editor.insert(&text);
            self.search_active(1);
        } else if self.palette_visible {
            self.palette_query.insert(&text);
            self.palette_index = 0;
            self.palette_scroll = 0;
            self.palette_hover = None;
            self.rebuild_scene();
        } else if let Some(terminal) = self.active_terminal_id() {
            let notifications_changed = self.mark_terminal_notifications_read(terminal);
            if let Some(runtime) = self.terminals.get(&terminal) {
                let _ = runtime
                    .session
                    .send(SessionCommand::Input(text.into_bytes()));
            }
            if notifications_changed {
                self.rebuild_scene();
            }
        }
    }

    fn text_editor_active(&self) -> bool {
        self.workspace_search_focused
            || self.prompt.is_some()
            || self.search_visible
            || self.palette_visible
    }

    fn update_ime_preedit(&mut self, text: &str) {
        if self.close_confirmation.is_some() || self.codex_trust_confirmation.is_some() {
            return;
        }
        if self.workspace_search_focused {
            self.workspace_search.set_preedit(text);
            self.rebuild_scene();
        } else if let Some(prompt) = &mut self.prompt {
            prompt.editor.set_preedit(text);
            self.rebuild_scene();
        } else if self.search_visible {
            self.search_editor.set_preedit(text);
            self.rebuild_scene();
        } else if self.palette_visible {
            self.palette_query.set_preedit(text);
            self.rebuild_scene();
        }
    }

    fn execute_palette_action(&mut self) {
        let registry = ActionRegistry::default();
        let actions = self.palette_actions(&registry);
        let index = self.palette_hover.unwrap_or(self.palette_index);
        let Some(action) = actions.get(index) else {
            return;
        };
        let action_id = action.id().to_owned();
        self.palette_visible = false;
        self.palette_hover = None;
        self.execute_action(&action_id);
    }

    fn palette_actions<'a>(
        &self,
        registry: &'a ActionRegistry,
    ) -> Vec<&'a wmcterm_core::BuiltinAction> {
        let query = self.palette_query.text().to_lowercase();
        registry
            .actions()
            .iter()
            .filter(|action| {
                query.is_empty()
                    || action.id().to_lowercase().contains(&query)
                    || action.title().to_lowercase().contains(&query)
                    || self
                        .language
                        .action_title(action.id(), action.title())
                        .to_lowercase()
                        .contains(&query)
            })
            .collect()
    }

    fn shortcut_for_action(&self, action_id: &str) -> Option<String> {
        if let Some(configured) = self.keybindings.iter().find_map(|(shortcut, action)| {
            (action == action_id).then(|| format_shortcut_hint(shortcut))
        }) {
            return Some(configured);
        }
        if !self.config.ui.legacy_shortcuts {
            return None;
        }
        Some(
            match action_id {
                "pane.split_right" => "Ctrl+Shift+D",
                "pane.split_down" => "Ctrl+Shift+E",
                "pane.close" => "Ctrl+Shift+W",
                "surface.close" => "Ctrl+Shift+X",
                "surface.new" => "Ctrl+Shift+T",
                "workspace.new" => "Ctrl+Shift+A",
                "workspace.rename" => "F2",
                "sidebar.toggle" => "Ctrl+Shift+B",
                "notifications.toggle" => "Ctrl+Shift+N",
                "notifications.latest_unread" => "Ctrl+Shift+U",
                "surface.filter_logs" => "Ctrl+Shift+L",
                "terminal.clear_scrollback" => "Ctrl+Shift+K",
                "pane.zoom" => "Ctrl+Shift+Z",
                "terminal.find" => "Ctrl+F",
                "terminal.font_increase" => "Ctrl++",
                "terminal.font_decrease" => "Ctrl+-",
                "terminal.font_reset" => "Ctrl+0",
                _ => return None,
            }
            .to_owned(),
        )
    }

    fn execute_action(&mut self, action_id: &str) {
        match action_id {
            "command.palette" => {
                self.palette_visible = !self.palette_visible;
                self.palette_query.clear();
                self.palette_index = 0;
                self.palette_scroll = 0;
                self.palette_hover = None;
                self.rebuild_scene();
            }
            "workspace.new" => self.new_workspace(),
            "workspace.close" => self.request_close_workspace(),
            "workspace.rename" => self.open_workspace_prompt(PromptKind::RenameWorkspace),
            "workspace.describe" => self.open_workspace_prompt(PromptKind::DescribeWorkspace),
            "workspace.change_directory" => {
                self.open_workspace_prompt(PromptKind::ChangeWorkspaceDirectory)
            }
            "pane.split_right" => self.split_active(SplitAxis::Vertical),
            "pane.split_down" => self.split_active(SplitAxis::Horizontal),
            "surface.new" => self.new_surface(),
            "surface.close" => {
                self.request_close_active_surface();
            }
            "surface.filter_logs" => self.open_log_filter_prompt(),
            "agent.claude" => {
                self.new_surface_command(Some(managed_agent_command(
                    AgentRunMode::Interactive,
                    "claude",
                )));
            }
            "agent.codex" => {
                self.new_surface_command(Some(managed_agent_command(
                    AgentRunMode::Interactive,
                    "codex",
                )));
            }
            "agent.claude.unattended" => {
                self.new_surface_command(Some(managed_agent_command(
                    AgentRunMode::Unattended,
                    "claude",
                )));
            }
            "agent.codex.unattended" => {
                self.launch_unattended_codex();
            }
            "agent.claude.full_access" => {
                self.open_full_access_prompt(ManagedAgent::Claude);
            }
            "agent.codex.full_access" => {
                self.open_full_access_prompt(ManagedAgent::Codex);
            }
            "agent.gemini" => {
                self.new_surface_command(Some("gemini".into()));
            }
            "agent.opencode" => {
                self.new_surface_command(Some("opencode".into()));
            }
            "pane.close" => self.close_active_pane(),
            "pane.zoom" => self.toggle_pane_zoom(),
            "terminal.find" => {
                self.search_visible = true;
                self.search_editor = TextEditor::default();
                self.search_result = None;
                self.rebuild_scene();
            }
            "terminal.find_directory" => self.open_workspace_prompt(PromptKind::FindDirectory),
            "terminal.clear_scrollback" => self.clear_scrollback(),
            "terminal.font_increase" => self.adjust_font_size(1.0),
            "terminal.font_decrease" => self.adjust_font_size(-1.0),
            "terminal.font_reset" => self.reset_font_size(),
            "sidebar.toggle" => {
                self.sidebar_visible = !self.sidebar_visible;
                self.resize_terminals();
                self.rebuild_scene();
            }
            "notifications.toggle" => {
                self.notifications_visible = !self.notifications_visible;
                if self.notifications_visible {
                    self.notification_scroll = 0;
                    self.notification_selection = 0;
                }
                self.rebuild_scene();
            }
            "notifications.latest_unread" => self.jump_latest_unread(),
            "notifications.toggle_unread" => self.toggle_current_notification_unread(),
            "settings.open" => self.open_settings(),
            "window.new" => match env::current_exe()
                .and_then(|binary| ProcessCommand::new(binary).arg("gui").spawn().map(|_| ()))
            {
                Ok(()) => {}
                Err(error) => tracing::error!(%error, "creating a new window failed"),
            },
            "app.quit" => self.request_application_close(),
            _ => {}
        }
    }

    fn open_settings(&mut self) {
        let Some(path) = wmcterm_config::config_path() else {
            return;
        };
        let result = (|| -> Result<()> {
            if !path.exists() {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&path, toml::to_string_pretty(&Config::default())?)?;
            }
            #[cfg(target_os = "linux")]
            ProcessCommand::new("xdg-open").arg(&path).spawn()?;
            #[cfg(target_os = "macos")]
            ProcessCommand::new("open").arg(&path).spawn()?;
            #[cfg(windows)]
            ProcessCommand::new("notepad.exe").arg(&path).spawn()?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::error!(%error, "opening settings failed");
        }
    }

    fn pane_rects(&self) -> Vec<(PaneId, Rect)> {
        let mut output = Vec::new();
        if let Some(workspace) = self.state.active_workspace() {
            let area = terminal_workspace_area(
                self.window_size.width as f32,
                self.window_size.height as f32,
                self.sidebar_width(),
            );
            if self
                .state
                .panes
                .get(&workspace.active_pane)
                .is_some_and(|pane| pane.zoomed)
            {
                output.push((workspace.active_pane, area));
            } else {
                layout(&workspace.root, area, &mut output);
            }
        }
        output
    }

    fn new_surface(&mut self) {
        self.new_surface_command(None);
    }

    fn sidebar_width(&self) -> f32 {
        if !self.sidebar_visible {
            return 0.0;
        }
        let window_width = self.window_size.width as f32;
        let maximum = (window_width - 320.0).max(0.0);
        if maximum >= 160.0 {
            (self.config.sidebar.width as f32).clamp(160.0, maximum)
        } else {
            (window_width * 0.35).min(maximum)
        }
    }

    fn sidebar_resize_hit(&self, x: f32, y: f32) -> bool {
        self.sidebar_visible
            && (x - self.sidebar_width()).abs() <= 5.0
            && y >= 0.0
            && y < self.window_size.height as f32
    }

    fn window_resize_direction(&self, x: f32, y: f32) -> Option<ResizeDirection> {
        if self
            .renderer
            .as_ref()
            .is_some_and(|renderer| renderer.window().is_maximized())
        {
            return None;
        }
        let edge = 5.0;
        let west = x < edge;
        let east = x >= self.window_size.width as f32 - edge;
        let north = y < edge;
        let south = y >= self.window_size.height as f32 - edge;
        match (west, east, north, south) {
            (true, _, true, _) => Some(ResizeDirection::NorthWest),
            (_, true, true, _) => Some(ResizeDirection::NorthEast),
            (true, _, _, true) => Some(ResizeDirection::SouthWest),
            (_, true, _, true) => Some(ResizeDirection::SouthEast),
            (true, _, _, _) => Some(ResizeDirection::West),
            (_, true, _, _) => Some(ResizeDirection::East),
            (_, _, true, _) => Some(ResizeDirection::North),
            (_, _, _, true) => Some(ResizeDirection::South),
            _ => None,
        }
    }

    fn sidebar_layout(&self) -> ui::SidebarLayout {
        let indices = self.filtered_workspace_indices();
        let heights: Vec<_> = indices
            .iter()
            .map(|index| {
                workspace_sidebar_row_height(
                    self.workspace_agent_priority(*index) > 0,
                    self.state.workspaces[*index].git_branch.is_some(),
                )
            })
            .collect();
        let mut layout = ui::sidebar_layout(
            self.sidebar_width(),
            self.window_size.height as f32,
            &heights,
            self.sidebar_scroll,
        );
        for row in &mut layout.rows {
            row.index = indices[row.index];
        }
        layout
    }

    fn workspace_agent_priority(&self, workspace_index: usize) -> u8 {
        let Some(workspace) = self.state.workspaces.get(workspace_index) else {
            return 0;
        };
        let mut pane_ids = Vec::new();
        workspace.root.leaves(&mut pane_ids);
        aggregate_agent_priority(
            pane_ids
                .iter()
                .filter_map(|id| self.state.panes.get(id))
                .flat_map(|pane| pane.surfaces.iter())
                .filter_map(|surface_id| {
                    self.state
                        .surfaces
                        .get(surface_id)
                        .and_then(|surface| surface.agent.as_ref())
                        .map(|agent| (*surface_id, agent))
                })
                .map(|(surface, agent)| {
                    let active_codex_subagent =
                        agent.session_id.as_deref().is_some_and(|session_id| {
                            self.codex_subagents.states.iter().any(|(subagent, state)| {
                                *state == CodexSubagentState::Active
                                    && subagent.session_id == session_id
                                    && subagent.surface == surface
                            })
                        });
                    effective_codex_agent_state(agent.state, active_codex_subagent)
                }),
        )
    }

    fn workspace_claude_subagent_count(&self, workspace_index: usize) -> usize {
        let Some(workspace) = self.state.workspaces.get(workspace_index) else {
            return 0;
        };
        self.claude_subagents
            .values()
            .filter(|subagent| {
                self.state.panes.values().any(|pane| {
                    workspace.root.contains(pane.id) && pane.surfaces.contains(&subagent.surface)
                })
            })
            .count()
    }

    fn workspace_agent_run_mode(&self, workspace_index: usize) -> AgentRunMode {
        let Some(workspace) = self.state.workspaces.get(workspace_index) else {
            return AgentRunMode::Interactive;
        };
        let mut pane_ids = Vec::new();
        workspace.root.leaves(&mut pane_ids);
        pane_ids
            .iter()
            .filter_map(|id| self.state.panes.get(id))
            .flat_map(|pane| pane.surfaces.iter())
            .filter_map(|surface| self.state.surfaces.get(surface))
            .filter_map(|surface| surface.agent.as_ref())
            .map(|agent| agent.run_mode)
            .max_by_key(|mode| match mode {
                AgentRunMode::Interactive => 0,
                AgentRunMode::Unattended => 1,
                AgentRunMode::FullAccess => 2,
            })
            .unwrap_or_default()
    }

    fn agent_state_text(&self, priority: u8) -> &'static str {
        match priority {
            5 => self.language.error(),
            4 => self.language.waiting(),
            3 => self.language.running(),
            2 => self.language.completed(),
            _ => self.language.idle(),
        }
    }

    fn filtered_workspace_indices(&self) -> Vec<usize> {
        let query = self.workspace_search.text().trim().to_lowercase();
        self.state
            .workspaces
            .iter()
            .enumerate()
            .filter(|(_, workspace)| {
                query.is_empty()
                    || workspace.name.to_lowercase().contains(&query)
                    || workspace.description.to_lowercase().contains(&query)
                    || workspace
                        .cwd
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&query)
                    || workspace
                        .git_branch
                        .as_deref()
                        .is_some_and(|branch| branch.to_lowercase().contains(&query))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn workspace_search_bounds(&self) -> ui::Bounds {
        ui::Bounds {
            x: 8.0,
            y: 49.0,
            width: (self.sidebar_width() - 56.0).max(1.0),
            height: 30.0,
        }
    }

    fn titlebar_command_bounds(&self) -> ui::Bounds {
        let width = 456.0_f32.min((self.window_size.width as f32 - 420.0).max(180.0));
        ui::Bounds {
            x: ((self.window_size.width as f32 - width) / 2.0).max(8.0),
            y: 5.0,
            width,
            height: 30.0,
        }
    }

    fn command_palette_panel(&self, action_count: usize) -> ui::Bounds {
        let capacity = (((self.window_size.height as f32 - 150.0) / 25.0) as usize).clamp(3, 12);
        let visible = action_count.min(capacity);
        let command = self.titlebar_command_bounds();
        ui::Bounds {
            x: command.x,
            y: 38.0,
            width: command.width,
            height: (visible.max(1) as f32 * 25.0 + 16.0)
                .max(66.0)
                .min((self.window_size.height as f32 - 48.0).max(66.0)),
        }
    }

    fn prompt_panel(&self) -> ui::Bounds {
        ui::centered_panel(
            self.window_size.width as f32,
            self.window_size.height as f32,
            520.0,
            132.0,
            16.0,
        )
    }

    fn confirmation_panel(&self) -> ui::Bounds {
        ui::centered_panel(
            self.window_size.width as f32,
            self.window_size.height as f32,
            540.0,
            168.0,
            16.0,
        )
    }

    fn confirmation_buttons(&self) -> (ui::Bounds, ui::Bounds) {
        let panel = self.confirmation_panel();
        let width = 116.0;
        let height = 34.0;
        let top = panel.y + panel.height - height - 16.0;
        let confirm = ui::Bounds {
            x: panel.x + panel.width - width - 16.0,
            y: top,
            width,
            height,
        };
        let cancel = ui::Bounds {
            x: confirm.x - width - 10.0,
            y: top,
            width,
            height,
        };
        (cancel, confirm)
    }

    fn codex_trust_buttons(&self) -> (ui::Bounds, ui::Bounds, ui::Bounds) {
        let panel = self.confirmation_panel();
        let width = 150.0;
        let height = 34.0;
        let gap = 10.0;
        let top = panel.y + panel.height - height - 16.0;
        let trust = ui::Bounds {
            x: panel.x + panel.width - width - 16.0,
            y: top,
            width,
            height,
        };
        let proceed = ui::Bounds {
            x: trust.x - width - gap,
            y: top,
            width,
            height,
        };
        let cancel = ui::Bounds {
            x: proceed.x - 96.0 - gap,
            y: top,
            width: 96.0,
            height,
        };
        (cancel, proceed, trust)
    }

    fn prompt_input_bounds(&self) -> ui::Bounds {
        let panel = self.prompt_panel();
        ui::Bounds {
            x: panel.x + 18.0,
            y: panel.y + 48.0,
            width: panel.width - 36.0,
            height: 36.0,
        }
    }

    fn titlebar_buttons(&self) -> Vec<TitlebarButton> {
        let notification_label = unread_badge_label(
            self.state
                .notifications
                .iter()
                .filter(|notification| !notification.read)
                .count()
                + self.pending_agent_requests.len().unwrap_or_default(),
        )
        .unwrap_or_else(|| "●".to_owned());
        let actions = [
            (
                TitlebarAction::ToggleSidebar,
                "☰".to_owned(),
                self.sidebar_visible,
            ),
            (
                TitlebarAction::ToggleNotifications,
                notification_label,
                self.notifications_visible,
            ),
            (TitlebarAction::Minimize, "—".to_owned(), false),
            (TitlebarAction::ToggleMaximize, "□".to_owned(), false),
            (TitlebarAction::Close, "×".to_owned(), false),
        ];
        actions
            .into_iter()
            .zip(ui::titlebar_chrome_layout(self.window_size.width as f32))
            .map(|((action, label, active), bounds)| TitlebarButton {
                action,
                label,
                bounds,
                active,
            })
            .collect()
    }

    fn execute_titlebar_action(&mut self, action: TitlebarAction) {
        match action {
            TitlebarAction::ToggleSidebar => self.execute_action("sidebar.toggle"),
            TitlebarAction::ToggleNotifications => self.execute_action("notifications.toggle"),
            TitlebarAction::Minimize => {
                if let Some(renderer) = &self.renderer {
                    renderer.window().set_minimized(true);
                }
            }
            TitlebarAction::ToggleMaximize => {
                if let Some(renderer) = &self.renderer {
                    let window = renderer.window();
                    window.set_maximized(!window.is_maximized());
                }
            }
            TitlebarAction::Close => self.request_application_close(),
        }
    }

    fn empty_workspace_button(&self) -> ui::Bounds {
        let content_left = self.sidebar_width();
        let content_width = (self.window_size.width as f32 - content_left).max(1.0);
        let width = 220.0_f32.min((content_width - 32.0).max(1.0));
        ui::Bounds {
            x: content_left + (content_width - width) / 2.0,
            y: (self.window_size.height as f32 - 38.0) / 2.0 + 18.0,
            width,
            height: 38.0,
        }
    }

    fn notification_panel(&self) -> ui::Bounds {
        let width = 350.0_f32
            .min((self.window_size.width as f32 - 32.0).max(1.0))
            .max(220.0_f32.min(self.window_size.width as f32));
        ui::Bounds {
            x: (self.window_size.width as f32 - width - 16.0).max(0.0),
            y: 48.0,
            width,
            height: (self.window_size.height as f32 - 90.0).max(1.0),
        }
    }

    fn agent_approval_region(&self, pending_count: usize) -> ui::Bounds {
        let panel = self.notification_panel();
        if pending_count == 0 {
            return ui::Bounds {
                x: panel.x,
                y: panel.y + 42.0,
                width: panel.width,
                height: 0.0,
            };
        }
        let desired = 16.0 + pending_count.min(3) as f32 * 120.0;
        let maximum = (panel.height * 0.62).max(128.0);
        ui::Bounds {
            x: panel.x,
            y: panel.y + 42.0,
            width: panel.width,
            height: desired.min(maximum).min((panel.height - 50.0).max(1.0)),
        }
    }

    fn notification_list_panel(&self, pending_count: usize) -> ui::Bounds {
        let panel = self.notification_panel();
        if pending_count == 0 {
            return panel;
        }
        let approvals = self.agent_approval_region(pending_count);
        let y = approvals.y + approvals.height;
        ui::Bounds {
            x: panel.x,
            y,
            width: panel.width,
            height: (panel.y + panel.height - y).max(1.0),
        }
    }

    fn pending_agent_request_list(&self) -> Vec<AgentInteractionRequest> {
        self.pending_agent_requests.list().unwrap_or_default()
    }

    fn agent_request_source(&self, request: &AgentInteractionRequest) -> String {
        let Some(surface_id) = uuid::Uuid::parse_str(&request.surface_id).ok() else {
            return "Agent".into();
        };
        let agent = self
            .state
            .surfaces
            .get(&surface_id)
            .and_then(|surface| surface.agent.as_ref())
            .map(|agent| agent.name.as_str())
            .unwrap_or("Agent");
        let workspace = self.state.workspaces.iter().find(|workspace| {
            self.state
                .panes
                .values()
                .any(|pane| workspace.root.contains(pane.id) && pane.surfaces.contains(&surface_id))
        });
        workspace.map_or_else(
            || agent.to_owned(),
            |workspace| format!("{agent} · {}", workspace.name),
        )
    }

    fn agent_request_detail_layout(
        &self,
        request: &AgentInteractionRequest,
        scroll: usize,
    ) -> AgentRequestDetailLayout {
        agent_request_detail_layout(self.window_size, &request.message, scroll)
    }

    fn scroll_agent_request_detail(&mut self, delta: isize) {
        let Some((request_id, current)) = self.agent_request_detail.as_ref() else {
            return;
        };
        let Ok(Some(request)) = self.pending_agent_requests.get(request_id) else {
            return;
        };
        let layout = self.agent_request_detail_layout(&request, *current);
        let next = current.saturating_add_signed(delta).min(layout.max_scroll);
        if let Some((_, scroll)) = &mut self.agent_request_detail {
            *scroll = next;
        }
    }

    fn agent_request_detail_scroll_metrics(&self) -> Option<(usize, usize)> {
        let (request_id, scroll) = self.agent_request_detail.as_ref()?;
        let request = self.pending_agent_requests.get(request_id).ok()??;
        let layout = self.agent_request_detail_layout(&request, *scroll);
        Some((layout.visible_lines, layout.max_scroll))
    }

    fn agent_request_detail_page_size(&self) -> usize {
        self.agent_request_detail_scroll_metrics()
            .map_or(1, |(visible, _)| visible.saturating_sub(1).max(1))
    }

    fn agent_request_detail_max_scroll(&self) -> usize {
        self.agent_request_detail_scroll_metrics()
            .map_or(0, |(_, maximum)| maximum)
    }

    fn set_agent_request_detail_scroll(&mut self, scroll: usize) {
        let scroll = scroll.min(self.agent_request_detail_max_scroll());
        if let Some((_, current)) = &mut self.agent_request_detail {
            *current = scroll;
        }
    }

    fn agent_request_range(&self, count: usize) -> std::ops::Range<usize> {
        let region = self.agent_approval_region(count);
        let capacity = (((region.height - 16.0) / 120.0) as usize).max(1);
        ui::scrolled_range(count, self.agent_request_scroll, capacity)
    }

    fn move_agent_request_selection(&mut self, direction: isize) {
        let requests = self.pending_agent_request_list();
        if requests.is_empty() {
            self.agent_request_selection = 0;
            self.agent_request_scroll = 0;
            return;
        }
        self.agent_request_selection = self
            .agent_request_selection
            .saturating_add_signed(direction.signum())
            .min(requests.len() - 1);
        self.agent_request_action = self.agent_request_action.min(
            agent_approval_actions(&requests[self.agent_request_selection])
                .len()
                .saturating_sub(1),
        );
        let capacity = self.agent_request_range(requests.len()).len().max(1);
        if self.agent_request_selection < self.agent_request_scroll {
            self.agent_request_scroll = self.agent_request_selection;
        } else if self.agent_request_selection >= self.agent_request_scroll + capacity {
            self.agent_request_scroll = self.agent_request_selection + 1 - capacity;
        }
    }

    fn scroll_agent_requests(&mut self, direction: isize) {
        let count = self.pending_agent_requests.len().unwrap_or_default();
        let capacity = self.agent_request_range(count).len().max(1);
        self.agent_request_scroll = self
            .agent_request_scroll
            .saturating_add_signed(direction.signum())
            .min(count.saturating_sub(capacity));
        self.agent_request_selection = self.agent_request_selection.clamp(
            self.agent_request_scroll,
            (self.agent_request_scroll + capacity.saturating_sub(1)).min(count.saturating_sub(1)),
        );
        self.update_navigation_hover();
    }

    fn move_agent_request_action(&mut self, direction: isize) {
        let requests = self.pending_agent_request_list();
        let Some(request) = requests.get(self.agent_request_selection) else {
            self.agent_request_action = 0;
            return;
        };
        let count = agent_approval_actions(request).len();
        if count > 0 {
            self.agent_request_action = self
                .agent_request_action
                .saturating_add_signed(direction.signum())
                .min(count - 1);
        }
    }

    fn resolve_selected_agent_request(&mut self) {
        let requests = self.pending_agent_request_list();
        let Some(request) = requests.get(self.agent_request_selection) else {
            return;
        };
        let Some(action) = agent_approval_actions(request)
            .get(self.agent_request_action)
            .copied()
        else {
            return;
        };
        self.resolve_agent_request_from_ui(request.request_id.clone(), action);
    }

    fn resolve_agent_request_from_ui(&mut self, request_id: String, action: AgentApprovalAction) {
        let Ok(Some(request)) = self.pending_agent_requests.get(&request_id) else {
            return;
        };
        let resolution = agent_approval_resolution(&request, action);
        let result = self
            .pending_agent_requests
            .resolve(&request_id, resolution, unix_time_ms());
        if matches!(result, Ok(CompleteOutcome::Completed(_))) {
            self.remember_agent_approval_result(request_id.clone(), action);
            self.conclude_agent_request_state(&request);
        }
        let remaining = self.pending_agent_requests.len().unwrap_or_default();
        self.agent_request_selection = self
            .agent_request_selection
            .min(remaining.saturating_sub(1));
        self.agent_request_scroll = self.agent_request_scroll.min(remaining.saturating_sub(1));
        self.agent_request_action = 0;
        self.rebuild_scene();
    }

    fn remember_agent_approval_result(&mut self, request_id: String, action: AgentApprovalAction) {
        remember_bounded_approval_result(
            &mut self.agent_approval_results,
            &mut self.agent_approval_result_order,
            request_id,
            action,
            512,
        );
    }

    fn notification_range(&self) -> std::ops::Range<usize> {
        let pending_count = self.pending_agent_requests.len().unwrap_or_default();
        let capacity = ui::notification_rows(
            self.notification_list_panel(pending_count),
            self.state.notifications.len(),
        )
        .len()
        .min(12);
        ui::scrolled_range(
            self.state.notifications.len(),
            self.notification_scroll,
            capacity,
        )
    }

    fn scroll_notifications(&mut self, direction: isize) {
        let capacity = self.notification_range().len();
        let max_scroll = self.state.notifications.len().saturating_sub(capacity);
        self.notification_scroll = self
            .notification_scroll
            .saturating_add_signed(direction.signum())
            .min(max_scroll);
        self.notification_selection = self.notification_selection.clamp(
            self.notification_scroll,
            self.notification_scroll + capacity.saturating_sub(1),
        );
        self.navigation_hover = None;
        self.notification_hover_started = None;
        self.notification_marquee_line = 0;
        self.update_navigation_hover();
    }

    fn selected_notification_id(&self) -> Option<uuid::Uuid> {
        self.state
            .notifications
            .iter()
            .rev()
            .nth(self.notification_selection)
            .map(|notification| notification.id)
    }

    fn move_notification_selection(&mut self, direction: isize) {
        let count = self.state.notifications.len();
        if count == 0 {
            self.notification_selection = 0;
            return;
        }
        self.notification_selection = self
            .notification_selection
            .saturating_add_signed(direction.signum())
            .min(count - 1);
        let pending_count = self.pending_agent_requests.len().unwrap_or_default();
        let capacity = ui::notification_rows(self.notification_list_panel(pending_count), count)
            .len()
            .clamp(1, 12);
        if self.notification_selection < self.notification_scroll {
            self.notification_scroll = self.notification_selection;
        } else if self.notification_selection >= self.notification_scroll + capacity {
            self.notification_scroll = self.notification_selection + 1 - capacity;
        }
    }

    fn surface_tabs_layout(&self, pane: PaneId, rect: Rect) -> Vec<ui::SurfaceTab> {
        let Some(pane) = self.state.panes.get(&pane) else {
            return Vec::new();
        };
        let tab_font_size = self.terminal_font_size.min(12.0);
        let preferred_widths: Vec<_> = pane
            .surfaces
            .iter()
            .enumerate()
            .map(|(index, surface)| {
                self.state.surfaces.get(surface).map_or(64.0, |surface| {
                    ui::surface_tab_width_at(
                        &surface.title,
                        index,
                        tab_font_size * 0.6,
                        surface.agent.is_some(),
                    )
                })
            })
            .collect();
        ui::surface_tabs_layout(
            ui::Bounds {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            },
            &preferred_widths,
            pane.active_surface,
        )
    }

    fn surface_new_tab_bounds(&self, rect: Rect) -> ui::Bounds {
        ui::Bounds {
            x: (rect.x + rect.width - 28.0).max(rect.x),
            y: rect.y,
            width: 28.0_f32.min(rect.width),
            height: 25.0,
        }
    }

    fn surface_tab_overflow_bounds(&self, pane: PaneId, rect: Rect) -> Option<ui::Bounds> {
        let total = self.state.panes.get(&pane)?.surfaces.len();
        let visible = self.surface_tabs_layout(pane, rect).len();
        ui::surface_tab_overflow_bounds(
            ui::Bounds {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            },
            visible,
            total,
        )
    }

    fn tab_overflow_lines(&self, pane: PaneId) -> Vec<String> {
        let Some(pane) = self.state.panes.get(&pane) else {
            return Vec::new();
        };
        pane.surfaces
            .iter()
            .enumerate()
            .map(|(index, surface_id)| {
                let selected = index == pane.active_surface;
                let title = self
                    .state
                    .surfaces
                    .get(surface_id)
                    .map(|surface| ui::surface_tab_title(&surface.title, surface.agent.is_some()))
                    .unwrap_or_default();
                format!(
                    "{}{}  {title}",
                    if selected { "✓ " } else { "  " },
                    index + 1
                )
            })
            .collect()
    }

    fn tab_overflow_layout(
        &self,
        menu: TabOverflowMenu,
        lines: &[String],
    ) -> ui::TabOverflowMenuLayout {
        ui::tab_overflow_menu_layout(
            menu.anchor.0,
            menu.anchor.1,
            self.window_size.width as f32,
            self.window_size.height as f32,
            lines,
            menu.scroll_offset,
        )
    }

    fn sidebar_scrollbar_at_cursor(&self) -> Option<(ui::Bounds, ui::Bounds)> {
        if !self.sidebar_visible || self.pointer_over_overlay() {
            return None;
        }
        self.sidebar_layout()
            .scrollbar
            .filter(|(track, _)| track.contains(self.cursor_position.0, self.cursor_position.1))
    }

    fn begin_sidebar_scrollbar_action(&mut self) -> bool {
        let Some((track, thumb)) = self.sidebar_scrollbar_at_cursor() else {
            return false;
        };
        let pointer_y = self.cursor_position.1;
        // Clicking the rail centers the thumb, then permits a continuous drag.
        let grab_y = if thumb.contains(self.cursor_position.0, pointer_y) {
            pointer_y - thumb.y
        } else {
            thumb.height / 2.0
        };
        self.sidebar_scroll = scrollbar_scroll_for_pointer(
            track,
            thumb,
            self.sidebar_layout().max_scroll,
            pointer_y,
            grab_y,
        );
        self.sidebar_scrollbar_drag = Some(grab_y);
        self.rebuild_scene();
        true
    }

    fn update_sidebar_scrollbar_drag(&mut self) -> bool {
        let Some(grab_y) = self.sidebar_scrollbar_drag else {
            return false;
        };
        let layout = self.sidebar_layout();
        if self.sidebar_visible
            && !self.pointer_over_overlay()
            && let Some((track, thumb)) = layout.scrollbar
        {
            self.sidebar_scroll = scrollbar_scroll_for_pointer(
                track,
                thumb,
                layout.max_scroll,
                self.cursor_position.1,
                grab_y,
            );
            self.rebuild_scene();
        } else {
            self.sidebar_scrollbar_drag = None;
            self.mouse_pressed = false;
        }
        true
    }

    fn scroll_sidebar(&mut self, direction: isize) {
        let layout = self.sidebar_layout();
        self.sidebar_scroll = self
            .sidebar_scroll
            .saturating_add_signed(direction.signum())
            .min(layout.max_scroll);
        self.rebuild_scene();
    }

    fn scroll_palette(&mut self, direction: isize) {
        let registry = ActionRegistry::default();
        let len = self.palette_actions(&registry).len();
        let capacity = (((self.window_size.height as f32 - 150.0) / 25.0) as usize).clamp(3, 12);
        self.palette_scroll = self
            .palette_scroll
            .saturating_add_signed(direction)
            .min(len.saturating_sub(capacity));
        self.palette_hover = None;
        self.rebuild_scene();
    }

    fn ensure_palette_selection_visible(&mut self, count: usize) {
        let capacity = (((self.window_size.height as f32 - 150.0) / 25.0) as usize).clamp(3, 12);
        if self.palette_index < self.palette_scroll {
            self.palette_scroll = self.palette_index;
        } else if self.palette_index >= self.palette_scroll.saturating_add(capacity) {
            self.palette_scroll = self.palette_index + 1 - capacity;
        }
        self.palette_scroll = self.palette_scroll.min(count.saturating_sub(capacity));
    }

    fn scroll_tabs_at_cursor(&mut self, direction: isize) -> bool {
        let (x, y) = self.cursor_position;
        let Some((pane_id, _)) = self.pane_rects().into_iter().find(|(_, rect)| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + 25.0
        }) else {
            return false;
        };
        let Some(pane) = self.state.panes.get_mut(&pane_id) else {
            return true;
        };
        if pane.surfaces.is_empty() {
            return true;
        }
        pane.active_surface =
            ui::scroll_selection(pane.active_surface, pane.surfaces.len(), direction);
        if let Some(workspace) = self.state.workspaces.get_mut(self.state.active_workspace) {
            workspace.active_pane = pane_id;
        }
        self.resize_terminals();
        self.rebuild_scene();
        true
    }

    fn cell_width(&self) -> f32 {
        self.terminal_font_size * 0.6
    }

    fn cell_height(&self) -> f32 {
        self.terminal_font_size * 1.4
    }

    fn active_cursor_blinks(&self) -> bool {
        self.window_focused
            && self
                .active_terminal_id()
                .and_then(|id| self.terminals.get(&id))
                .and_then(|runtime| runtime.snapshot.cursor)
                .is_some_and(|cursor| cursor.visible && cursor.blinking)
    }

    fn reset_cursor_blink(&mut self) {
        let changed = !self.cursor_blink_visible;
        self.cursor_blink_visible = true;
        self.cursor_blink_last_toggle = std::time::Instant::now();
        if changed {
            self.rebuild_scene();
        }
    }

    fn update_cursor_blink(&mut self, now: std::time::Instant) -> Option<std::time::Duration> {
        if !self.active_cursor_blinks() {
            if !self.cursor_blink_visible {
                self.cursor_blink_visible = true;
                self.rebuild_scene();
            }
            self.cursor_blink_last_toggle = now;
            return None;
        }
        if advance_cursor_blink(
            &mut self.cursor_blink_visible,
            &mut self.cursor_blink_last_toggle,
            now,
        ) {
            self.rebuild_scene();
        }
        Some(
            CURSOR_BLINK_INTERVAL
                .saturating_sub(now.saturating_duration_since(self.cursor_blink_last_toggle)),
        )
    }

    fn adjust_font_size(&mut self, delta: f32) {
        self.terminal_font_size = (self.terminal_font_size + delta).clamp(8.0, 40.0);
        self.resize_terminals();
        self.rebuild_scene();
    }

    fn reset_font_size(&mut self) {
        self.terminal_font_size = self.config.terminal.font_size.clamp(8.0, 40.0);
        self.resize_terminals();
        self.rebuild_scene();
    }

    fn search_active(&mut self, direction: isize) {
        if let Some(runtime) = self
            .active_terminal_id()
            .and_then(|id| self.terminals.get(&id))
        {
            let _ = runtime.session.send(SessionCommand::Search {
                query: self.search_editor.text().to_owned(),
                direction,
            });
            return;
        }
        let Some(surface) = self.active_surface_id() else {
            return;
        };
        let Some(backend) = self.surface_backends.get(&surface) else {
            return;
        };
        let offsets = document_search_offsets(backend.as_ref(), self.search_editor.text());
        if offsets.is_empty() {
            self.search_result = Some(SearchResult {
                query: self.search_editor.text().to_owned(),
                current: 0,
                total: 0,
            });
        } else {
            let same_query = self.search_result.as_ref().is_some_and(|result| {
                result.query == self.search_editor.text() && result.total == offsets.len()
            });
            let index = if !same_query {
                if direction < 0 { offsets.len() - 1 } else { 0 }
            } else {
                let current = self
                    .search_result
                    .as_ref()
                    .map(|result| result.current)
                    .unwrap_or(1);
                if direction < 0 {
                    (current + offsets.len() - 2) % offsets.len()
                } else {
                    current.wrapping_rem(offsets.len())
                }
            };
            self.document_scroll.insert(surface, offsets[index]);
            self.search_result = Some(SearchResult {
                query: self.search_editor.text().to_owned(),
                current: index + 1,
                total: offsets.len(),
            });
        }
        self.rebuild_scene();
    }

    fn clear_scrollback(&mut self) {
        if let Some(runtime) = self
            .active_terminal_id()
            .and_then(|id| self.terminals.get(&id))
        {
            let _ = runtime.session.send(SessionCommand::ClearScrollback);
        }
    }

    fn toggle_pane_zoom(&mut self) {
        let Some(pane) = self
            .state
            .active_workspace()
            .map(|workspace| workspace.active_pane)
        else {
            return;
        };
        if let Some(pane) = self.state.panes.get_mut(&pane) {
            pane.zoomed = !pane.zoomed;
        }
        self.resize_terminals();
        self.rebuild_scene();
    }

    fn activate_surface_number(&mut self, index: usize) {
        let Some(pane) = self
            .state
            .active_workspace()
            .map(|workspace| workspace.active_pane)
        else {
            return;
        };
        if let Some(pane) = self.state.panes.get_mut(&pane)
            && index < pane.surfaces.len()
        {
            pane.active_surface = index;
            self.resize_terminals();
            self.rebuild_scene();
        }
    }

    fn new_workspace(&mut self) {
        let name = format!(
            "{} {}",
            self.language.workspace_default(),
            self.state.workspaces.len() + 1
        );
        let process_cwd = env::current_dir().unwrap_or_default();
        let cwd = select_new_workspace_cwd(
            self.config.workspace.inherit_cwd,
            self.state
                .active_workspace()
                .map(|workspace| workspace.cwd.as_path()),
            self.config.workspace.default_cwd.as_deref(),
            &process_cwd,
        );
        if self
            .state
            .execute(AppCommand::Workspace(WorkspaceCommand::Create {
                name,
                cwd,
            }))
            .is_err()
        {
            return;
        }
        let missing: Vec<_> = self
            .state
            .surfaces
            .values()
            .filter_map(|surface| match surface.kind {
                SurfaceKind::Terminal(id) if !self.terminals.contains_key(&id) => Some(id),
                _ => None,
            })
            .collect();
        for terminal in missing {
            if let Err(error) = self.spawn_terminal(terminal) {
                tracing::error!(%error, "spawning workspace terminal failed");
                self.report_terminal_notice(
                    terminal,
                    TerminalNotice::SpawnFailed(error.to_string()),
                );
            }
        }
        self.refresh_workspace_metadata();
        self.resize_terminals();
        self.rebuild_scene();
    }

    fn open_workspace_prompt(&mut self, kind: PromptKind) {
        let Some(workspace) = self.state.active_workspace() else {
            return;
        };
        let input = match kind {
            PromptKind::RenameWorkspace => workspace.name.clone(),
            PromptKind::DescribeWorkspace => workspace.description.clone(),
            PromptKind::ChangeWorkspaceDirectory => workspace.cwd.display().to_string(),
            PromptKind::FindDirectory => String::new(),
            PromptKind::FilterLogs => String::new(),
        };
        self.prompt = Some(TextPrompt {
            kind,
            editor: TextEditor::new(input),
            error: None,
        });
        self.rebuild_scene();
    }

    fn open_log_filter_prompt(&mut self) {
        let Some(surface) = self.active_surface_id() else {
            return;
        };
        let Some(logs) = self
            .surface_backends
            .get(&surface)
            .and_then(|backend| backend.as_any().downcast_ref::<LogSurface>())
        else {
            return;
        };
        self.prompt = Some(TextPrompt {
            kind: PromptKind::FilterLogs,
            editor: TextEditor::new(format_log_filter(logs.filter())),
            error: None,
        });
        self.rebuild_scene();
    }

    fn open_full_access_prompt(&mut self, agent: ManagedAgent) {
        if self.state.active_workspace().is_none() {
            return;
        }
        self.prompt = None;
        self.close_confirmation = Some(CloseConfirmation {
            target: CloseTarget::FullAccess(agent),
            title: self.language.full_access_confirmation_title(agent.name()),
            message: self.language.full_access_confirmation_message().into(),
        });
        self.rebuild_scene();
    }

    fn launch_unattended_codex(&mut self) {
        let Some(workspace) = self.state.active_workspace() else {
            return;
        };
        let project_root =
            std::fs::canonicalize(&workspace.cwd).unwrap_or_else(|_| workspace.cwd.clone());
        match wmcterm_codex::project_trust_state(&project_root) {
            Ok(
                wmcterm_codex::ProjectTrustState::NotApplicable
                | wmcterm_codex::ProjectTrustState::Trusted,
            ) => self.start_unattended_codex(),
            Ok(wmcterm_codex::ProjectTrustState::Untrusted) => {
                self.codex_trust_confirmation = Some(CodexTrustConfirmation {
                    project_root,
                    error: None,
                });
                self.rebuild_scene();
            }
            Err(error) => {
                self.codex_trust_confirmation = Some(CodexTrustConfirmation {
                    project_root,
                    error: Some(error.to_string()),
                });
                self.rebuild_scene();
            }
        }
    }

    fn start_unattended_codex(&mut self) {
        self.new_surface_command(Some(managed_agent_command(
            AgentRunMode::Unattended,
            "codex",
        )));
    }

    fn trust_and_start_unattended_codex(&mut self) {
        let Some(confirmation) = self.codex_trust_confirmation.take() else {
            return;
        };
        match wmcterm_codex::trust_project(&confirmation.project_root) {
            Ok(()) => self.start_unattended_codex(),
            Err(error) => {
                self.codex_trust_confirmation = Some(CodexTrustConfirmation {
                    error: Some(error.to_string()),
                    ..confirmation
                });
                self.rebuild_scene();
            }
        }
    }

    fn continue_untrusted_codex(&mut self) {
        if self.codex_trust_confirmation.take().is_some() {
            self.start_unattended_codex();
        }
    }

    fn submit_prompt(&mut self) {
        let Some(prompt) = self.prompt.take() else {
            return;
        };
        let Some(workspace_id) = self.state.active_workspace().map(|workspace| workspace.id) else {
            return;
        };
        let input = prompt.editor.text().trim().to_owned();
        let changes_directory = matches!(prompt.kind, PromptKind::ChangeWorkspaceDirectory);
        if matches!(prompt.kind, PromptKind::RenameWorkspace) && input.is_empty() {
            self.prompt = Some(TextPrompt {
                error: Some(self.language.name_required().into()),
                ..prompt
            });
            self.rebuild_scene();
            return;
        }
        let command = match prompt.kind {
            PromptKind::RenameWorkspace => AppCommand::Workspace(WorkspaceCommand::Rename {
                id: workspace_id,
                name: input,
            }),
            PromptKind::DescribeWorkspace => AppCommand::Workspace(WorkspaceCommand::Describe {
                id: workspace_id,
                description: input,
            }),
            PromptKind::ChangeWorkspaceDirectory => {
                let current = self
                    .state
                    .active_workspace()
                    .map(|workspace| workspace.cwd.as_path())
                    .unwrap_or_else(|| std::path::Path::new("."));
                let cwd = match resolve_existing_directory(&input, current) {
                    Ok(cwd) => cwd,
                    Err(error) => {
                        self.prompt = Some(TextPrompt {
                            error: Some(self.language.invalid_workspace_directory(&error)),
                            ..prompt
                        });
                        self.rebuild_scene();
                        return;
                    }
                };
                AppCommand::Workspace(WorkspaceCommand::ChangeDirectory {
                    id: workspace_id,
                    cwd,
                })
            }
            PromptKind::FindDirectory => {
                if !input.is_empty() {
                    self.new_surface_command(Some(format!(
                        "rg --line-number --color=always -- {} .",
                        shell_quote_command(&input)
                    )));
                }
                return;
            }
            PromptKind::FilterLogs => {
                let Some(surface) = self.active_surface_id() else {
                    return;
                };
                if let Some(logs) = self
                    .surface_backends
                    .get_mut(&surface)
                    .and_then(|backend| backend.as_any_mut().downcast_mut::<LogSurface>())
                {
                    logs.set_filter(parse_log_filter(&input));
                    self.document_scroll.insert(surface, 0);
                }
                self.rebuild_scene();
                return;
            }
        };
        let _ = self.state.execute(command);
        if changes_directory {
            self.refresh_workspace_metadata();
        }
        self.rebuild_scene();
    }

    fn move_workspace(&mut self, offset: isize) {
        let Some(id) = self.state.active_workspace().map(|workspace| workspace.id) else {
            return;
        };
        let _ = self
            .state
            .execute(AppCommand::Workspace(WorkspaceCommand::Move { id, offset }));
        self.rebuild_scene();
    }

    fn refresh_workspace_metadata(&mut self) {
        if self.metadata_refresh_in_flight {
            return;
        }
        self.metadata_refresh_in_flight = true;
        let workspaces: Vec<_> = self
            .state
            .workspaces
            .iter()
            .map(|workspace| (workspace.id, workspace.cwd.clone()))
            .collect();
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let metadata = workspaces
                .into_iter()
                .map(|(id, cwd)| WorkspaceMetadata {
                    id,
                    git_branch: ProcessCommand::new("git")
                        .args(["-C"])
                        .arg(&cwd)
                        .args(["branch", "--show-current"])
                        .output()
                        .ok()
                        .filter(|output| output.status.success())
                        .and_then(|output| String::from_utf8(output.stdout).ok())
                        .map(|branch| branch.trim().to_owned())
                        .filter(|branch| !branch.is_empty()),
                    listening_ports: listening_ports_for(&cwd),
                })
                .collect();
            let _ = proxy.send_event(UserEvent::WorkspaceMetadata(metadata));
        });
    }

    fn terminal_activity_count(&self, terminal: TerminalId) -> usize {
        let processes = self.terminal_processes.get(&terminal).map_or(0, Vec::len);
        if processes > 0 {
            return processes;
        }
        let agent_active = self.state.surfaces.values().any(|surface| {
            matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal)
                && surface.agent.as_ref().is_some_and(|agent| {
                    matches!(
                        agent.state,
                        wmcterm_core::AgentState::Running
                            | wmcterm_core::AgentState::WaitingForInput
                            | wmcterm_core::AgentState::WaitingForPermission
                    )
                })
        });
        usize::from(agent_active)
    }

    fn confirm_close(&mut self) {
        let Some(confirmation) = self.close_confirmation.take() else {
            return;
        };
        match confirmation.target {
            CloseTarget::Surfaces(surfaces) => {
                for surface in surfaces {
                    self.remove_surface_by_id(surface);
                }
                self.resize_terminals();
            }
            CloseTarget::Workspace(workspace) => {
                self.remove_workspace_by_id(workspace);
            }
            CloseTarget::Application => self.quit_requested = true,
            CloseTarget::FullAccess(agent) => {
                self.new_surface_command(Some(agent.full_access_command().into()));
            }
        }
        self.update_navigation_hover();
        self.update_cursor_icon();
        self.rebuild_scene();
    }

    fn request_application_close(&mut self) {
        let terminal_count = self.terminals.len();
        let process_count = self
            .terminals
            .keys()
            .map(|terminal| self.terminal_activity_count(*terminal))
            .sum();
        let pending_request_count = self.pending_agent_requests.len().unwrap_or_default();
        if application_requires_close_confirmation(process_count, pending_request_count) {
            self.close_confirmation = Some(CloseConfirmation {
                target: CloseTarget::Application,
                title: self.language.close_application_title().to_owned(),
                message: self.language.close_application_warning(
                    terminal_count,
                    process_count,
                    pending_request_count,
                ),
            });
            self.update_navigation_hover();
            self.update_cursor_icon();
            self.rebuild_scene();
        } else {
            self.quit_requested = true;
        }
    }

    fn request_close_workspace(&mut self) {
        let Some(workspace) = self.state.active_workspace().map(|workspace| workspace.id) else {
            return;
        };
        let _ = self.request_close_workspace_by_id(workspace);
    }

    fn request_close_workspace_by_id(&mut self, workspace_id: uuid::Uuid) -> Result<bool, String> {
        let workspace = self
            .state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .ok_or_else(|| "workspace not found".to_owned())?;
        let mut pane_ids = Vec::new();
        workspace.root.leaves(&mut pane_ids);
        let terminal_ids: Vec<_> = pane_ids
            .iter()
            .filter_map(|pane| self.state.panes.get(pane))
            .flat_map(|pane| pane.surfaces.iter())
            .filter_map(|surface| self.state.surfaces.get(surface))
            .filter_map(|surface| match surface.kind {
                SurfaceKind::Terminal(id) => Some(id),
                _ => None,
            })
            .collect();
        let process_count = terminal_ids
            .iter()
            .map(|terminal| self.terminal_activity_count(*terminal))
            .sum();
        if workspace_requires_close_confirmation(terminal_ids.len(), process_count) {
            self.close_confirmation = Some(CloseConfirmation {
                target: CloseTarget::Workspace(workspace.id),
                title: self.language.close_workspace_title().to_owned(),
                message: self
                    .language
                    .close_workspace_warning(terminal_ids.len(), process_count),
            });
            self.update_navigation_hover();
            self.update_cursor_icon();
            self.rebuild_scene();
            return Ok(false);
        }
        Ok(self.remove_workspace_by_id(workspace_id))
    }

    fn remove_workspace_by_id(&mut self, workspace_id: uuid::Uuid) -> bool {
        let Some(workspace) = self
            .state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .cloned()
        else {
            return false;
        };
        let previously_active = self.state.active_workspace().map(|workspace| workspace.id);
        let mut pane_ids = Vec::new();
        workspace.root.leaves(&mut pane_ids);
        let surface_ids: Vec<_> = pane_ids
            .iter()
            .filter_map(|pane| self.state.panes.get(pane))
            .flat_map(|pane| pane.surfaces.iter().copied())
            .collect();
        let terminal_ids: Vec<_> = surface_ids
            .iter()
            .filter_map(|surface| self.state.surfaces.get(surface))
            .filter_map(|surface| match surface.kind {
                SurfaceKind::Terminal(id) => Some(id),
                _ => None,
            })
            .collect();
        if self
            .state
            .execute(AppCommand::Workspace(WorkspaceCommand::Close {
                id: workspace_id,
            }))
            .is_ok()
        {
            if previously_active != Some(workspace_id)
                && let Some(active) = previously_active
                && let Some(index) = self
                    .state
                    .workspaces
                    .iter()
                    .position(|workspace| workspace.id == active)
            {
                self.state.active_workspace = index;
            }
            for surface in surface_ids {
                self.surface_backends.remove(&surface);
                self.document_scroll.remove(&surface);
                self.agent_sessions.remove(&surface);
                self.claude_subagents
                    .retain(|_, subagent| subagent.surface != surface);
                clear_codex_subagent_surface(&mut self.codex_subagents, surface);
            }
            for terminal in terminal_ids {
                self.terminals.remove(&terminal);
                self.commands.remove(&terminal);
                self.terminal_notices.remove(&terminal);
                self.terminal_processes.remove(&terminal);
                self.last_bell_notifications.remove(&terminal);
                self.foreground_agents.remove(&terminal);
                self.automatic_agent_status.remove(&terminal);
            }
            self.resize_terminals();
            self.rebuild_scene();
            return true;
        }
        false
    }

    fn new_surface_command(&mut self, command: Option<String>) -> Option<wmcterm_core::SurfaceId> {
        let pane_id = self
            .state
            .active_workspace()
            .map(|workspace| workspace.active_pane)?;
        let surface = wmcterm_core::Surface::terminal("Shell");
        let terminal = match surface.kind {
            SurfaceKind::Terminal(id) => id,
            _ => return None,
        };
        let surface_id = surface.id;
        self.state.surfaces.insert(surface_id, surface);
        if let Some(pane) = self.state.panes.get_mut(&pane_id) {
            pane.surfaces.push(surface_id);
            pane.active_surface = pane.surfaces.len() - 1;
        }
        if let Err(error) = self.spawn_terminal_command(terminal, command) {
            tracing::error!(%error, "spawning surface failed");
            self.report_terminal_notice(terminal, TerminalNotice::SpawnFailed(error.to_string()));
        }
        self.resize_terminals();
        self.rebuild_scene();
        Some(surface_id)
    }

    fn rpc_target_pane(&self, params: &Value) -> Result<(usize, PaneId), String> {
        rpc_target_pane_in_state(&self.state, params)
    }

    fn rpc_create_surface(
        &mut self,
        workspace_index: usize,
        pane_id: PaneId,
        options: TerminalCreateOptions,
        focus: bool,
    ) -> Result<Value, String> {
        let workspace_id = self.state.workspaces[workspace_index].id;
        let mut surface = wmcterm_core::Surface::terminal(
            options
                .title
                .as_deref()
                .unwrap_or_else(|| self.language.shell()),
        );
        let SurfaceKind::Terminal(terminal_id) = surface.kind else {
            return Err("terminal surface creation failed".into());
        };
        let surface_id = surface.id;
        if let Some(title) = &options.title {
            surface.title.clone_from(title);
        }
        self.state.surfaces.insert(surface_id, surface);
        self.state
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| "target pane not found".to_owned())?
            .surfaces
            .push(surface_id);
        if let Err(error) = self.spawn_terminal_with_options(terminal_id, options) {
            self.state.surfaces.remove(&surface_id);
            if let Some(pane) = self.state.panes.get_mut(&pane_id) {
                pane.surfaces.retain(|id| *id != surface_id);
                pane.active_surface = pane
                    .active_surface
                    .min(pane.surfaces.len().saturating_sub(1));
            }
            return Err(error.to_string());
        }
        focus_created_surface(
            &mut self.state,
            workspace_index,
            pane_id,
            Some(surface_id),
            focus,
        );
        self.resize_terminals();
        self.rebuild_scene();
        Ok(json!({
            "workspace_id": workspace_id,
            "pane_id": pane_id,
            "surface_id": surface_id,
            "terminal_id": terminal_id,
        }))
    }

    fn rpc_split_pane(
        &mut self,
        workspace_index: usize,
        target_pane: PaneId,
        placement: SplitPlacement,
        options: TerminalCreateOptions,
        focus: bool,
    ) -> Result<Value, String> {
        let workspace_id = self.state.workspaces[workspace_index].id;
        let pane_id = PaneId::new_v4();
        let mut surface = wmcterm_core::Surface::terminal(
            options
                .title
                .as_deref()
                .unwrap_or_else(|| self.language.shell()),
        );
        let SurfaceKind::Terminal(terminal_id) = surface.kind else {
            return Err("terminal surface creation failed".into());
        };
        let surface_id = surface.id;
        if let Some(title) = &options.title {
            surface.title.clone_from(title);
        }
        self.state.workspaces[workspace_index]
            .root
            .split(target_pane, pane_id, placement.axis, 0.5)
            .map_err(|error| error.to_string())?;
        if placement.new_first {
            place_new_split_first(
                &mut self.state.workspaces[workspace_index].root,
                target_pane,
                pane_id,
            );
        }
        self.state.surfaces.insert(surface_id, surface);
        self.state.panes.insert(
            pane_id,
            Pane {
                id: pane_id,
                surfaces: vec![surface_id],
                active_surface: 0,
                zoomed: false,
            },
        );
        if let Err(error) = self.spawn_terminal_with_options(terminal_id, options) {
            if let Ok(root) = self.state.workspaces[workspace_index]
                .root
                .clone()
                .remove(pane_id)
            {
                self.state.workspaces[workspace_index].root = root;
            }
            self.state.panes.remove(&pane_id);
            self.state.surfaces.remove(&surface_id);
            return Err(error.to_string());
        }
        focus_created_surface(
            &mut self.state,
            workspace_index,
            pane_id,
            Some(surface_id),
            focus,
        );
        self.resize_terminals();
        self.rebuild_scene();
        Ok(json!({
            "workspace_id": workspace_id,
            "pane_id": pane_id,
            "surface_id": surface_id,
            "terminal_id": terminal_id,
        }))
    }

    fn rpc_open_text_surface(
        &mut self,
        workspace_index: usize,
        pane_id: PaneId,
        path: &std::path::Path,
        logs: bool,
        focus: bool,
    ) -> Result<Value, String> {
        let workspace_id = self.state.workspaces[workspace_index].id;
        let backend: Box<dyn SurfaceBackend> = if logs {
            Box::new(LogSurface::open(path, 10_000).map_err(|error| error.to_string())?)
        } else {
            Box::new(MarkdownSurface::open(path).map_err(|error| error.to_string())?)
        };
        let surface_id = SurfaceId::new_v4();
        let kind_id = uuid::Uuid::new_v4();
        let surface = wmcterm_core::Surface {
            id: surface_id,
            title: path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("Document")
                .to_owned(),
            kind: if logs {
                SurfaceKind::Logs(kind_id)
            } else {
                SurfaceKind::Markdown(kind_id)
            },
            attention: AttentionState::None,
            agent: None,
            resource: Some(path.display().to_string()),
        };
        self.state.surfaces.insert(surface_id, surface);
        self.surface_backends.insert(surface_id, backend);
        self.state
            .panes
            .get_mut(&pane_id)
            .ok_or_else(|| "target pane not found".to_owned())?
            .surfaces
            .push(surface_id);
        focus_created_surface(
            &mut self.state,
            workspace_index,
            pane_id,
            Some(surface_id),
            focus,
        );
        self.resize_terminals();
        self.rebuild_scene();
        Ok(json!({
            "workspace_id": workspace_id,
            "pane_id": pane_id,
            "surface_id": surface_id,
        }))
    }

    fn request_close_active_surface(&mut self) -> bool {
        let Some(surface_id) = self.active_surface_id() else {
            return false;
        };
        self.request_close_surfaces(vec![surface_id])
    }

    fn request_close_surfaces(&mut self, surfaces: Vec<SurfaceId>) -> bool {
        let terminals: Vec<_> = surfaces
            .iter()
            .filter_map(|surface| self.state.surfaces.get(surface))
            .filter_map(|surface| match surface.kind {
                SurfaceKind::Terminal(terminal) => Some(terminal),
                _ => None,
            })
            .collect();
        let process_count = terminals
            .iter()
            .map(|terminal| self.terminal_activity_count(*terminal))
            .sum();
        if process_count == 0 {
            for surface in surfaces {
                self.remove_surface_by_id(surface);
            }
            self.resize_terminals();
            self.rebuild_scene();
            return true;
        }
        self.close_confirmation = Some(CloseConfirmation {
            target: CloseTarget::Surfaces(surfaces),
            title: self.language.close_terminal_title().to_owned(),
            message: self
                .language
                .close_terminal_warning(terminals.len(), process_count),
        });
        self.update_navigation_hover();
        self.update_cursor_icon();
        self.rebuild_scene();
        false
    }

    /// Remove one surface without changing the user's active workspace, pane,
    /// or tab merely to address the target.
    fn remove_surface_by_id(&mut self, surface_id: SurfaceId) -> bool {
        let Some(pane_id) = self
            .state
            .panes
            .iter()
            .find(|(_, pane)| pane.surfaces.contains(&surface_id))
            .map(|(id, _)| *id)
        else {
            return false;
        };
        let Some(pane) = self.state.panes.get_mut(&pane_id) else {
            return false;
        };
        let Some(index) = pane.surfaces.iter().position(|id| *id == surface_id) else {
            return false;
        };
        pane.surfaces.remove(index);
        if index < pane.active_surface {
            pane.active_surface -= 1;
        } else {
            pane.active_surface = pane
                .active_surface
                .min(pane.surfaces.len().saturating_sub(1));
        }
        self.cleanup_surface_runtime(surface_id);
        true
    }

    fn cleanup_surface_runtime(&mut self, surface_id: SurfaceId) {
        self.surface_backends.remove(&surface_id);
        self.document_scroll.remove(&surface_id);
        self.agent_sessions.remove(&surface_id);
        self.claude_subagents
            .retain(|_, subagent| subagent.surface != surface_id);
        clear_codex_subagent_surface(&mut self.codex_subagents, surface_id);
        if let Some(surface) = self.state.surfaces.remove(&surface_id)
            && let SurfaceKind::Terminal(terminal) = surface.kind
        {
            self.terminals.remove(&terminal);
            self.commands.remove(&terminal);
            self.terminal_notices.remove(&terminal);
            self.terminal_processes.remove(&terminal);
            self.last_bell_notifications.remove(&terminal);
            self.foreground_agents.remove(&terminal);
            self.automatic_agent_status.remove(&terminal);
        }
    }

    /// Remove one pane directly. Target lookup never mutates UI focus.
    fn remove_pane_by_id(&mut self, pane_id: PaneId) -> bool {
        let Some(workspace_index) = self
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.root.contains(pane_id))
        else {
            return false;
        };
        let mut leaves = Vec::new();
        self.state.workspaces[workspace_index]
            .root
            .leaves(&mut leaves);
        if leaves.len() <= 1 {
            return false;
        }
        let next = leaves
            .iter()
            .copied()
            .find(|candidate| *candidate != pane_id)
            .expect("multiple leaves include a survivor");
        let Ok(root) = self.state.workspaces[workspace_index]
            .root
            .clone()
            .remove(pane_id)
        else {
            return false;
        };
        self.state.workspaces[workspace_index].root = root;
        if self.state.workspaces[workspace_index].active_pane == pane_id {
            self.state.workspaces[workspace_index].active_pane = next;
        }
        if let Some(pane) = self.state.panes.remove(&pane_id) {
            for surface in pane.surfaces {
                self.cleanup_surface_runtime(surface);
            }
        }
        true
    }

    fn close_active_surface_now(&mut self) -> bool {
        let Some(pane_id) = self
            .state
            .active_workspace()
            .map(|workspace| workspace.active_pane)
        else {
            return false;
        };
        let Some(pane) = self.state.panes.get_mut(&pane_id) else {
            return false;
        };
        if pane.surfaces.is_empty() {
            return false;
        }
        let surface_id = pane.surfaces.remove(pane.active_surface);
        pane.active_surface = pane
            .active_surface
            .min(pane.surfaces.len().saturating_sub(1));
        self.surface_backends.remove(&surface_id);
        self.document_scroll.remove(&surface_id);
        self.agent_sessions.remove(&surface_id);
        self.claude_subagents
            .retain(|_, subagent| subagent.surface != surface_id);
        clear_codex_subagent_surface(&mut self.codex_subagents, surface_id);
        if let Some(surface) = self.state.surfaces.remove(&surface_id)
            && let SurfaceKind::Terminal(terminal) = surface.kind
        {
            self.terminals.remove(&terminal);
            self.commands.remove(&terminal);
            self.terminal_notices.remove(&terminal);
            self.terminal_processes.remove(&terminal);
            self.last_bell_notifications.remove(&terminal);
            self.foreground_agents.remove(&terminal);
            self.automatic_agent_status.remove(&terminal);
        }
        self.resize_terminals();
        self.rebuild_scene();
        true
    }

    fn handle_exited_terminal_key(&mut self, event: &KeyEvent) -> bool {
        let Some(terminal) = self.active_terminal_id().filter(|terminal| {
            matches!(
                self.terminal_notices.get(terminal),
                Some(TerminalNotice::Exited(_))
            )
        }) else {
            return false;
        };
        let reconnect = matches!(event.logical_key, Key::Named(NamedKey::Enter))
            && !self.modifiers.control_key()
            && !self.modifiers.shift_key()
            && !self.modifiers.alt_key()
            && !self.modifiers.super_key();
        let close = matches!(&event.logical_key, Key::Character(value) if value.eq_ignore_ascii_case("d"))
            && self.modifiers.control_key()
            && !self.modifiers.shift_key()
            && !self.modifiers.alt_key()
            && !self.modifiers.super_key();
        if close {
            self.close_active_surface_now();
            return true;
        }
        if !reconnect {
            return false;
        }
        self.terminals.remove(&terminal);
        match self.spawn_terminal(terminal) {
            Ok(()) => {
                if let Some(surface) = self.state.surfaces.values_mut().find(
                    |surface| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal),
                ) {
                    surface.attention = AttentionState::None;
                }
                if let Some(index) = self.workspace_for_terminal(terminal)
                    && let Some(workspace) = self.state.workspaces.get_mut(index)
                {
                    workspace.status = wmcterm_core::WorkspaceStatus::Idle;
                }
                self.resize_terminals();
            }
            Err(error) => {
                tracing::error!(terminal=%terminal, %error, "reconnecting terminal failed");
                self.report_terminal_notice(
                    terminal,
                    TerminalNotice::SpawnFailed(error.to_string()),
                );
            }
        }
        self.rebuild_scene();
        true
    }

    fn cycle_surface(&mut self, direction: isize) {
        let Some(pane_id) = self
            .state
            .active_workspace()
            .map(|workspace| workspace.active_pane)
        else {
            return;
        };
        if let Some(pane) = self.state.panes.get_mut(&pane_id)
            && !pane.surfaces.is_empty()
        {
            pane.active_surface =
                pane.active_surface.wrapping_add_signed(direction) % pane.surfaces.len();
        }
        self.resize_terminals();
        self.rebuild_scene();
    }

    fn activate_surface_id(&mut self, surface: SurfaceId) -> Result<()> {
        let pane = self
            .state
            .panes
            .values()
            .find(|pane| pane.surfaces.contains(&surface))
            .map(|pane| pane.id)
            .context("surface does not belong to a pane")?;
        let workspace_index = self
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.root.contains(pane))
            .context("surface does not belong to a workspace")?;
        self.state.active_workspace = workspace_index;
        self.state
            .execute(AppCommand::Pane(PaneCommand::Focus { id: pane }))?;
        self.state.execute(AppCommand::Surface(
            wmcterm_core::SurfaceCommand::Activate { pane, surface },
        ))?;
        self.resize_terminals();
        self.rebuild_scene();
        Ok(())
    }

    fn focus_pane_direction(&mut self, direction: (f32, f32)) {
        let Some(active) = self
            .state
            .active_workspace()
            .map(|workspace| workspace.active_pane)
        else {
            return;
        };
        let rects = self.pane_rects();
        let Some((_, current)) = rects.iter().find(|(pane, _)| *pane == active) else {
            return;
        };
        let center = (
            current.x + current.width / 2.0,
            current.y + current.height / 2.0,
        );
        let target = rects
            .iter()
            .filter(|(pane, _)| *pane != active)
            .filter_map(|(pane, rect)| {
                let candidate = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
                let delta = (candidate.0 - center.0, candidate.1 - center.1);
                let forward = delta.0 * direction.0 + delta.1 * direction.1;
                (forward > 0.0).then_some((*pane, forward + delta.0.abs() + delta.1.abs()))
            })
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|item| item.0);
        if let Some(target) = target
            && let Some(workspace) = self.state.workspaces.get_mut(self.state.active_workspace)
        {
            workspace.active_pane = target;
            self.resize_terminals();
            self.rebuild_scene();
        }
    }

    fn close_active_pane(&mut self) {
        let Some(workspace_index) = self
            .state
            .workspaces
            .get(self.state.active_workspace)
            .map(|_| self.state.active_workspace)
        else {
            return;
        };
        let pane_id = self.state.workspaces[workspace_index].active_pane;
        let mut leaves = Vec::new();
        self.state.workspaces[workspace_index]
            .root
            .leaves(&mut leaves);
        if leaves.len() <= 1 {
            return;
        }
        let next = leaves
            .iter()
            .copied()
            .find(|id| *id != pane_id)
            .unwrap_or(pane_id);
        if let Ok(root) = self.state.workspaces[workspace_index]
            .root
            .clone()
            .remove(pane_id)
        {
            self.state.workspaces[workspace_index].root = root;
            self.state.workspaces[workspace_index].active_pane = next;
            if let Some(pane) = self.state.panes.remove(&pane_id) {
                for surface_id in pane.surfaces {
                    self.surface_backends.remove(&surface_id);
                    self.agent_sessions.remove(&surface_id);
                    self.claude_subagents
                        .retain(|_, subagent| subagent.surface != surface_id);
                    clear_codex_subagent_surface(&mut self.codex_subagents, surface_id);
                    if let Some(surface) = self.state.surfaces.remove(&surface_id)
                        && let SurfaceKind::Terminal(terminal) = surface.kind
                    {
                        self.terminals.remove(&terminal);
                        self.commands.remove(&terminal);
                        self.terminal_notices.remove(&terminal);
                        self.terminal_processes.remove(&terminal);
                        self.last_bell_notifications.remove(&terminal);
                        self.foreground_agents.remove(&terminal);
                        self.automatic_agent_status.remove(&terminal);
                    }
                }
            }
            self.resize_terminals();
            self.rebuild_scene();
        }
    }

    /// Apply non-terminal navigation at the pointer. Returns true when a UI
    /// control consumed the click and the terminal must not also receive it.
    fn focus_at_cursor(&mut self) -> bool {
        let (x, y) = self.cursor_position;
        if self.titlebar_command_bounds().contains(x, y) {
            self.palette_visible = true;
            self.palette_index = 0;
            self.palette_scroll = 0;
            self.palette_hover = None;
            self.rebuild_scene();
            return true;
        }
        if let Some(action) = self
            .titlebar_buttons()
            .into_iter()
            .find(|button| button.bounds.contains(x, y))
            .map(|button| button.action)
        {
            self.execute_titlebar_action(action);
            return true;
        }
        if self.sidebar_visible && self.workspace_search_bounds().contains(x, y) {
            self.workspace_search_focused = true;
            let bounds = self.workspace_search_bounds();
            let character_width = 14.0 * 0.6;
            let viewport = self.workspace_search.viewport(editor_columns(
                bounds.width - 18.0 - 2.0 * character_width,
                character_width,
            ));
            let column = viewport.start_column
                + ((x - bounds.x - 26.0).max(0.0) / character_width).round() as usize;
            self.workspace_search
                .begin_pointer_selection(column, self.modifiers.shift_key());
            self.text_edit_drag = Some(TextEditTarget::WorkspaceSearch);
            self.mouse_pressed = true;
            self.rebuild_scene();
            return true;
        }
        if self.workspace_search_focused {
            self.workspace_search_focused = false;
            self.workspace_search.clear_preedit();
        }
        if self.state.workspaces.is_empty() && self.empty_workspace_button().contains(x, y) {
            self.new_workspace();
            return true;
        }
        if let Some(pane) = self.pane_rects().into_iter().find_map(|(pane, rect)| {
            (self
                .state
                .panes
                .get(&pane)
                .is_some_and(|pane| pane.surfaces.is_empty())
                && empty_pane_button(rect).contains(x, y))
            .then_some(pane)
        }) {
            let _ = self
                .state
                .execute(AppCommand::Pane(PaneCommand::Focus { id: pane }));
            self.new_surface_command(None);
            return true;
        }
        let pending_requests = self.pending_agent_request_list();
        let pending_count = pending_requests.len();
        if self.notifications_visible && pending_count > 0 {
            let visible = self.agent_request_range(pending_count);
            let action_counts = pending_requests[visible.clone()]
                .iter()
                .map(|request| agent_approval_actions(request).len())
                .collect::<Vec<_>>();
            if let Some(hit) = ui::agent_approval_hit(
                self.agent_approval_region(pending_count),
                &action_counts,
                x,
                y,
            ) {
                let absolute = visible.start + hit.card;
                if let Some(request) = pending_requests.get(absolute)
                    && let Some(action) = agent_approval_actions(request).get(hit.action).copied()
                {
                    self.agent_request_selection = absolute;
                    self.agent_request_action = hit.action;
                    self.resolve_agent_request_from_ui(request.request_id.clone(), action);
                }
                return true;
            }
            let cards =
                ui::agent_approval_cards(self.agent_approval_region(pending_count), &action_counts);
            if let Some(card) = cards.iter().position(|card| card.content.contains(x, y)) {
                let absolute = visible.start + card;
                if let Some(request) = pending_requests.get(absolute) {
                    self.agent_request_detail = Some((request.request_id.clone(), 0));
                    self.rebuild_scene();
                    return true;
                }
            }
        }
        let notification_panel = self.notification_list_panel(pending_count);
        let notification_range = self.notification_range();
        if self.notifications_visible
            && notification_panel.contains(x, y)
            && let Some(hit) =
                ui::notification_row_at(notification_panel, notification_range.len(), x, y)
        {
            let index = match hit {
                ui::NotificationRowHit::Open(index) | ui::NotificationRowHit::Close(index) => index,
            };
            let target = self
                .state
                .notifications
                .iter()
                .rev()
                .nth(notification_range.start + index)
                .map(|item| item.id);
            if let Some(notification) = target {
                match hit {
                    ui::NotificationRowHit::Open(_) => self.focus_notification(notification),
                    ui::NotificationRowHit::Close(_) => self.clear_notification(notification),
                }
                return true;
            }
        }
        if self.sidebar_visible && x < self.sidebar_width() {
            let layout = self.sidebar_layout();
            if let Some(index) = layout
                .rows
                .iter()
                .find(|row| row.bounds.contains(x, y))
                .map(|row| row.index)
            {
                self.state.active_workspace = index;
                self.resize_terminals();
                self.rebuild_scene();
                return true;
            }
            if layout.new_workspace.contains(x, y) {
                self.new_workspace();
                return true;
            }
        }
        if let Some((pane, rect)) = self.pane_rects().into_iter().find(|(_, rect)| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        }) {
            let _ = self
                .state
                .execute(AppCommand::Pane(PaneCommand::Focus { id: pane }));
            if y < rect.y + 25.0 {
                if let Some(overflow) = self.surface_tab_overflow_bounds(pane, rect)
                    && overflow.contains(x, y)
                {
                    self.tab_overflow_menu = Some(TabOverflowMenu {
                        pane,
                        anchor: (overflow.x, overflow.y + overflow.height),
                        scroll_offset: self
                            .state
                            .panes
                            .get(&pane)
                            .map(|pane| {
                                ui::tab_overflow_initial_offset(
                                    pane.surfaces.len(),
                                    ui::tab_overflow_menu_capacity(
                                        self.window_size.height as f32,
                                        pane.surfaces.len(),
                                    ),
                                    pane.active_surface,
                                )
                            })
                            .unwrap_or(0),
                    });
                    self.rebuild_scene();
                    return true;
                }
                if self.surface_new_tab_bounds(rect).contains(x, y) {
                    self.new_surface_command(None);
                    return true;
                }
                let tabs = self.surface_tabs_layout(pane, rect);
                if let Some(index) = tabs
                    .iter()
                    .find(|tab| tab.close.is_some_and(|close| close.contains(x, y)))
                    .map(|tab| tab.index)
                {
                    if let Some(pane_state) = self.state.panes.get_mut(&pane) {
                        pane_state.active_surface = index;
                    }
                    self.request_close_active_surface();
                    return true;
                }
                if let Some(index) = tabs
                    .iter()
                    .find(|tab| tab.bounds.contains(x, y))
                    .map(|tab| tab.index)
                    && let Some(pane_state) = self.state.panes.get_mut(&pane)
                {
                    pane_state.active_surface = index;
                }
                self.resize_terminals();
                self.rebuild_scene();
                return true;
            }
            self.resize_terminals();
            self.rebuild_scene();
        }
        false
    }

    fn jump_latest_unread(&mut self) {
        if let Some(notification) = self
            .state
            .notifications
            .iter()
            .rev()
            .find(|item| !item.read)
            .map(|item| item.id)
        {
            self.focus_notification(notification);
        }
    }

    fn toggle_current_notification_unread(&mut self) {
        let Some(workspace) = self.state.active_workspace() else {
            return;
        };
        let Some(surface) = self
            .state
            .panes
            .get(&workspace.active_pane)
            .and_then(|pane| pane.active_surface())
        else {
            return;
        };
        if let Some(notification) = self
            .state
            .notifications
            .iter_mut()
            .rev()
            .find(|notification| notification.surface == surface)
        {
            notification.read = !notification.read;
            self.sync_notification_attention(surface);
            self.rebuild_scene();
        }
    }

    fn focus_notification(&mut self, notification: uuid::Uuid) {
        let Some(destination) = notification_destination(&self.state, notification) else {
            return;
        };
        self.state.active_workspace = destination.workspace_index;
        if let Some((pane_id, surface_index)) = destination.pane {
            if let Some(workspace) = self.state.workspaces.get_mut(destination.workspace_index) {
                workspace.active_pane = pane_id;
            }
            if let Some(pane) = self.state.panes.get_mut(&pane_id) {
                pane.active_surface = surface_index;
            }
        }
        if let Some(item) = self
            .state
            .notifications
            .iter_mut()
            .find(|item| item.id == notification)
        {
            item.read = true;
        }
        self.sync_notification_attention(destination.surface);
        self.resize_terminals();
        self.rebuild_scene();
    }

    fn set_notification_read(&mut self, notification: uuid::Uuid, read: bool) {
        let surface = self
            .state
            .notifications
            .iter_mut()
            .find(|item| item.id == notification)
            .map(|item| {
                item.read = read;
                item.surface
            });
        if let Some(surface) = surface {
            self.sync_notification_attention(surface);
        }
    }

    fn mark_all_notifications_read(&mut self) {
        let mut surfaces = self
            .state
            .notifications
            .iter_mut()
            .filter(|notification| !notification.read)
            .map(|notification| {
                notification.read = true;
                notification.surface
            })
            .collect::<Vec<_>>();
        surfaces.sort_unstable();
        surfaces.dedup();
        for surface in surfaces {
            self.sync_notification_attention(surface);
        }
        if self.navigation_hover == Some(NavigationHover::NotificationMarkAllRead) {
            self.navigation_hover = None;
            self.update_cursor_icon();
        }
        self.rebuild_scene();
    }

    fn has_unread_notifications(&self) -> bool {
        self.state
            .notifications
            .iter()
            .any(|notification| !notification.read)
    }

    fn mark_terminal_notifications_read(&mut self, terminal: TerminalId) -> bool {
        let Some(surface) = self.surface_for_terminal(terminal) else {
            return false;
        };
        if !mark_surface_notifications_read(&mut self.state.notifications, surface) {
            return false;
        }
        self.sync_notification_attention(surface);
        true
    }

    fn clear_notification(&mut self, notification: uuid::Uuid) {
        let surface = self
            .state
            .notifications
            .iter()
            .find(|item| item.id == notification)
            .map(|item| item.surface);
        self.state
            .notifications
            .retain(|item| item.id != notification);
        if let Some(surface) = surface {
            self.sync_notification_attention(surface);
        }
        let last = self.state.notifications.len().saturating_sub(1);
        self.notification_selection = self.notification_selection.min(last);
        self.notification_scroll = self.notification_scroll.min(last);
        self.rebuild_scene();
    }

    fn sync_notification_attention(&mut self, surface: SurfaceId) {
        self.sync_surface_agent_request_state(surface);
    }

    fn sync_surface_agent_request_state(&mut self, surface_id: SurfaceId) {
        let requests = self.pending_agent_request_list();
        let pending_state = pending_agent_state_for_surface(&requests, surface_id);
        let session_state = self
            .agent_sessions
            .get(&surface_id)
            .map(|session| session.state);
        let current_state = self
            .state
            .surfaces
            .get(&surface_id)
            .and_then(|surface| surface.agent.as_ref())
            .map(|agent| agent.state);
        let preserved_attention = self
            .state
            .surfaces
            .get(&surface_id)
            .map(|surface| surface.attention)
            .filter(|attention| {
                matches!(attention, AttentionState::Activity | AttentionState::Error)
            });
        let agent_state = effective_agent_state(pending_state, session_state, current_state);

        let notification_attention = self
            .state
            .notifications
            .iter()
            .rev()
            .filter(|item| item.surface == surface_id && !item.read)
            .map(|item| item.attention)
            .max_by_key(|attention| attention_priority(*attention));
        let terminal_attention = self
            .state
            .surfaces
            .get(&surface_id)
            .and_then(|surface| match surface.kind {
                SurfaceKind::Terminal(terminal) => self.terminal_notices.get(&terminal),
                _ => None,
            })
            .and_then(terminal_notice_attention);
        let attention = [
            pending_state.map(|_| AttentionState::NeedsInput),
            notification_attention,
            agent_state.map(agent_state_attention),
            terminal_attention,
            preserved_attention,
        ]
        .into_iter()
        .flatten()
        .max_by_key(|attention| attention_priority(*attention))
        .unwrap_or_default();

        if let Some(surface) = self.state.surfaces.get_mut(&surface_id) {
            if let (Some(agent), Some(state)) = (&mut surface.agent, agent_state) {
                agent.state = state;
            }
            surface.attention = attention;
        }
        if let Some(workspace_index) = self.workspace_for_surface(surface_id) {
            self.sync_workspace_agent_status(workspace_index);
        }
    }

    fn workspace_for_surface(&self, surface: SurfaceId) -> Option<usize> {
        let pane = self
            .state
            .panes
            .values()
            .find(|pane| pane.surfaces.contains(&surface))?
            .id;
        self.state
            .workspaces
            .iter()
            .position(|workspace| workspace.root.contains(pane))
    }

    fn sync_workspace_agent_status(&mut self, workspace_index: usize) {
        let priority = self.workspace_agent_priority(workspace_index);
        let has_terminal_error =
            self.state
                .workspaces
                .get(workspace_index)
                .is_some_and(|workspace| {
                    let mut pane_ids = Vec::new();
                    workspace.root.leaves(&mut pane_ids);
                    pane_ids
                        .iter()
                        .filter_map(|pane| self.state.panes.get(pane))
                        .flat_map(|pane| pane.surfaces.iter())
                        .filter_map(|surface| self.state.surfaces.get(surface))
                        .filter_map(|surface| match surface.kind {
                            SurfaceKind::Terminal(terminal) => Some(terminal),
                            _ => None,
                        })
                        .any(|terminal| {
                            self.terminal_notices
                                .get(&terminal)
                                .is_some_and(terminal_notice_is_error)
                        })
                });
        if let Some(workspace) = self.state.workspaces.get_mut(workspace_index) {
            workspace.status = if has_terminal_error {
                wmcterm_core::WorkspaceStatus::Error
            } else {
                workspace_status_for_agent_priority(priority)
            };
        }
    }

    fn tab_at_cursor(&self) -> Option<(PaneId, SurfaceId)> {
        let (x, y) = self.cursor_position;
        let (pane, rect) = self.pane_rects().into_iter().find(|(_, rect)| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + 25.0
        })?;
        let index = self
            .surface_tabs_layout(pane, rect)
            .iter()
            .find(|tab| tab.bounds.contains(x, y))?
            .index;
        let surface = *self.state.panes.get(&pane)?.surfaces.get(index)?;
        Some((pane, surface))
    }

    fn open_tab_context_menu(&mut self) -> bool {
        let Some((pane, surface)) = self.tab_at_cursor() else {
            return false;
        };
        self.tab_context_menu = Some(TabContextMenu {
            pane,
            surface,
            anchor: self.cursor_position,
        });
        self.workspace_context_menu = None;
        self.sidebar_context_menu = None;
        self.terminal_context_menu = None;
        self.notification_context_menu = None;
        self.rebuild_scene();
        true
    }

    fn open_workspace_context_menu(&mut self) -> bool {
        let (x, y) = self.cursor_position;
        if !self.sidebar_visible || x >= self.sidebar_width() {
            return false;
        }
        let layout = self.sidebar_layout();
        let Some(index) = layout
            .rows
            .iter()
            .find(|row| row.bounds.contains(x, y))
            .map(|row| row.index)
        else {
            return false;
        };
        let Some(workspace) = self.state.workspaces.get(index) else {
            return false;
        };
        self.state.active_workspace = index;
        self.workspace_context_menu = Some(WorkspaceContextMenu {
            workspace: workspace.id,
            anchor: self.cursor_position,
        });
        self.sidebar_context_menu = None;
        self.tab_context_menu = None;
        self.terminal_context_menu = None;
        self.notification_context_menu = None;
        self.resize_terminals();
        self.rebuild_scene();
        true
    }

    fn open_sidebar_context_menu(&mut self) -> bool {
        let (x, y) = self.cursor_position;
        if !self.sidebar_visible || x >= self.sidebar_width() || y < 40.0 {
            return false;
        }
        let layout = self.sidebar_layout();
        if self.workspace_search_bounds().contains(x, y)
            || layout.new_workspace.contains(x, y)
            || layout.rows.iter().any(|row| row.bounds.contains(x, y))
        {
            return false;
        }
        self.sidebar_context_menu = Some(SidebarContextMenu {
            anchor: self.cursor_position,
        });
        self.tab_context_menu = None;
        self.workspace_context_menu = None;
        self.terminal_context_menu = None;
        self.notification_context_menu = None;
        self.rebuild_scene();
        true
    }

    fn open_terminal_context_menu(&mut self) -> bool {
        let Some((pane, terminal)) = self.terminal_at_cursor() else {
            return false;
        };
        if let Some(workspace) = self.state.workspaces.get_mut(self.state.active_workspace) {
            workspace.active_pane = pane;
        }
        self.terminal_context_menu = Some(TerminalContextMenu {
            pane,
            terminal,
            anchor: self.cursor_position,
        });
        self.tab_context_menu = None;
        self.workspace_context_menu = None;
        self.sidebar_context_menu = None;
        self.notification_context_menu = None;
        self.mark_terminal_notifications_read(terminal);
        self.rebuild_scene();
        true
    }

    fn notification_at_cursor(&self) -> Option<uuid::Uuid> {
        if !self.notifications_visible {
            return None;
        }
        let (x, y) = self.cursor_position;
        let pending_count = self.pending_agent_requests.len().unwrap_or_default();
        let panel = self.notification_list_panel(pending_count);
        let visible = self.notification_range();
        let hit = ui::notification_row_at(panel, visible.len(), x, y)?;
        let row = match hit {
            ui::NotificationRowHit::Open(row) | ui::NotificationRowHit::Close(row) => row,
        };
        self.state
            .notifications
            .iter()
            .rev()
            .nth(visible.start + row)
            .map(|item| item.id)
    }

    fn open_notification_context_menu(&mut self) -> bool {
        let Some(notification) = self.notification_at_cursor() else {
            return false;
        };
        self.notification_context_menu = Some(NotificationContextMenu {
            notification,
            anchor: self.cursor_position,
        });
        self.tab_context_menu = None;
        self.workspace_context_menu = None;
        self.sidebar_context_menu = None;
        self.terminal_context_menu = None;
        self.rebuild_scene();
        true
    }

    fn execute_notification_context_action(&mut self, action: NotificationContextAction) {
        let Some(menu) = self.notification_context_menu.take() else {
            return;
        };
        match action {
            NotificationContextAction::Open => self.focus_notification(menu.notification),
            NotificationContextAction::ToggleRead => {
                if let Some(read) = self
                    .state
                    .notifications
                    .iter()
                    .find(|item| item.id == menu.notification)
                    .map(|item| item.read)
                {
                    self.set_notification_read(menu.notification, !read);
                }
                self.rebuild_scene();
            }
            NotificationContextAction::Clear => self.clear_notification(menu.notification),
        }
    }

    fn execute_terminal_context_action(&mut self, action: TerminalContextAction) {
        let Some(menu) = self.terminal_context_menu.take() else {
            return;
        };
        if !terminal_context_action_enabled(action, self.selection, menu.pane) {
            self.rebuild_scene();
            return;
        }
        match action {
            TerminalContextAction::Copy => {
                self.copy_selection(ClipboardTarget::Clipboard);
            }
            TerminalContextAction::Paste => {
                if let Some(text) = self
                    .clipboard
                    .as_mut()
                    .and_then(|clipboard| clipboard.get_text().ok())
                {
                    let notifications_changed =
                        self.mark_terminal_notifications_read(menu.terminal);
                    self.track_foreground_agent_paste(menu.terminal, &text);
                    if let Some(runtime) = self.terminals.get(&menu.terminal) {
                        let _ = runtime.session.send(SessionCommand::Paste(text));
                    }
                    if notifications_changed {
                        self.rebuild_scene();
                    }
                }
            }
            TerminalContextAction::CopyAll => {
                self.copy_all_to_clipboard(menu.terminal);
            }
            TerminalContextAction::ClearScrollback => {
                if let Some(runtime) = self.terminals.get(&menu.terminal) {
                    let _ = runtime.session.send(SessionCommand::ClearScrollback);
                }
            }
        }
        self.rebuild_scene();
    }

    fn execute_tab_context_action(&mut self, action: TabContextAction) {
        let Some(menu) = self.tab_context_menu.take() else {
            return;
        };
        let Some(index) = self
            .state
            .panes
            .get(&menu.pane)
            .and_then(|pane| pane.surfaces.iter().position(|id| *id == menu.surface))
        else {
            self.rebuild_scene();
            return;
        };
        if let Some(workspace) = self.state.workspaces.get_mut(self.state.active_workspace) {
            workspace.active_pane = menu.pane;
        }
        if let Some(pane) = self.state.panes.get_mut(&menu.pane) {
            pane.active_surface = index;
        }
        let tab_count = self
            .state
            .panes
            .get(&menu.pane)
            .map(|pane| pane.surfaces.len())
            .unwrap_or(0);
        if !tab_context_action_enabled(action, tab_count) {
            self.rebuild_scene();
            return;
        }
        match action {
            TabContextAction::Close => {
                self.request_close_active_surface();
            }
            TabContextAction::CloseOthers => {
                let others = self
                    .state
                    .panes
                    .get(&menu.pane)
                    .map(|pane| {
                        pane.surfaces
                            .iter()
                            .copied()
                            .filter(|surface| *surface != menu.surface)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.request_close_surfaces(others);
                if let Some(pane) = self.state.panes.get_mut(&menu.pane) {
                    pane.active_surface = pane
                        .surfaces
                        .iter()
                        .position(|id| *id == menu.surface)
                        .unwrap_or(0);
                }
                self.resize_terminals();
                self.rebuild_scene();
            }
            TabContextAction::NewSurface => {
                self.new_surface_command(None);
            }
        }
    }

    fn execute_workspace_context_action(&mut self, action: WorkspaceContextAction) {
        let Some(menu) = self.workspace_context_menu.take() else {
            return;
        };
        let Some(index) = self
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == menu.workspace)
        else {
            self.rebuild_scene();
            return;
        };
        self.state.active_workspace = index;
        if !workspace_context_action_enabled(action, self.state.workspaces.len()) {
            self.rebuild_scene();
            return;
        }
        match action {
            WorkspaceContextAction::NewSurface => {
                self.new_surface_command(None);
            }
            WorkspaceContextAction::OpenFileManager => {
                let cwd = self.state.workspaces[index].cwd.clone();
                if let Err(error) = open_directory_in_file_manager(&cwd) {
                    tracing::error!(%error, path=%cwd.display(), "opening workspace in file manager failed");
                }
                self.rebuild_scene();
            }
            WorkspaceContextAction::OpenTerminal => {
                let cwd = self.state.workspaces[index].cwd.clone();
                if let Err(error) = open_directory_in_terminal(&cwd) {
                    tracing::error!(%error, path=%cwd.display(), "opening workspace in terminal failed");
                }
                self.rebuild_scene();
            }
            WorkspaceContextAction::Rename => {
                self.open_workspace_prompt(PromptKind::RenameWorkspace);
            }
            WorkspaceContextAction::Describe => {
                self.open_workspace_prompt(PromptKind::DescribeWorkspace);
            }
            WorkspaceContextAction::ChangeDirectory => {
                self.open_workspace_prompt(PromptKind::ChangeWorkspaceDirectory);
            }
            WorkspaceContextAction::Close => self.request_close_workspace(),
        }
    }

    fn begin_mouse_action(&mut self) {
        if self.workspace_search_focused
            && !self
                .workspace_search_bounds()
                .contains(self.cursor_position.0, self.cursor_position.1)
        {
            self.workspace_search_focused = false;
            self.workspace_search.clear_preedit();
            self.rebuild_scene();
        }
        if self.handle_overlay_click() {
            return;
        }
        if let Some(direction) =
            self.window_resize_direction(self.cursor_position.0, self.cursor_position.1)
        {
            if let Some(renderer) = &self.renderer {
                let _ = renderer.window().drag_resize_window(direction);
            }
            return;
        }
        self.mouse_pressed = true;
        let (x, y) = self.cursor_position;
        if self.sidebar_resize_hit(x, y) {
            self.sidebar_drag = true;
            return;
        }
        if self.begin_sidebar_scrollbar_action() {
            return;
        }
        if self.begin_scrollbar_action() {
            return;
        }
        if self.begin_workspace_drag() {
            return;
        }
        if let Some(workspace) = self.state.active_workspace() {
            let zoomed = self
                .state
                .panes
                .get(&workspace.active_pane)
                .is_some_and(|pane| pane.zoomed);
            let area = terminal_workspace_area(
                self.window_size.width as f32,
                self.window_size.height as f32,
                self.sidebar_width(),
            );
            if !zoomed && let Some(drag) = divider_at(&workspace.root, area, x, y, &mut Vec::new())
            {
                self.split_drag = Some(drag);
                return;
            }
        }
        if self.focus_at_cursor() {
            if self.text_edit_drag.is_none() {
                self.mouse_pressed = false;
            }
            return;
        }
        if y < 40.0 {
            if let Some(renderer) = &self.renderer {
                let now = std::time::Instant::now();
                if self
                    .last_titlebar_click
                    .is_some_and(|(previous, position)| {
                        titlebar_double_click(previous, position, now, (x, y))
                    })
                {
                    let window = renderer.window();
                    window.set_maximized(!window.is_maximized());
                    self.last_titlebar_click = None;
                } else {
                    self.last_titlebar_click = Some((now, (x, y)));
                    let _ = renderer.window().drag_window();
                }
            }
            self.mouse_pressed = false;
            return;
        }
        if let Some((_, terminal)) = self.terminal_at_cursor()
            && self.mark_terminal_notifications_read(terminal)
        {
            self.rebuild_scene();
        }
        if self.modifiers.control_key()
            && let Some(target) = self.terminal_open_target_at_cursor()
        {
            self.mouse_pressed = false;
            open_terminal_target(target);
            return;
        }
        if !self.modifiers.shift_key()
            && self.send_terminal_mouse(MouseActionInput::Press, Some(MouseButtonInput::Left))
        {
            self.terminal_mouse_capture = self.terminal_at_cursor();
            self.selection = None;
            return;
        }
        if let Some((pane, terminal, cell)) = self.cell_at_cursor() {
            let now = std::time::Instant::now();
            let word_bounds = self.terminal_word_bounds(terminal, cell);
            let click_cell = word_bounds.map_or(cell, |(start, _)| start);
            let double_click = self.last_terminal_click.is_some_and(|previous| {
                previous.pane == pane
                    && previous.terminal == terminal
                    && previous.cell == click_cell
                    && terminal_double_click(previous.at, now)
            });
            self.last_terminal_click = Some(TerminalClick {
                at: now,
                pane,
                terminal,
                cell: click_cell,
            });
            if double_click && let Some((start, end)) = word_bounds {
                self.selection = Some(MouseSelection {
                    pane,
                    terminal,
                    start,
                    end,
                });
                // Keep the word boundary stable if a queued pointer-motion event
                // arrives before the second button release.
                self.mouse_pressed = false;
                self.last_terminal_click = None;
                self.rebuild_scene();
                return;
            }
            self.selection = Some(MouseSelection {
                pane,
                terminal,
                start: cell,
                end: cell,
            });
            self.selection_last_redraw = None;
            self.selection_autoscroll_last = None;
        } else {
            self.selection = None;
            self.selection_last_redraw = None;
            self.selection_autoscroll_last = None;
        }
    }

    fn terminal_word_bounds(
        &self,
        terminal: TerminalId,
        cell: (usize, usize),
    ) -> Option<((usize, usize), (usize, usize))> {
        let snapshot = &self.terminals.get(&terminal)?.snapshot;
        let viewport_row = cell.1.checked_sub(snapshot.screen_row_origin)?;
        let row = snapshot.cells.get(viewport_row)?;
        let (start, end) = word_cell_bounds(row, cell.0)?;
        Some(((start, cell.1), (end, cell.1)))
    }

    /// Resolve only the row under the pointer. Ordinary rendering and pointer
    /// movement never scan the viewport or scrollback for plain-text links.
    fn terminal_open_target_at_cursor(&self) -> Option<TerminalOpenTarget> {
        const MAX_LINK_ROW_CELLS: usize = 4_096;

        let (_, terminal, (column, screen_row)) = self.cell_at_cursor()?;
        let runtime = self.terminals.get(&terminal)?;
        let viewport_row = screen_row.checked_sub(runtime.snapshot.screen_row_origin)?;
        let row = runtime.snapshot.cells.get(viewport_row)?;
        if row.len() > MAX_LINK_ROW_CELLS {
            return None;
        }
        if let Some(uri) = row.get(column)?.hyperlink.as_deref() {
            return terminal_target_from_osc8(uri, &self.cwd_for_terminal(terminal)?);
        }

        let mut text = String::new();
        let mut clicked_byte = None;
        for (index, cell) in row.iter().enumerate() {
            if index == column {
                clicked_byte = Some(text.len());
            }
            text.push_str(&cell.text);
        }
        terminal_target_in_line(&text, clicked_byte?, &self.cwd_for_terminal(terminal)?)
    }

    fn scrollbar_at_cursor(&self) -> Option<(PaneId, TerminalId, ui::ScrollbarLayout)> {
        let (x, y) = self.cursor_position;
        let (pane, rect) = self.pane_rects().into_iter().find(|(_, rect)| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        })?;
        let terminal = self
            .state
            .panes
            .get(&pane)
            .and_then(|pane| pane.active_surface())
            .and_then(|surface| self.state.surfaces.get(&surface))
            .and_then(|surface| match surface.kind {
                SurfaceKind::Terminal(terminal) => Some(terminal),
                _ => None,
            })?;
        let snapshot = &self.terminals.get(&terminal)?.snapshot;
        let layout = ui::terminal_scrollbar_layout(
            ui::Bounds {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            },
            snapshot.rows as usize,
            snapshot.scrollback_rows,
            snapshot.viewport_offset,
        )?;
        layout
            .track
            .contains(x, y)
            .then_some((pane, terminal, layout))
    }

    fn begin_scrollbar_action(&mut self) -> bool {
        let Some((pane, terminal, layout)) = self.scrollbar_at_cursor() else {
            return false;
        };
        let (x, y) = self.cursor_position;
        let _ = self
            .state
            .execute(AppCommand::Pane(PaneCommand::Focus { id: pane }));
        if layout.thumb.contains(x, y) {
            let last_offset = self
                .terminals
                .get(&terminal)
                .map(|runtime| runtime.snapshot.viewport_offset)
                .unwrap_or_default()
                .min(layout.max_offset);
            self.scrollbar_drag = Some(ScrollbarDrag {
                pane,
                terminal,
                grab_y: y - layout.thumb.y,
                last_offset,
            });
        } else if let Some(delta) = layout.page_delta_at(y) {
            if let Some(runtime) = self.terminals.get(&terminal) {
                let _ = runtime.session.send(SessionCommand::Scroll(delta));
            }
            self.mouse_pressed = false;
        }
        self.rebuild_scene();
        true
    }

    fn update_scrollbar_drag(&mut self) -> bool {
        let Some(mut drag) = self.scrollbar_drag else {
            return false;
        };
        let Some((_, rect)) = self
            .pane_rects()
            .into_iter()
            .find(|(pane, _)| *pane == drag.pane)
        else {
            self.scrollbar_drag = None;
            return true;
        };
        let Some(runtime) = self.terminals.get(&drag.terminal) else {
            self.scrollbar_drag = None;
            return true;
        };
        let Some(layout) = ui::terminal_scrollbar_layout(
            ui::Bounds {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            },
            runtime.snapshot.rows as usize,
            runtime.snapshot.scrollback_rows,
            drag.last_offset,
        ) else {
            self.scrollbar_drag = None;
            return true;
        };
        let target = layout.offset_for_pointer(self.cursor_position.1, drag.grab_y);
        if target != drag.last_offset {
            let delta = drag.last_offset as isize - target as isize;
            let _ = runtime.session.send(SessionCommand::Scroll(delta));
            drag.last_offset = target;
            self.scrollbar_drag = Some(drag);
        }
        true
    }

    fn handle_overlay_click(&mut self) -> bool {
        let (x, y) = self.cursor_position;
        if self.codex_trust_confirmation.is_some() {
            let (cancel, proceed, trust) = self.codex_trust_buttons();
            if trust.contains(x, y) {
                self.trust_and_start_unattended_codex();
            } else if proceed.contains(x, y) {
                self.continue_untrusted_codex();
            } else if cancel.contains(x, y) {
                self.codex_trust_confirmation = None;
                self.update_navigation_hover();
                self.update_cursor_icon();
                self.rebuild_scene();
            }
            return true;
        }
        if self.close_confirmation.is_some() {
            let (cancel, confirm) = self.confirmation_buttons();
            if confirm.contains(x, y) {
                self.confirm_close();
            } else if cancel.contains(x, y) {
                self.close_confirmation = None;
                self.update_navigation_hover();
                self.update_cursor_icon();
                self.rebuild_scene();
            }
            return true;
        }
        if let Some((request_id, scroll)) = self.agent_request_detail.clone() {
            let layout = self
                .pending_agent_requests
                .get(&request_id)
                .ok()
                .flatten()
                .map(|request| self.agent_request_detail_layout(&request, scroll));
            if let Some(layout) = layout {
                if let Some((track, thumb)) = layout.scrollbar
                    && (ui::Bounds {
                        x: track.x - 6.0,
                        width: track.width + 12.0,
                        ..track
                    })
                    .contains(x, y)
                {
                    let grab_y = if y >= thumb.y && y < thumb.y + thumb.height {
                        y - thumb.y
                    } else {
                        thumb.height / 2.0
                    };
                    self.set_agent_request_detail_scroll(scrollbar_scroll_for_pointer(
                        track,
                        thumb,
                        layout.max_scroll,
                        y,
                        grab_y,
                    ));
                    self.agent_request_detail_scrollbar_drag =
                        Some(AgentRequestDetailScrollbarDrag { grab_y });
                    self.mouse_pressed = true;
                    self.rebuild_scene();
                    return true;
                }
                if layout.panel.contains(x, y) {
                    return true;
                }
            }
            self.agent_request_detail = None;
            self.agent_request_detail_scrollbar_drag = None;
            self.rebuild_scene();
            return true;
        }
        if let Some(menu) = self.tab_overflow_menu {
            let lines = self.tab_overflow_lines(menu.pane);
            let layout = self.tab_overflow_layout(menu, &lines);
            let index = ui::menu_row_at(
                layout.panel,
                x,
                y,
                ui::TAB_OVERFLOW_ROW_HEIGHT,
                layout.visible.len(),
            )
            .and_then(|row| layout.surface_index(row));
            if let Some(index) = index {
                if let Some(pane) = self.state.panes.get_mut(&menu.pane) {
                    pane.active_surface = index;
                }
                self.tab_overflow_menu = None;
                self.resize_terminals();
                self.rebuild_scene();
            } else {
                self.tab_overflow_menu = None;
                self.rebuild_scene();
            }
            return true;
        }
        if let Some(menu) = self.notification_context_menu {
            let bounds = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.notification_context_labels(false)),
                30.0,
                NOTIFICATION_CONTEXT_ACTIONS.len(),
            );
            let action = ui::menu_row_at(bounds, x, y, 30.0, NOTIFICATION_CONTEXT_ACTIONS.len())
                .and_then(|index| NOTIFICATION_CONTEXT_ACTIONS.get(index).copied());
            if let Some(action) = action {
                self.execute_notification_context_action(action);
            } else {
                self.notification_context_menu = None;
                if !self.notification_panel().contains(x, y) {
                    self.notifications_visible = false;
                    self.notification_scroll = 0;
                    self.notification_selection = 0;
                }
                self.rebuild_scene();
            }
            return true;
        }
        if let Some(menu) = self.terminal_context_menu {
            let bounds = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.terminal_context_labels()),
                30.0,
                TERMINAL_CONTEXT_ACTIONS.len(),
            );
            let action = ui::menu_row_at(bounds, x, y, 30.0, TERMINAL_CONTEXT_ACTIONS.len())
                .and_then(|index| TERMINAL_CONTEXT_ACTIONS.get(index).copied());
            if let Some(action) = action {
                self.execute_terminal_context_action(action);
            } else {
                self.terminal_context_menu = None;
                self.rebuild_scene();
            }
            return true;
        }
        if let Some(menu) = self.tab_context_menu {
            let bounds = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.tab_context_labels()),
                30.0,
                TAB_CONTEXT_ACTIONS.len(),
            );
            let action = ui::menu_row_at(bounds, x, y, 30.0, TAB_CONTEXT_ACTIONS.len())
                .and_then(|index| TAB_CONTEXT_ACTIONS.get(index).copied());
            if let Some(action) = action {
                self.execute_tab_context_action(action);
            } else {
                self.tab_context_menu = None;
                self.rebuild_scene();
            }
            return true;
        }
        if let Some(menu) = self.workspace_context_menu {
            let bounds = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.workspace_context_labels()),
                30.0,
                WORKSPACE_CONTEXT_ACTIONS.len(),
            );
            let action = ui::menu_row_at(bounds, x, y, 30.0, WORKSPACE_CONTEXT_ACTIONS.len())
                .and_then(|index| WORKSPACE_CONTEXT_ACTIONS.get(index).copied());
            if let Some(action) = action {
                self.execute_workspace_context_action(action);
            } else {
                self.workspace_context_menu = None;
                self.rebuild_scene();
            }
            return true;
        }
        if let Some(menu) = self.sidebar_context_menu {
            let bounds = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&[self.language.new_workspace()]),
                30.0,
                1,
            );
            if ui::menu_row_at(bounds, x, y, 30.0, 1).is_some() {
                self.sidebar_context_menu = None;
                self.new_workspace();
            } else {
                self.sidebar_context_menu = None;
                self.rebuild_scene();
            }
            return true;
        }
        if self.prompt.is_some() {
            let input = self.prompt_input_bounds();
            if input.contains(x, y)
                && let Some(prompt) = &mut self.prompt
            {
                let character_width = 15.0 * 0.6;
                let text_x = input.x + 10.0 + 2.0 * character_width;
                let viewport = prompt.editor.viewport(editor_columns(
                    input.width - 20.0 - 2.0 * character_width,
                    character_width,
                ));
                let column = viewport.start_column
                    + ((x - text_x).max(0.0) / character_width).round() as usize;
                prompt
                    .editor
                    .begin_pointer_selection(column, self.modifiers.shift_key());
                self.text_edit_drag = Some(TextEditTarget::Prompt);
                self.mouse_pressed = true;
                prompt.error = None;
                self.rebuild_scene();
            }
            return true;
        }
        if self.palette_visible {
            let registry = ActionRegistry::default();
            let actions = self.palette_actions(&registry);
            let capacity =
                (((self.window_size.height as f32 - 150.0) / 25.0) as usize).clamp(3, 12);
            let visible = ui::scrolled_range(actions.len(), self.palette_scroll, capacity);
            let panel = self.command_palette_panel(actions.len());
            let command = self.titlebar_command_bounds();
            if command.contains(x, y) {
                let prefix = format!("{} > ", self.language.command_palette());
                let query_left = command.x + 12.0 + ui::display_width(&prefix) as f32 * 14.0 * 0.6;
                let character_width = 14.0 * 0.6;
                let viewport = self.palette_query.viewport(editor_columns(
                    command.width - 24.0 - ui::display_width(&prefix) as f32 * character_width,
                    character_width,
                ));
                let column = viewport.start_column
                    + ((x - query_left).max(0.0) / character_width).round() as usize;
                self.palette_query
                    .begin_pointer_selection(column, self.modifiers.shift_key());
                self.text_edit_drag = Some(TextEditTarget::Palette);
                self.mouse_pressed = true;
                self.rebuild_scene();
                return true;
            }
            if !panel.contains(x, y) {
                self.palette_visible = false;
                self.palette_hover = None;
                self.palette_query.clear_preedit();
                self.rebuild_scene();
                return true;
            }
            if panel.contains(x, y)
                && let Some(row) = ui::palette_row_at(panel, visible.len(), x, y)
            {
                self.palette_index = visible.start + row;
                self.palette_hover = Some(self.palette_index);
                self.execute_palette_action();
            }
            return true;
        }
        if self.search_visible {
            let width = 430.0_f32
                .min(self.window_size.width as f32 - 32.0)
                .max(220.0);
            let left = (self.window_size.width as f32 - width - 16.0).max(16.0);
            if (ui::Bounds {
                x: left,
                y: 8.0,
                width,
                height: 30.0,
            })
            .contains(x, y)
            {
                let character_width = 14.0 * 0.6;
                let label_columns = ui::display_width(self.language.find()) + 2;
                let text_x = left + 10.0 + label_columns as f32 * character_width;
                let viewport = self.search_editor.viewport(editor_columns(
                    width - 20.0 - label_columns as f32 * character_width,
                    character_width,
                ));
                let column = viewport.start_column
                    + ((x - text_x).max(0.0) / character_width).round() as usize;
                self.search_editor
                    .begin_pointer_selection(column, self.modifiers.shift_key());
                self.text_edit_drag = Some(TextEditTarget::Search);
                self.mouse_pressed = true;
                self.rebuild_scene();
                return true;
            }
        }
        if notification_toggle_press(
            self.notifications_visible,
            self.titlebar_buttons().into_iter().any(|button| {
                matches!(button.action, TitlebarAction::ToggleNotifications)
                    && button.bounds.contains(x, y)
            }),
        ) {
            self.execute_action("notifications.toggle");
            return true;
        }
        match notification_overlay_press(
            self.notifications_visible,
            self.notification_panel(),
            x,
            y,
        ) {
            NotificationOverlayPress::Inside => {
                if ui::notification_mark_all_read_hit(
                    self.notification_panel(),
                    self.has_unread_notifications(),
                    x,
                    y,
                ) {
                    self.mark_all_notifications_read();
                    return true;
                }
                self.focus_at_cursor();
                return true;
            }
            NotificationOverlayPress::DismissAndPropagate => {
                self.notifications_visible = false;
                self.notification_scroll = 0;
                self.notification_selection = 0;
                self.notification_context_menu = None;
                self.rebuild_scene();
                // Losing focus dismisses the floating panel, while the same
                // press continues to the terminal or other target underneath.
            }
            NotificationOverlayPress::Inactive => {}
        }
        false
    }

    fn dismiss_notifications_outside_cursor(&mut self) -> bool {
        let (x, y) = self.cursor_position;
        if notification_overlay_press(self.notifications_visible, self.notification_panel(), x, y)
            != NotificationOverlayPress::DismissAndPropagate
        {
            return false;
        }
        self.notifications_visible = false;
        self.notification_scroll = 0;
        self.notification_selection = 0;
        self.notification_context_menu = None;
        self.rebuild_scene();
        true
    }

    fn update_palette_hover(&mut self) {
        if !self.palette_visible {
            if self.palette_hover.take().is_some() {
                self.rebuild_scene();
            }
            return;
        }
        let registry = ActionRegistry::default();
        let actions = self.palette_actions(&registry);
        let capacity = (((self.window_size.height as f32 - 150.0) / 25.0) as usize).clamp(3, 12);
        let visible = ui::scrolled_range(actions.len(), self.palette_scroll, capacity);
        let panel = self.command_palette_panel(actions.len());
        let (x, y) = self.cursor_position;
        let hovered = ui::palette_row_at(panel, visible.len(), x, y).map(|row| visible.start + row);
        if hovered != self.palette_hover {
            self.palette_hover = hovered;
            self.rebuild_scene();
        }
    }

    fn navigation_hover_at_cursor(&self) -> Option<NavigationHover> {
        let (x, y) = self.cursor_position;
        if self.codex_trust_confirmation.is_some() {
            let (cancel, proceed, trust) = self.codex_trust_buttons();
            return codex_trust_confirmation_hover(cancel, proceed, trust, x, y);
        }
        if self.close_confirmation.is_some() {
            let (cancel, confirm) = self.confirmation_buttons();
            return confirmation_hover(cancel, confirm, x, y);
        }
        if let Some(menu) = self.tab_overflow_menu {
            let lines = self.tab_overflow_lines(menu.pane);
            let layout = self.tab_overflow_layout(menu, &lines);
            let row = ui::menu_row_at(
                layout.panel,
                x,
                y,
                ui::TAB_OVERFLOW_ROW_HEIGHT,
                layout.visible.len(),
            )?;
            let index = layout.surface_index(row)?;
            return Some(NavigationHover::ContextAction(index));
        }
        if let Some(menu) = self.notification_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.notification_context_labels(false)),
                30.0,
                NOTIFICATION_CONTEXT_ACTIONS.len(),
            );
            let index = ui::menu_row_at(panel, x, y, 30.0, NOTIFICATION_CONTEXT_ACTIONS.len())?;
            return Some(NavigationHover::ContextAction(index));
        }
        if let Some(menu) = self.terminal_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.terminal_context_labels()),
                30.0,
                TERMINAL_CONTEXT_ACTIONS.len(),
            );
            let index = ui::menu_row_at(panel, x, y, 30.0, TERMINAL_CONTEXT_ACTIONS.len())?;
            return terminal_context_action_enabled(
                TERMINAL_CONTEXT_ACTIONS[index],
                self.selection,
                menu.pane,
            )
            .then_some(NavigationHover::ContextAction(index));
        }
        if let Some(menu) = self.tab_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.tab_context_labels()),
                30.0,
                TAB_CONTEXT_ACTIONS.len(),
            );
            let index = ui::menu_row_at(panel, x, y, 30.0, TAB_CONTEXT_ACTIONS.len())?;
            let tab_count = self.state.panes.get(&menu.pane)?.surfaces.len();
            return tab_context_action_enabled(TAB_CONTEXT_ACTIONS[index], tab_count)
                .then_some(NavigationHover::ContextAction(index));
        }
        if let Some(menu) = self.workspace_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.workspace_context_labels()),
                30.0,
                WORKSPACE_CONTEXT_ACTIONS.len(),
            );
            let index = ui::menu_row_at(panel, x, y, 30.0, WORKSPACE_CONTEXT_ACTIONS.len())?;
            return workspace_context_action_enabled(
                WORKSPACE_CONTEXT_ACTIONS[index],
                self.state.workspaces.len(),
            )
            .then_some(NavigationHover::ContextAction(index));
        }
        if let Some(menu) = self.sidebar_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&[self.language.new_workspace()]),
                30.0,
                1,
            );
            ui::menu_row_at(panel, x, y, 30.0, 1)?;
            return Some(NavigationHover::ContextAction(0));
        }
        if self.prompt.is_some() || self.palette_visible {
            return None;
        }
        if self.sidebar_resize_hit(x, y) && !self.pointer_over_overlay() {
            return Some(NavigationHover::SidebarResize);
        }
        if self.sidebar_scrollbar_at_cursor().is_some() {
            return Some(NavigationHover::SidebarScrollbar);
        }
        if let Some(index) = self
            .titlebar_buttons()
            .iter()
            .position(|button| button.bounds.contains(x, y))
        {
            return Some(NavigationHover::TitlebarButton(index));
        }
        if self.state.workspaces.is_empty() && self.empty_workspace_button().contains(x, y) {
            return Some(NavigationHover::EmptyWorkspaceNew);
        }
        if let Some(pane) = self.pane_rects().into_iter().find_map(|(pane, rect)| {
            (self
                .state
                .panes
                .get(&pane)
                .is_some_and(|pane| pane.surfaces.is_empty())
                && empty_pane_button(rect).contains(x, y))
            .then_some(pane)
        }) {
            return Some(NavigationHover::EmptyPaneNew(pane));
        }
        if self.notifications_visible && self.notification_panel().contains(x, y) {
            if ui::notification_mark_all_read_hit(
                self.notification_panel(),
                self.has_unread_notifications(),
                x,
                y,
            ) {
                return Some(NavigationHover::NotificationMarkAllRead);
            }
            let requests = self.pending_agent_request_list();
            if !requests.is_empty() {
                let visible_requests = self.agent_request_range(requests.len());
                let action_counts = requests[visible_requests.clone()]
                    .iter()
                    .map(|request| agent_approval_actions(request).len())
                    .collect::<Vec<_>>();
                if let Some(hit) = ui::agent_approval_hit(
                    self.agent_approval_region(requests.len()),
                    &action_counts,
                    x,
                    y,
                ) {
                    return Some(NavigationHover::AgentApproval(
                        visible_requests.start + hit.card,
                        hit.action,
                    ));
                }
            }
            let visible = self.notification_range();
            let pending_count = requests.len();
            let panel = self.notification_list_panel(pending_count);
            return ui::notification_row_at(panel, visible.len(), x, y).map(|hit| match hit {
                ui::NotificationRowHit::Open(row) => {
                    NavigationHover::Notification(visible.start + row)
                }
                ui::NotificationRowHit::Close(row) => {
                    NavigationHover::NotificationClose(visible.start + row)
                }
            });
        }
        if self.sidebar_visible && x < self.sidebar_width() {
            let layout = self.sidebar_layout();
            if let Some(row) = layout.rows.iter().find(|row| row.bounds.contains(x, y)) {
                return Some(NavigationHover::Workspace(row.index));
            }
            return layout
                .new_workspace
                .contains(x, y)
                .then_some(NavigationHover::NewWorkspace);
        }
        if let Some(workspace) = self.state.active_workspace()
            && !self
                .state
                .panes
                .get(&workspace.active_pane)
                .is_some_and(|pane| pane.zoomed)
        {
            let area = terminal_workspace_area(
                self.window_size.width as f32,
                self.window_size.height as f32,
                self.sidebar_width(),
            );
            if let Some(hovered) = divider_at(&workspace.root, area, x, y, &mut Vec::new()) {
                let mut dividers = Vec::new();
                collect_dividers(&workspace.root, area, &mut Vec::new(), &mut dividers);
                if let Some(index) = dividers
                    .iter()
                    .position(|divider| divider.path == hovered.path)
                {
                    return Some(NavigationHover::PaneDivider(index));
                }
            }
        }
        for (pane, rect) in self.pane_rects() {
            if y < rect.y || y >= rect.y + 25.0 || x < rect.x || x >= rect.x + rect.width {
                continue;
            }
            if self
                .surface_tab_overflow_bounds(pane, rect)
                .is_some_and(|overflow| overflow.contains(x, y))
            {
                return Some(NavigationHover::TabOverflow(pane));
            }
            if self.surface_new_tab_bounds(rect).contains(x, y) {
                return Some(NavigationHover::TabNew(pane));
            }
            for tab in self.surface_tabs_layout(pane, rect) {
                if tab.close.is_some_and(|close| close.contains(x, y)) {
                    return Some(NavigationHover::TabClose(pane, tab.index));
                }
                if tab.bounds.contains(x, y) {
                    return Some(NavigationHover::Tab(pane, tab.index));
                }
            }
        }
        None
    }

    fn update_navigation_hover(&mut self) {
        let hover = self.navigation_hover_at_cursor();
        if hover != self.navigation_hover {
            let previous_notification = match self.navigation_hover {
                Some(
                    NavigationHover::Notification(index)
                    | NavigationHover::NotificationClose(index),
                ) => Some(index),
                _ => None,
            };
            let next_notification = match hover {
                Some(
                    NavigationHover::Notification(index)
                    | NavigationHover::NotificationClose(index),
                ) => Some(index),
                _ => None,
            };
            if next_notification != previous_notification {
                self.notification_hover_started =
                    next_notification.map(|index| (index, std::time::Instant::now()));
                self.notification_marquee_line = 0;
            }
            self.navigation_hover = hover;
            self.rebuild_scene();
        }
    }

    fn update_cursor_icon(&self) {
        let (x, y) = self.cursor_position;
        let icon = if self.codex_trust_confirmation.is_some() {
            let (cancel, proceed, trust) = self.codex_trust_buttons();
            if codex_trust_confirmation_hover(cancel, proceed, trust, x, y).is_some() {
                CursorIcon::Pointer
            } else {
                CursorIcon::Default
            }
        } else if self.close_confirmation.is_some() {
            let (cancel, confirm) = self.confirmation_buttons();
            confirmation_cursor_icon(cancel, confirm, x, y)
        } else if let Some(direction) = self.window_resize_direction(x, y) {
            match direction {
                ResizeDirection::East | ResizeDirection::West => CursorIcon::EwResize,
                ResizeDirection::North | ResizeDirection::South => CursorIcon::NsResize,
                ResizeDirection::NorthEast | ResizeDirection::SouthWest => CursorIcon::NeswResize,
                ResizeDirection::NorthWest | ResizeDirection::SouthEast => CursorIcon::NwseResize,
            }
        } else if self.sidebar_drag || self.sidebar_resize_hit(x, y) {
            CursorIcon::ColResize
        } else if let Some(drag) = &self.split_drag {
            match drag.axis {
                SplitAxis::Vertical => CursorIcon::ColResize,
                SplitAxis::Horizontal => CursorIcon::RowResize,
            }
        } else if self.sidebar_scrollbar_drag.is_some()
            || self.sidebar_scrollbar_at_cursor().is_some()
            || self.scrollbar_drag.is_some()
            || self.scrollbar_at_cursor().is_some()
        {
            CursorIcon::NsResize
        } else if self.titlebar_command_bounds().contains(x, y) {
            if self.palette_visible {
                CursorIcon::Text
            } else {
                CursorIcon::Pointer
            }
        } else if self.sidebar_visible && self.workspace_search_bounds().contains(x, y) {
            CursorIcon::Text
        } else if self.prompt.is_some() {
            let input = self.prompt_input_bounds();
            if input.contains(x, y) {
                CursorIcon::Text
            } else {
                CursorIcon::Default
            }
        } else if self.palette_visible {
            if self.palette_hover.is_some() {
                CursorIcon::Pointer
            } else {
                CursorIcon::Default
            }
        } else if self.search_visible
            && (ui::Bounds {
                x: (self.window_size.width as f32
                    - 430.0_f32
                        .min(self.window_size.width as f32 - 32.0)
                        .max(220.0)
                    - 16.0)
                    .max(16.0),
                y: 8.0,
                width: 430.0_f32
                    .min(self.window_size.width as f32 - 32.0)
                    .max(220.0),
                height: 30.0,
            })
            .contains(x, y)
        {
            CursorIcon::Text
        } else if self.navigation_hover.is_some() {
            CursorIcon::Pointer
        } else if let Some(workspace) = self.state.active_workspace() {
            let area = terminal_workspace_area(
                self.window_size.width as f32,
                self.window_size.height as f32,
                self.sidebar_width(),
            );
            if !self
                .state
                .panes
                .get(&workspace.active_pane)
                .is_some_and(|pane| pane.zoomed)
                && let Some(divider) = divider_at(&workspace.root, area, x, y, &mut Vec::new())
            {
                match divider.axis {
                    SplitAxis::Vertical => CursorIcon::ColResize,
                    SplitAxis::Horizontal => CursorIcon::RowResize,
                }
            } else if self.modifiers.control_key()
                && self.terminal_open_target_at_cursor().is_some()
            {
                CursorIcon::Pointer
            } else if self.terminal_at_cursor().is_some() {
                CursorIcon::Text
            } else {
                CursorIcon::Default
            }
        } else {
            CursorIcon::Default
        };
        if let Some(renderer) = &self.renderer {
            renderer.window().set_cursor(icon);
        }
    }

    fn update_ime_cursor_area(&self) {
        let Some(renderer) = &self.renderer else {
            return;
        };
        let (x, y, height) = if self.workspace_search_focused {
            let bounds = self.workspace_search_bounds();
            let character_width = 14.0 * 0.6;
            let viewport = self.workspace_search.viewport(editor_columns(
                bounds.width - 18.0 - 2.0 * character_width,
                character_width,
            ));
            (
                bounds.x
                    + 26.0
                    + (self.workspace_search.cursor_display_column() - viewport.start_column)
                        as f32
                        * character_width,
                bounds.y + 2.0,
                bounds.height - 4.0,
            )
        } else if let Some(prompt) = &self.prompt {
            let input = self.prompt_input_bounds();
            let character_width = 15.0 * 0.6;
            let viewport = prompt.editor.viewport(editor_columns(
                input.width - 20.0 - 2.0 * character_width,
                character_width,
            ));
            (
                input.x
                    + 10.0
                    + 2.0 * character_width
                    + (prompt.editor.cursor_display_column() - viewport.start_column) as f32
                        * character_width,
                input.y + 4.0,
                input.height - 8.0,
            )
        } else if self.palette_visible {
            let command = self.titlebar_command_bounds();
            let prefix = format!("{} > ", self.language.command_palette());
            let character_width = 14.0 * 0.6;
            let viewport = self.palette_query.viewport(editor_columns(
                command.width - 24.0 - ui::display_width(&prefix) as f32 * character_width,
                character_width,
            ));
            (
                command.x
                    + 12.0
                    + ui::display_width(&prefix) as f32 * character_width
                    + (self.palette_query.cursor_display_column() - viewport.start_column) as f32
                        * character_width,
                command.y + 3.0,
                24.0,
            )
        } else if self.search_visible {
            let width = 430.0_f32
                .min(self.window_size.width as f32 - 32.0)
                .max(220.0);
            let left = (self.window_size.width as f32 - width - 16.0).max(16.0);
            let character_width = 14.0 * 0.6;
            let prefix_columns = ui::display_width(self.language.find()) + 2;
            let viewport = self.search_editor.viewport(editor_columns(
                width - 20.0 - prefix_columns as f32 * character_width,
                character_width,
            ));
            (
                left + 10.0
                    + prefix_columns as f32 * character_width
                    + (self.search_editor.cursor_display_column() - viewport.start_column) as f32
                        * character_width,
                10.0,
                25.0,
            )
        } else if let Some((rect, cursor)) = self.active_terminal_id().and_then(|active| {
            self.pane_rects().into_iter().find_map(|(pane, rect)| {
                let terminal = self.terminal_in_pane(pane)?.1;
                if terminal != active {
                    return None;
                }
                self.terminals
                    .get(&terminal)
                    .and_then(|runtime| runtime.snapshot.cursor)
                    .map(|cursor| (rect, cursor))
            })
        }) {
            (
                rect.x + 8.0 + cursor.x as f32 * self.cell_width(),
                rect.y + 25.0 + cursor.y as f32 * self.cell_height(),
                self.cell_height(),
            )
        } else {
            (
                self.cursor_position.0,
                self.cursor_position.1,
                self.cell_height(),
            )
        };
        renderer.window().set_ime_cursor_area(
            LogicalPosition::new(x.max(0.0), y.max(0.0)),
            LogicalSize::new(2.0, height.max(1.0)),
        );
    }

    fn pointer_over_overlay(&self) -> bool {
        if self.close_confirmation.is_some() || self.codex_trust_confirmation.is_some() {
            return true;
        }
        if self.tab_overflow_menu.is_some()
            || self.tab_context_menu.is_some()
            || self.workspace_context_menu.is_some()
            || self.sidebar_context_menu.is_some()
            || self.terminal_context_menu.is_some()
            || self.notification_context_menu.is_some()
        {
            return true;
        }
        if self.prompt.is_some() || self.palette_visible {
            return true;
        }
        let (x, y) = self.cursor_position;
        if self.search_visible {
            let width = 430.0_f32
                .min(self.window_size.width as f32 - 32.0)
                .max(220.0);
            let left = (self.window_size.width as f32 - width - 16.0).max(16.0);
            if (ui::Bounds {
                x: left,
                y: 8.0,
                width,
                height: 30.0,
            })
            .contains(x, y)
            {
                return true;
            }
        }
        if self.notifications_visible {
            return self.notification_panel().contains(x, y);
        }
        false
    }

    fn update_agent_request_detail_scrollbar_drag(&mut self) -> bool {
        let Some(drag) = self.agent_request_detail_scrollbar_drag else {
            return false;
        };
        let Some((request_id, scroll)) = self.agent_request_detail.clone() else {
            self.agent_request_detail_scrollbar_drag = None;
            return false;
        };
        let Some(layout) = self
            .pending_agent_requests
            .get(&request_id)
            .ok()
            .flatten()
            .map(|request| self.agent_request_detail_layout(&request, scroll))
        else {
            self.agent_request_detail_scrollbar_drag = None;
            return false;
        };
        let Some((track, thumb)) = layout.scrollbar else {
            self.agent_request_detail_scrollbar_drag = None;
            return false;
        };
        let next = scrollbar_scroll_for_pointer(
            track,
            thumb,
            layout.max_scroll,
            self.cursor_position.1,
            drag.grab_y,
        );
        if next != scroll {
            self.set_agent_request_detail_scroll(next);
            self.rebuild_scene();
        }
        true
    }

    fn update_mouse_action(&mut self) {
        if self.update_agent_request_detail_scrollbar_drag() {
            return;
        }
        if self.update_text_edit_drag() {
            return;
        }
        if self.sidebar_drag {
            let maximum = (self.window_size.width as f32 - 320.0).max(160.0);
            self.config.sidebar.width = self.cursor_position.0.clamp(160.0, maximum) as u16;
            self.resize_terminals();
            self.rebuild_scene();
            return;
        }
        if self.update_sidebar_scrollbar_drag() {
            return;
        }
        if self.update_scrollbar_drag() {
            return;
        }
        if self.update_workspace_drag() {
            return;
        }
        if self.terminal_mouse_capture.is_some() {
            self.send_captured_terminal_mouse(
                MouseActionInput::Motion,
                Some(MouseButtonInput::Left),
            );
            return;
        }
        if let Some(drag) = self.split_drag.clone() {
            let (x, y) = self.cursor_position;
            let ratio = match drag.axis {
                SplitAxis::Vertical => (x - drag.container.x) / drag.container.width,
                SplitAxis::Horizontal => (y - drag.container.y) / drag.container.height,
            }
            .clamp(0.1, 0.9);
            if let Some(workspace) = self.state.workspaces.get_mut(self.state.active_workspace) {
                set_split_ratio(&mut workspace.root, &drag.path, ratio);
            }
            self.resize_terminals();
            self.rebuild_scene();
            return;
        }
        let Some(mut selection) = self.selection else {
            return;
        };
        if let Some((pane, terminal, cell)) = self.cell_at_cursor()
            && pane == selection.pane
            && terminal == selection.terminal
            && cell != selection.end
        {
            self.selection_autoscroll_last = None;
            selection.end = cell;
            self.selection = Some(selection);
            let now = std::time::Instant::now();
            if selection_redraw_due(self.selection_last_redraw, now) {
                self.rebuild_scene();
                // Measure the quiet interval after the expensive text layout,
                // so queued pointer events cannot immediately trigger another.
                self.selection_last_redraw = Some(std::time::Instant::now());
            }
        }
    }

    fn begin_workspace_drag(&mut self) -> bool {
        if !self.sidebar_visible || !self.workspace_search.text().trim().is_empty() {
            return false;
        }
        let (x, y) = self.cursor_position;
        if x >= self.sidebar_width() {
            return false;
        }
        let layout = self.sidebar_layout();
        let Some((source_row, row)) = layout
            .rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.bounds.contains(x, y))
        else {
            return false;
        };
        let source_index = row.index;
        let Some(workspace) = self.state.workspaces.get(source_index) else {
            return false;
        };
        self.state.active_workspace = source_index;
        self.workspace_drag = Some(WorkspaceDrag {
            workspace: workspace.id,
            source_index,
            source_row,
            target_index: source_index,
            press_y: y,
            grab_y: y - row.bounds.y,
            active: false,
        });
        self.resize_terminals();
        self.rebuild_scene();
        true
    }

    fn update_workspace_drag(&mut self) -> bool {
        const START_THRESHOLD: f32 = 5.0;

        let Some(mut drag) = self.workspace_drag else {
            return false;
        };
        if !drag.active && (self.cursor_position.1 - drag.press_y).abs() < START_THRESHOLD {
            return true;
        }
        drag.active = true;
        let layout = self.sidebar_layout();
        if !layout.rows.is_empty() {
            let target_row = ui::workspace_drag_target(
                &layout.rows,
                drag.source_row.min(layout.rows.len() - 1),
                self.cursor_position.1,
            );
            drag.target_index = layout.rows[target_row].index;
        }
        self.workspace_drag = Some(drag);
        self.rebuild_scene();
        true
    }

    fn update_selection_autoscroll(&mut self) -> bool {
        const INTERVAL: std::time::Duration = std::time::Duration::from_millis(32);

        let Some(mut selection) = self.selection.filter(|_| self.mouse_pressed) else {
            self.selection_autoscroll_last = None;
            return false;
        };
        if self.text_edit_drag.is_some()
            || self.terminal_mouse_capture.is_some()
            || self.scrollbar_drag.is_some()
            || self.split_drag.is_some()
            || self.sidebar_drag
            || self.sidebar_scrollbar_drag.is_some()
            || self.workspace_drag.is_some()
        {
            self.selection_autoscroll_last = None;
            return false;
        }
        let Some((_, rect)) = self
            .pane_rects()
            .into_iter()
            .find(|(pane, _)| *pane == selection.pane)
        else {
            self.selection_autoscroll_last = None;
            return false;
        };
        let active_terminal = self
            .state
            .panes
            .get(&selection.pane)
            .and_then(|pane| pane.active_surface())
            .and_then(|surface| self.state.surfaces.get(&surface))
            .and_then(|surface| match surface.kind {
                SurfaceKind::Terminal(terminal) => Some(terminal),
                _ => None,
            });
        if active_terminal != Some(selection.terminal) {
            self.selection_autoscroll_last = None;
            return false;
        }

        let content_top = rect.y + 25.0;
        let content_bottom = rect.y + rect.height;
        let Some(lines) = selection_autoscroll_lines(
            self.cursor_position.1,
            content_top,
            content_bottom,
            self.cell_height(),
        ) else {
            self.selection_autoscroll_last = None;
            return false;
        };
        let now = std::time::Instant::now();
        if self
            .selection_autoscroll_last
            .is_some_and(|last| now.saturating_duration_since(last) < INTERVAL)
        {
            return true;
        }
        self.selection_autoscroll_last = Some(now);

        let Some(runtime) = self.terminals.get(&selection.terminal) else {
            return false;
        };
        let snapshot = &runtime.snapshot;
        if snapshot.cells.is_empty() || snapshot.cols == 0 {
            return true;
        }
        let column = (((self.cursor_position.0 - rect.x - 8.0) / self.cell_width()).floor()
            as isize)
            .clamp(0, snapshot.cols as isize - 1) as usize;
        let row = if lines < 0 {
            0
        } else {
            snapshot.cells.len() - 1
        };
        let endpoint = (column, snapshot.screen_row_origin.saturating_add(row));
        let can_scroll = if lines < 0 {
            snapshot.viewport_offset < snapshot.scrollback_rows
        } else {
            snapshot.viewport_offset > 0
        };
        let endpoint_changed = endpoint != selection.end;
        selection.end = endpoint;
        self.selection = Some(selection);
        if can_scroll {
            let _ = runtime.session.send(SessionCommand::Scroll(lines));
        }
        if endpoint_changed && selection_redraw_due(self.selection_last_redraw, now) {
            self.rebuild_scene();
            self.selection_last_redraw = Some(std::time::Instant::now());
        }
        true
    }

    fn update_text_edit_drag(&mut self) -> bool {
        let Some(target) = self.text_edit_drag else {
            return false;
        };
        let (x, _) = self.cursor_position;
        match target {
            TextEditTarget::WorkspaceSearch => {
                let bounds = self.workspace_search_bounds();
                let character_width = 14.0 * 0.6;
                let viewport = self.workspace_search.viewport(editor_columns(
                    bounds.width - 18.0 - 2.0 * character_width,
                    character_width,
                ));
                let column = viewport.start_column
                    + ((x - bounds.x - 26.0).max(0.0) / character_width).round() as usize;
                self.workspace_search.extend_pointer_selection(column);
            }
            TextEditTarget::Prompt => {
                let input = self.prompt_input_bounds();
                let character_width = 15.0 * 0.6;
                if let Some(prompt) = &mut self.prompt {
                    let viewport = prompt.editor.viewport(editor_columns(
                        input.width - 20.0 - 2.0 * character_width,
                        character_width,
                    ));
                    let column = viewport.start_column
                        + ((x - input.x - 10.0 - 2.0 * character_width).max(0.0) / character_width)
                            .round() as usize;
                    prompt.editor.extend_pointer_selection(column);
                }
            }
            TextEditTarget::Palette => {
                let command = self.titlebar_command_bounds();
                let prefix = format!("{} > ", self.language.command_palette());
                let character_width = 14.0 * 0.6;
                let left = command.x + 12.0 + ui::display_width(&prefix) as f32 * character_width;
                let viewport = self.palette_query.viewport(editor_columns(
                    command.width - 24.0 - ui::display_width(&prefix) as f32 * character_width,
                    character_width,
                ));
                let column = viewport.start_column
                    + ((x - left).max(0.0) / character_width).round() as usize;
                self.palette_query.extend_pointer_selection(column);
            }
            TextEditTarget::Search => {
                let width = 430.0_f32
                    .min(self.window_size.width as f32 - 32.0)
                    .max(220.0);
                let left = (self.window_size.width as f32 - width - 16.0).max(16.0);
                let character_width = 14.0 * 0.6;
                let text_left = left
                    + 10.0
                    + (ui::display_width(self.language.find()) + 2) as f32 * character_width;
                let prefix_columns = ui::display_width(self.language.find()) + 2;
                let viewport = self.search_editor.viewport(editor_columns(
                    width - 20.0 - prefix_columns as f32 * character_width,
                    character_width,
                ));
                let column = viewport.start_column
                    + ((x - text_left).max(0.0) / character_width).round() as usize;
                self.search_editor.extend_pointer_selection(column);
            }
        }
        self.rebuild_scene();
        true
    }

    fn finish_mouse_action(&mut self) {
        self.mouse_pressed = false;
        self.selection_last_redraw = None;
        self.selection_autoscroll_last = None;
        if self.agent_request_detail_scrollbar_drag.take().is_some() {
            return;
        }
        if self.text_edit_drag.take().is_some() {
            self.rebuild_scene();
            return;
        }
        if self.sidebar_drag {
            self.sidebar_drag = false;
            self.rebuild_scene();
            return;
        }
        if self.sidebar_scrollbar_drag.take().is_some() {
            self.rebuild_scene();
            return;
        }
        if self.scrollbar_drag.take().is_some() {
            return;
        }
        if let Some(drag) = self.workspace_drag.take() {
            if drag.active && drag.target_index != drag.source_index {
                let _ = self
                    .state
                    .execute(AppCommand::Workspace(WorkspaceCommand::Move {
                        id: drag.workspace,
                        offset: drag.target_index as isize - drag.source_index as isize,
                    }));
                self.resize_terminals();
            }
            self.rebuild_scene();
            return;
        }
        if self.terminal_mouse_capture.is_some() {
            self.send_captured_terminal_mouse(
                MouseActionInput::Release,
                Some(MouseButtonInput::Left),
            );
            self.terminal_mouse_capture = None;
            return;
        }
        let resized_split = self.split_drag.is_some();
        self.split_drag = None;
        if resized_split {
            return;
        }
        let Some(selection) = self.selection else {
            return;
        };
        if selection.start == selection.end {
            self.selection = None;
            self.rebuild_scene();
            return;
        }
        self.copy_selection(ClipboardTarget::Primary);
        // A high-frequency drag may have coalesced its final motion event.
        // Always paint the authoritative endpoint once on release.
        self.rebuild_scene();
    }

    fn cell_at_cursor(&self) -> Option<(PaneId, TerminalId, (usize, usize))> {
        let (x, y) = self.cursor_position;
        let (pane, rect) = self
            .pane_rects()
            .into_iter()
            .find(|(_, rect)| terminal_content_contains(*rect, x, y))?;
        let col = ((x - rect.x - 8.0) / self.cell_width()).floor().max(0.0) as usize;
        let row = ((y - rect.y - 25.0) / self.cell_height()).floor().max(0.0) as usize;
        let pane_state = self.state.panes.get(&pane)?;
        let terminal = pane_state
            .active_surface()
            .and_then(|id| self.state.surfaces.get(&id))
            .and_then(|surface| match surface.kind {
                SurfaceKind::Terminal(id) => Some(id),
                _ => None,
            })?;
        let snapshot = &self.terminals.get(&terminal)?.snapshot;
        let screen_row = snapshot.screen_row_origin.saturating_add(row);
        let col = snapshot
            .cells
            .get(row)
            .map(|cells| wide_cell_lead(cells, col))
            .unwrap_or(col);
        (row < snapshot.cells.len() && col < snapshot.cols as usize).then_some((
            pane,
            terminal,
            (col, screen_row),
        ))
    }

    fn copy_selection(&mut self, target: ClipboardTarget) -> bool {
        let Some(selection) = self
            .selection
            .filter(|selection| selection.start != selection.end)
        else {
            return false;
        };
        let Some(runtime) = self.terminals.get(&selection.terminal) else {
            return false;
        };
        let point = |(column, screen_row): (usize, usize)| SelectionPoint {
            column: column.min(u16::MAX as usize) as u16,
            screen_row: screen_row.min(u32::MAX as usize) as u32,
        };
        let send_result = runtime.session.send(SessionCommand::CopySelection {
            start: point(selection.start),
            end: point(selection.end),
            target,
        });
        if send_result.is_ok() {
            return true;
        }
        let Some(text) = snapshot_selection_text(&runtime.snapshot, selection.start, selection.end)
        else {
            return false;
        };
        self.set_clipboard_text(text, target).is_ok()
    }

    fn copy_all_to_clipboard(&mut self, terminal: TerminalId) -> bool {
        let Some(runtime) = self.terminals.get(&terminal) else {
            return false;
        };
        if runtime.session.send(SessionCommand::CopyAll).is_ok() {
            return true;
        }
        let text = runtime.snapshot.to_plain_text();
        self.set_clipboard_text(text, ClipboardTarget::Clipboard)
            .is_ok()
    }

    fn set_clipboard_text(&mut self, text: String, target: ClipboardTarget) -> Result<(), String> {
        #[cfg(windows)]
        {
            let _ = target;
            return set_windows_clipboard_text(&text).map_err(|error| error.to_string());
        }
        #[cfg(not(windows))]
        {
            if let Some(clipboard) = &mut self.clipboard
                && set_arboard_text(clipboard, text.clone(), target).is_ok()
            {
                return Ok(());
            }
            let mut clipboard = arboard::Clipboard::new().map_err(|error| error.to_string())?;
            set_arboard_text(&mut clipboard, text, target)?;
            self.clipboard = Some(clipboard);
            Ok(())
        }
    }

    fn send_terminal_mouse(
        &mut self,
        action: MouseActionInput,
        button: Option<MouseButtonInput>,
    ) -> bool {
        let (x, y) = self.cursor_position;
        let Some((pane, rect)) = self
            .pane_rects()
            .into_iter()
            .find(|(_, rect)| terminal_content_contains(*rect, x, y))
        else {
            return false;
        };
        let Some(terminal) = self
            .state
            .panes
            .get(&pane)
            .and_then(|pane| pane.active_surface())
            .and_then(|id| self.state.surfaces.get(&id))
            .and_then(|surface| match surface.kind {
                SurfaceKind::Terminal(id) => Some(id),
                _ => None,
            })
        else {
            return false;
        };
        let Some(runtime) = self.terminals.get(&terminal) else {
            return false;
        };
        if !runtime.snapshot.mouse_tracking {
            return false;
        }
        let (mouse_x, mouse_y, cell_width, cell_height) = terminal_mouse_geometry(
            (x - rect.x - 8.0).max(0.0),
            (y - rect.y - 25.0).max(0.0),
            self.cell_width(),
            self.cell_height(),
            self.scale_factor,
        );
        let _ = runtime.session.send(SessionCommand::Mouse(MouseInput {
            action,
            button,
            x: mouse_x,
            y: mouse_y,
            cell_width,
            cell_height,
            modifiers: KeyModifiers {
                shift: self.modifiers.shift_key(),
                alt: self.modifiers.alt_key(),
                control: self.modifiers.control_key(),
                super_key: self.modifiers.super_key(),
            },
            any_button_pressed: self.mouse_pressed,
        }));
        true
    }

    fn terminal_at_cursor(&self) -> Option<(PaneId, TerminalId)> {
        let (x, y) = self.cursor_position;
        let (pane, _) = self
            .pane_rects()
            .into_iter()
            .find(|(_, rect)| terminal_content_contains(*rect, x, y))?;
        self.terminal_in_pane(pane)
    }

    fn terminal_at_point(&self, x: f32, y: f32) -> Option<(PaneId, TerminalId)> {
        let panes = self.pane_rects();
        let bounds: Vec<_> = panes
            .iter()
            .map(|(_, rect)| ui::Bounds {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
            })
            .collect();
        let index = ui::bounds_at(&bounds, x, y)?;
        let pane = panes[index].0;
        self.terminal_in_pane(pane)
    }

    fn terminal_in_pane(&self, pane: PaneId) -> Option<(PaneId, TerminalId)> {
        let terminal = self
            .state
            .panes
            .get(&pane)?
            .active_surface()
            .and_then(|id| self.state.surfaces.get(&id))
            .and_then(|surface| match surface.kind {
                SurfaceKind::Terminal(id) => Some(id),
                _ => None,
            })?;
        Some((pane, terminal))
    }

    fn send_captured_terminal_mouse(
        &mut self,
        action: MouseActionInput,
        button: Option<MouseButtonInput>,
    ) {
        let Some((pane, terminal)) = self.terminal_mouse_capture else {
            return;
        };
        let Some((_, rect)) = self.pane_rects().into_iter().find(|(id, _)| *id == pane) else {
            return;
        };
        let Some(runtime) = self.terminals.get(&terminal) else {
            return;
        };
        let (x, y) = self.cursor_position;
        let local_x = (x - rect.x - 8.0).clamp(0.0, (rect.width - 9.0).max(0.0));
        let local_y = (y - rect.y - 25.0).clamp(0.0, (rect.height - 26.0).max(0.0));
        let (mouse_x, mouse_y, cell_width, cell_height) = terminal_mouse_geometry(
            local_x,
            local_y,
            self.cell_width(),
            self.cell_height(),
            self.scale_factor,
        );
        let _ = runtime.session.send(SessionCommand::Mouse(MouseInput {
            action,
            button,
            x: mouse_x,
            y: mouse_y,
            cell_width,
            cell_height,
            modifiers: KeyModifiers {
                shift: self.modifiers.shift_key(),
                alt: self.modifiers.alt_key(),
                control: self.modifiers.control_key(),
                super_key: self.modifiers.super_key(),
            },
            any_button_pressed: !matches!(action, MouseActionInput::Release),
        }));
    }

    fn scroll_surface_at_cursor(&mut self, delta: isize) {
        let (x, y) = self.cursor_position;
        let Some((pane_id, _)) = self.pane_rects().into_iter().find(|(_, rect)| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y + 25.0 && y < rect.y + rect.height
        }) else {
            return;
        };
        let Some(surface) = self
            .state
            .panes
            .get(&pane_id)
            .and_then(|pane| pane.active_surface())
        else {
            return;
        };
        if let Some(terminal) = self.state.surfaces.get(&surface).and_then(|surface| {
            if let SurfaceKind::Terminal(terminal) = surface.kind {
                Some(terminal)
            } else {
                None
            }
        }) {
            if let Some(runtime) = self.terminals.get(&terminal) {
                let _ = runtime.session.send(SessionCommand::Scroll(delta));
            }
            return;
        }
        if let Some(backend) = self.surface_backends.get(&surface) {
            let line_count =
                if let Some(markdown) = backend.as_any().downcast_ref::<MarkdownSurface>() {
                    markdown_lines(markdown.document(), neutral_document_colors()).len()
                } else if let Some(logs) = backend.as_any().downcast_ref::<LogSurface>() {
                    logs.visible_entries().count()
                } else {
                    backend.render_text().lines().count()
                };
            let offset = self.document_scroll.entry(surface).or_default();
            *offset = offset
                .saturating_add_signed(delta)
                .min(line_count.saturating_sub(1));
            self.rebuild_scene();
        }
    }

    fn paste_clipboard(&mut self, target: ClipboardTarget) {
        let Some(text) = self
            .clipboard
            .as_mut()
            .and_then(|clipboard| get_arboard_text(clipboard, target).ok())
        else {
            return;
        };
        let Some(terminal) = self.active_terminal_id() else {
            return;
        };
        let notifications_changed = self.mark_terminal_notifications_read(terminal);
        self.track_foreground_agent_paste(terminal, &text);
        if let Some(runtime) = self.terminals.get(&terminal) {
            let _ = runtime.session.send(SessionCommand::Paste(text));
        }
        self.reset_cursor_blink();
        if notifications_changed {
            self.rebuild_scene();
        }
    }

    fn track_foreground_agent_paste(&mut self, terminal: TerminalId, text: &str) {
        if self.foreground_agents.contains_key(&terminal) {
            append_bounded_agent_input(
                self.foreground_agent_input.entry(terminal).or_default(),
                text,
            );
        }
    }

    fn resize_terminals(&mut self) {
        let cell_width = self.cell_width();
        let cell_height = self.cell_height();
        let scale_factor = self.scale_factor;
        let cell_metrics_px = (
            (cell_width as f64 * scale_factor).round().max(1.0) as u32,
            (cell_height as f64 * scale_factor).round().max(1.0) as u32,
        );
        for (pane_id, rect) in self.pane_rects() {
            let terminal = self
                .state
                .panes
                .get(&pane_id)
                .and_then(|p| p.active_surface())
                .and_then(|id| self.state.surfaces.get(&id))
                .and_then(|s| match s.kind {
                    SurfaceKind::Terminal(id) => Some(id),
                    _ => None,
                });
            let Some(runtime) = terminal.and_then(|id| self.terminals.get_mut(&id)) else {
                continue;
            };
            let cols = ((rect.width - 16.0).max(cell_width) / cell_width) as u16;
            let rows = ((rect.height - 25.0).max(cell_height) / cell_height) as u16;
            if let Ok(size) = TerminalSize::new(cols.max(1), rows.max(1))
                && (size != runtime.size || cell_metrics_px != runtime.cell_metrics_px)
            {
                let _ = runtime.session.send(SessionCommand::Resize {
                    size,
                    cell_width_px: cell_metrics_px.0,
                    cell_height_px: cell_metrics_px.1,
                });
                runtime.size = size;
                runtime.cell_metrics_px = cell_metrics_px;
            }
        }
    }

    fn rebuild_scene(&mut self) {
        self.scene_dirty = true;
    }

    fn rebuild_scene_if_due(&mut self, now: std::time::Instant) -> bool {
        if !self.scene_dirty
            || !frame_rebuild_due_with_interval(
                self.last_scene_rebuild,
                now,
                adaptive_frame_interval(self.frame_work_estimate),
            )
        {
            return false;
        }
        self.scene_dirty = false;
        self.last_scene_rebuild = Some(now);
        let started = std::time::Instant::now();
        self.rebuild_scene_now();
        self.pending_scene_build_duration = started.elapsed();
        true
    }

    fn scene_rebuild_wait(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        if !self.scene_dirty {
            return None;
        }
        Some(
            self.last_scene_rebuild
                .map_or(std::time::Duration::ZERO, |last| {
                    adaptive_frame_interval(self.frame_work_estimate)
                        .saturating_sub(now.saturating_duration_since(last))
                }),
        )
    }

    fn observe_render_duration(&mut self, render_duration: std::time::Duration) {
        let observed = self
            .pending_scene_build_duration
            .saturating_add(render_duration);
        self.pending_scene_build_duration = std::time::Duration::ZERO;
        self.frame_work_estimate = smooth_frame_work(self.frame_work_estimate, observed);
    }

    fn rebuild_scene_now(&mut self) {
        let Some(renderer) = &self.renderer else {
            return;
        };
        let opacity = if renderer.transparency_active() {
            self.config.window.opacity.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let theme_crust =
            background_opacity(theme_rgba(self.config.theme.sidebar_background), opacity);
        let theme_mantle =
            background_opacity(theme_rgba(self.config.theme.sidebar_background), opacity);
        let theme_base = background_opacity(theme_rgba(self.config.theme.background), opacity);
        let theme_surface =
            background_opacity(theme_rgba(self.config.theme.panel_background), opacity);
        let theme_hover = theme_rgba(self.config.theme.hover_background);
        let theme_selection = theme_rgba(self.config.theme.selection_background);
        let theme_selection_text = theme_rgba(self.config.theme.selection_foreground);
        let theme_cursor = theme_rgba(self.config.theme.cursor);
        let theme_cursor_text = theme_rgba(self.config.theme.cursor_text);
        let theme_border = theme_rgba(self.config.theme.border);
        let theme_text = theme_rgba(self.config.theme.foreground);
        let theme_subtext = theme_rgba(self.config.theme.muted_foreground);
        let theme_weak = theme_rgba(self.config.theme.weak_foreground);
        let theme_blue = theme_rgba(self.config.theme.active_border);
        let theme_lavender = theme_rgba(self.config.theme.cursor);
        let theme_green = theme_rgba(self.config.theme.success);
        let theme_yellow = theme_rgba(self.config.theme.warning);
        let theme_red = theme_rgba(self.config.theme.error);
        let sidebar_layout = self.sidebar_layout();
        self.sidebar_scroll = sidebar_layout.scroll;
        let workspace = self.state.active_workspace();
        let mut blocks = Vec::new();
        let mut rectangles = Vec::new();
        let mut overlay_blocks = Vec::new();
        let mut overlay_rectangles = Vec::new();
        if self.sidebar_visible {
            rectangles.push(RectBlock {
                left: 0.0,
                top: 0.0,
                width: self.sidebar_width(),
                height: self.window_size.height as f32,
                color: theme_mantle,
            });
            let search = self.workspace_search_bounds();
            rectangles.push(RectBlock {
                left: search.x,
                top: search.y,
                width: search.width,
                height: search.height,
                color: if self.workspace_search_focused {
                    theme_blue
                } else {
                    theme_border
                },
            });
            rectangles.push(RectBlock {
                left: search.x + 1.0,
                top: search.y + 1.0,
                width: (search.width - 2.0).max(1.0),
                height: (search.height - 2.0).max(1.0),
                color: theme_crust,
            });
            if self.workspace_search_focused {
                let character_width = 14.0 * 0.6;
                let viewport = self.workspace_search.viewport(editor_columns(
                    search.width - 18.0 - 2.0 * character_width,
                    character_width,
                ));
                push_editor_highlights(
                    &mut rectangles,
                    &self.workspace_search,
                    search.x + 9.0 + 2.0 * character_width,
                    search.y + 3.0,
                    search.height - 6.0,
                    EditorHighlightStyle {
                        character_width,
                        colors: (theme_selection, theme_cursor),
                        viewport,
                    },
                );
            }
            let mut search_spans = vec![TextSpan {
                text: "⌕ ".into(),
                color: theme_text,
                bold: false,
                italic: false,
            }];
            if self.workspace_search_focused {
                search_spans.extend(editor_spans(
                    &self.workspace_search,
                    theme_text,
                    theme_selection_text,
                    theme_cursor_text,
                    self.workspace_search.viewport(editor_columns(
                        search.width - 18.0 - 2.0 * 14.0 * 0.6,
                        14.0 * 0.6,
                    )),
                ));
            } else {
                search_spans.push(TextSpan {
                    text: if self.workspace_search.text().is_empty() {
                        self.language.search_workspaces().into()
                    } else {
                        self.workspace_search.text().into()
                    },
                    color: theme_subtext,
                    bold: false,
                    italic: false,
                });
            }
            blocks.push(TextBlock {
                text: String::new(),
                spans: search_spans,
                left: search.x + 9.0,
                top: search.y + 4.0,
                width: search.width - 18.0,
                height: search.height - 4.0,
                color: if self.workspace_search_focused {
                    theme_text
                } else {
                    theme_subtext
                },
                font_size: 14.0,
                line_height: 21.0,
            });
            for row in &sidebar_layout.rows {
                let item = &self.state.workspaces[row.index];
                let right_padding = if sidebar_layout.scrollbar.is_some() {
                    6.0
                } else {
                    12.0
                };
                let background_right_padding = if sidebar_layout.scrollbar.is_some() {
                    2.0
                } else {
                    6.0
                };
                let active = row.index == self.state.active_workspace;
                let hovered = self.navigation_hover == Some(NavigationHover::Workspace(row.index));
                if active || hovered {
                    rectangles.push(RectBlock {
                        left: row.bounds.x + 6.0,
                        top: row.bounds.y + 1.0,
                        width: (row.bounds.width - 6.0 - background_right_padding).max(1.0),
                        height: row.bounds.height - 2.0,
                        color: if active { theme_selection } else { theme_hover },
                    });
                }
                if active {
                    rectangles.push(RectBlock {
                        left: row.bounds.x + 6.0,
                        top: row.bounds.y + 1.0,
                        width: 3.0,
                        height: row.bounds.height - 2.0,
                        color: theme_blue,
                    });
                }
                rectangles.push(RectBlock {
                    left: row.bounds.x + 12.0,
                    top: row.bounds.y + row.bounds.height - 1.0,
                    width: (row.bounds.width - 12.0 - right_padding).max(1.0),
                    height: 1.0,
                    color: theme_border,
                });
                let unread: Vec<_> = self
                    .state
                    .notifications
                    .iter()
                    .filter(|notification| notification.workspace == item.id && !notification.read)
                    .collect();
                let unread_label = unread_badge_label(unread.len());
                let badge_width = unread_label.as_deref().map_or(0.0, |label| {
                    (ui::display_width(label) as f32 * 11.0 * 0.6 + 8.0).max(18.0)
                });
                let text_left = row.bounds.x
                    + if unread_label.is_some() {
                        16.0 + badge_width + 7.0
                    } else {
                        16.0
                    };
                let text_width =
                    (row.bounds.x + row.bounds.width - right_padding - text_left).max(1.0);
                let text_columns = (text_width / (14.0 * 0.6)).max(1.0) as usize;
                let agent_priority = self.workspace_agent_priority(row.index);
                let subagent_count = self.workspace_claude_subagent_count(row.index);
                let mut agent_state = if subagent_count == 0 {
                    self.agent_state_text(agent_priority).to_owned()
                } else {
                    format!(
                        "{} · {}",
                        self.agent_state_text(agent_priority),
                        self.language.subagent_count(subagent_count)
                    )
                };
                match self.workspace_agent_run_mode(row.index) {
                    AgentRunMode::Interactive => {}
                    AgentRunMode::Unattended => {
                        agent_state.push_str(" · ");
                        agent_state.push_str(self.language.unattended());
                    }
                    AgentRunMode::FullAccess => {
                        agent_state.push_str(" · ");
                        agent_state.push_str(self.language.full_access());
                    }
                }
                let show_agent = agent_priority > 0;
                let agent_color = match agent_priority {
                    5 => theme_red,
                    4 => theme_yellow,
                    3 => theme_blue,
                    2 => theme_green,
                    _ => theme_subtext,
                };
                if let Some(label) = unread_label {
                    let badge = ui::Bounds {
                        x: row.bounds.x + 16.0,
                        y: row.bounds.y + 3.0,
                        width: badge_width,
                        height: 18.0,
                    };
                    let badge_color = if unread
                        .iter()
                        .any(|notification| notification.attention == AttentionState::Error)
                    {
                        theme_red
                    } else if unread
                        .iter()
                        .any(|notification| notification.attention == AttentionState::NeedsInput)
                    {
                        theme_yellow
                    } else {
                        theme_blue
                    };
                    rectangles.push(RectBlock {
                        left: badge.x,
                        top: badge.y,
                        width: badge.width,
                        height: badge.height,
                        color: badge_color,
                    });
                    blocks.push(TextBlock {
                        text: label.clone(),
                        spans: vec![],
                        left: ui::centered_text_left(badge, &label, 11.0),
                        top: badge.y + 1.0,
                        width: badge.width,
                        height: badge.height,
                        color: theme_crust,
                        font_size: 11.0,
                        line_height: 16.0,
                    });
                }
                blocks.push(TextBlock {
                    text: ui::truncate_display_width(&item.name, text_columns),
                    spans: vec![],
                    left: text_left,
                    top: row.bounds.y + 3.0,
                    width: text_width,
                    height: 20.0,
                    color: if active { theme_text } else { theme_subtext },
                    font_size: 14.0,
                    line_height: 19.0,
                });
                let mut next_top = row.bounds.y + 24.0;
                if show_agent {
                    blocks.push(TextBlock {
                        text: String::new(),
                        spans: vec![
                            plain_span(format!("{}: ", self.language.agent()), theme_subtext),
                            plain_span(agent_state, agent_color),
                        ],
                        left: row.bounds.x + 16.0,
                        top: next_top,
                        width: row.bounds.width - 16.0 - right_padding,
                        height: 18.0,
                        color: theme_subtext,
                        font_size: 12.0,
                        line_height: 17.0,
                    });
                    next_top += 19.0;
                }
                if let Some(branch) = &item.git_branch {
                    blocks.push(TextBlock {
                        text: String::new(),
                        spans: vec![
                            plain_span("Git: ", theme_subtext),
                            plain_span(
                                ui::truncate_display_width(branch, text_columns.saturating_sub(5)),
                                theme_green,
                            ),
                        ],
                        left: row.bounds.x + 16.0,
                        top: next_top,
                        width: row.bounds.width - 16.0 - right_padding,
                        height: 18.0,
                        color: theme_subtext,
                        font_size: 12.0,
                        line_height: 17.0,
                    });
                    next_top += 19.0;
                }
                let path_columns =
                    ((row.bounds.width - 16.0 - right_padding) / (12.0 * 0.6)).max(1.0) as usize;
                blocks.push(TextBlock {
                    text: ui::truncate_display_width_start(
                        &item.cwd.to_string_lossy(),
                        path_columns,
                    ),
                    spans: vec![],
                    left: row.bounds.x + 16.0,
                    top: next_top,
                    width: row.bounds.width - 16.0 - right_padding,
                    height: 18.0,
                    color: theme_subtext,
                    font_size: 12.0,
                    line_height: 17.0,
                });
            }
            if let Some((track, thumb)) = sidebar_layout.scrollbar {
                rectangles.push(RectBlock {
                    left: track.x,
                    top: track.y,
                    width: track.width,
                    height: track.height,
                    color: theme_surface,
                });
                rectangles.push(RectBlock {
                    left: thumb.x + 1.0,
                    top: thumb.y,
                    width: thumb.width - 2.0,
                    height: thumb.height,
                    color: if self.sidebar_scrollbar_drag.is_some()
                        || self.navigation_hover == Some(NavigationHover::SidebarScrollbar)
                    {
                        theme_blue
                    } else {
                        theme_subtext
                    },
                });
            }
            rectangles.push(RectBlock {
                left: sidebar_layout.new_workspace.x,
                top: sidebar_layout.new_workspace.y,
                width: sidebar_layout.new_workspace.width,
                height: sidebar_layout.new_workspace.height,
                color: theme_surface,
            });
            rectangles.push(RectBlock {
                left: sidebar_layout.new_workspace.x + 1.0,
                top: sidebar_layout.new_workspace.y + 1.0,
                width: (sidebar_layout.new_workspace.width - 2.0).max(1.0),
                height: sidebar_layout.new_workspace.height - 2.0,
                color: if self.navigation_hover == Some(NavigationHover::NewWorkspace) {
                    theme_hover
                } else {
                    theme_mantle
                },
            });
            blocks.push(TextBlock {
                text: "+".into(),
                spans: vec![],
                left: ui::centered_text_left(sidebar_layout.new_workspace, "+", 18.0),
                top: sidebar_layout.new_workspace.y + 4.0,
                width: 14.0,
                height: 27.0,
                color: theme_text,
                font_size: 18.0,
                line_height: 21.0,
            });
        }
        if workspace.is_none() {
            let button = self.empty_workspace_button();
            let hovered = self.navigation_hover == Some(NavigationHover::EmptyWorkspaceNew);
            let message = self.language.empty_workspace();
            let action = self.language.new_workspace();
            rectangles.push(RectBlock {
                left: button.x,
                top: button.y,
                width: button.width,
                height: button.height,
                color: if hovered { theme_blue } else { theme_border },
            });
            rectangles.push(RectBlock {
                left: button.x + 1.0,
                top: button.y + 1.0,
                width: (button.width - 2.0).max(1.0),
                height: (button.height - 2.0).max(1.0),
                color: if hovered { theme_hover } else { theme_mantle },
            });
            blocks.push(TextBlock {
                text: message.into(),
                spans: vec![],
                left: button.x
                    + (button.width - ui::display_width(message) as f32 * 14.0 * 0.6) / 2.0,
                top: button.y - 34.0,
                width: button.width,
                height: 26.0,
                color: theme_subtext,
                font_size: 14.0,
                line_height: 20.0,
            });
            blocks.push(TextBlock {
                text: action.into(),
                spans: vec![],
                left: button.x
                    + (button.width - ui::display_width(action) as f32 * 14.0 * 0.6) / 2.0,
                top: button.y + 8.0,
                width: button.width - 32.0,
                height: button.height - 8.0,
                color: theme_text,
                font_size: 14.0,
                line_height: 20.0,
            });
        }
        for (pane_id, rect) in self.pane_rects() {
            let active = workspace.is_some_and(|w| w.active_pane == pane_id);
            let surface = self
                .state
                .panes
                .get(&pane_id)
                .and_then(|p| p.active_surface())
                .and_then(|id| self.state.surfaces.get(&id));
            let tab_layout = self.surface_tabs_layout(pane_id, rect);
            rectangles.push(RectBlock {
                left: rect.x,
                top: rect.y,
                width: rect.width,
                height: rect.height,
                color: if active { theme_base } else { theme_crust },
            });
            rectangles.push(RectBlock {
                left: rect.x,
                top: rect.y,
                width: rect.width,
                height: 1.0,
                color: match surface.map(|surface| surface.attention) {
                    Some(AttentionState::NeedsInput) => theme_yellow,
                    Some(AttentionState::Error) => theme_red,
                    Some(AttentionState::Activity) => theme_blue,
                    Some(AttentionState::None) if active => theme_blue,
                    Some(AttentionState::None) | None => theme_border,
                },
            });
            rectangles.push(RectBlock {
                left: rect.x,
                top: rect.y + 1.0,
                width: rect.width,
                height: 24.0,
                color: theme_mantle,
            });
            rectangles.push(RectBlock {
                left: rect.x,
                top: rect.y + 24.0,
                width: rect.width,
                height: 1.0,
                color: theme_border,
            });
            let new_tab = self.surface_new_tab_bounds(rect);
            let new_hovered = self.navigation_hover == Some(NavigationHover::TabNew(pane_id));
            rectangles.push(RectBlock {
                left: new_tab.x + 2.0,
                top: new_tab.y + 2.0,
                width: new_tab.width - 4.0,
                height: new_tab.height - 4.0,
                color: if new_hovered {
                    theme_hover
                } else {
                    theme_mantle
                },
            });
            blocks.push(TextBlock {
                text: "+".into(),
                spans: vec![],
                left: ui::centered_text_left(new_tab, "+", 16.0),
                top: new_tab.y + 3.0,
                width: 14.0,
                height: 21.0,
                color: if new_hovered {
                    theme_text
                } else {
                    theme_subtext
                },
                font_size: 16.0,
                line_height: 20.0,
            });
            if let Some(overflow) = self.surface_tab_overflow_bounds(pane_id, rect) {
                let hidden = self
                    .state
                    .panes
                    .get(&pane_id)
                    .map(|pane| pane.surfaces.len().saturating_sub(tab_layout.len()))
                    .unwrap_or(0);
                let hovered = self.navigation_hover == Some(NavigationHover::TabOverflow(pane_id));
                rectangles.push(RectBlock {
                    left: overflow.x + 2.0,
                    top: overflow.y + 2.0,
                    width: overflow.width - 4.0,
                    height: overflow.height - 4.0,
                    color: if hovered { theme_hover } else { theme_mantle },
                });
                let label = format!("⋯{hidden}");
                blocks.push(TextBlock {
                    text: label.clone(),
                    spans: vec![],
                    left: ui::centered_text_left(overflow, &label, 12.0),
                    top: overflow.y + 4.0,
                    width: overflow.width,
                    height: overflow.height - 4.0,
                    color: if hovered { theme_text } else { theme_subtext },
                    font_size: 12.0,
                    line_height: 18.0,
                });
            }
            let Some(surface) = surface else {
                let button = empty_pane_button(rect);
                let hovered = self.navigation_hover == Some(NavigationHover::EmptyPaneNew(pane_id));
                let message = self.language.empty_pane();
                let action = self
                    .language
                    .action_title("surface.new", "New Terminal Tab");
                rectangles.push(RectBlock {
                    left: button.x,
                    top: button.y,
                    width: button.width,
                    height: button.height,
                    color: if hovered { theme_blue } else { theme_border },
                });
                rectangles.push(RectBlock {
                    left: button.x + 1.0,
                    top: button.y + 1.0,
                    width: (button.width - 2.0).max(1.0),
                    height: (button.height - 2.0).max(1.0),
                    color: if hovered { theme_hover } else { theme_mantle },
                });
                blocks.push(TextBlock {
                    text: message.into(),
                    spans: vec![],
                    left: rect.x
                        + (rect.width - ui::display_width(message) as f32 * 14.0 * 0.6) / 2.0,
                    top: button.y - 34.0,
                    width: (rect.width - 32.0).max(1.0),
                    height: 26.0,
                    color: theme_subtext,
                    font_size: 14.0,
                    line_height: 20.0,
                });
                blocks.push(TextBlock {
                    text: action.into(),
                    spans: vec![],
                    left: button.x
                        + (button.width - ui::display_width(action) as f32 * 14.0 * 0.6) / 2.0,
                    top: button.y + 7.0,
                    width: button.width - 28.0,
                    height: button.height - 8.0,
                    color: theme_text,
                    font_size: 14.0,
                    line_height: 20.0,
                });
                continue;
            };
            if let Some(pane) = self.state.panes.get(&pane_id) {
                for tab in &tab_layout {
                    let Some(surface) = pane
                        .surfaces
                        .get(tab.index)
                        .and_then(|id| self.state.surfaces.get(id))
                    else {
                        continue;
                    };
                    let selected = tab.index == pane.active_surface;
                    let hovered = matches!(
                        self.navigation_hover,
                        Some(NavigationHover::Tab(hover_pane, index))
                            if hover_pane == pane_id && index == tab.index
                    );
                    rectangles.push(RectBlock {
                        left: tab.bounds.x,
                        top: tab.bounds.y + 1.0,
                        width: tab.bounds.width,
                        height: tab.bounds.height - 2.0,
                        color: if selected {
                            theme_selection
                        } else if hovered {
                            theme_hover
                        } else {
                            theme_mantle
                        },
                    });
                    rectangles.push(RectBlock {
                        left: tab.bounds.x + tab.bounds.width - 1.0,
                        top: tab.bounds.y + 5.0,
                        width: 1.0,
                        height: 15.0,
                        color: theme_border,
                    });
                    if selected {
                        rectangles.push(RectBlock {
                            left: tab.bounds.x,
                            top: tab.bounds.y + tab.bounds.height - 2.0,
                            width: tab.bounds.width,
                            height: 2.0,
                            color: theme_blue,
                        });
                    }
                    if matches!(
                        self.navigation_hover,
                        Some(NavigationHover::TabClose(hover_pane, index))
                            if hover_pane == pane_id && index == tab.index
                    ) && let Some(close) = tab.close
                    {
                        rectangles.push(RectBlock {
                            left: close.x,
                            top: close.y + 1.0,
                            width: close.width,
                            height: (close.height - 2.0).max(1.0),
                            color: [theme_red[0], theme_red[1], theme_red[2], 180],
                        });
                    }
                    let attention_color = match surface.attention {
                        AttentionState::NeedsInput => theme_yellow,
                        AttentionState::Error => theme_red,
                        AttentionState::Activity => theme_blue,
                        AttentionState::None if selected => theme_blue,
                        AttentionState::None => theme_subtext,
                    };
                    if surface.attention != AttentionState::None {
                        rectangles.push(RectBlock {
                            left: tab.bounds.x + 2.0,
                            top: tab.bounds.y + 5.0,
                            width: 2.0,
                            height: 14.0,
                            color: attention_color,
                        });
                    }
                    let tab_font_size = self.terminal_font_size.min(12.0);
                    let (title_bounds, number_bounds) = ui::surface_tab_text_bounds(
                        tab.bounds,
                        tab.close,
                        tab.index,
                        tab_font_size * 0.6,
                    );
                    blocks.push(TextBlock {
                        text: String::new(),
                        spans: vec![TextSpan {
                            text: ui::surface_tab_render_title(
                                &surface.title,
                                surface.agent.is_some(),
                                title_bounds.width,
                                tab_font_size * 0.6,
                            ),
                            color: if selected { theme_text } else { theme_subtext },
                            bold: selected,
                            italic: false,
                        }],
                        left: title_bounds.x,
                        top: tab.bounds.y + 3.0,
                        width: title_bounds.width,
                        height: tab.bounds.height - 3.0,
                        color: if selected { theme_text } else { theme_subtext },
                        font_size: tab_font_size,
                        line_height: self.cell_height(),
                    });
                    blocks.push(TextBlock {
                        text: (tab.index + 1).to_string(),
                        spans: vec![],
                        left: number_bounds.x,
                        top: tab.bounds.y + 3.0,
                        width: number_bounds.width,
                        height: tab.bounds.height - 3.0,
                        color: if selected { theme_text } else { theme_subtext },
                        font_size: tab_font_size,
                        line_height: self.cell_height(),
                    });
                    if let Some(close) = tab.close {
                        blocks.push(TextBlock {
                            text: "×".into(),
                            spans: vec![],
                            left: ui::centered_text_left(close, "×", tab_font_size),
                            top: close.y + 3.0,
                            width: close.width,
                            height: close.height - 3.0,
                            color: if selected { theme_text } else { theme_subtext },
                            font_size: tab_font_size,
                            line_height: self.cell_height(),
                        });
                    }
                }
            }
            let mut terminal_wide_glyphs = Vec::new();
            let (text, spans) = match surface.kind {
                SurfaceKind::Terminal(id) => self
                    .terminals
                    .get(&id)
                    .map(|runtime| {
                        let cell_width = self.cell_width();
                        let cell_height = self.cell_height();
                        let terminal_selection = self
                            .selection
                            .filter(|item| item.pane == pane_id && item.terminal == id)
                            .map(|selection| ordered_cells(selection.start, selection.end));
                        for (row, cells) in runtime.snapshot.cells.iter().enumerate() {
                            for (col, cell) in cells.iter().enumerate() {
                                let selected = terminal_selection.is_some_and(|selection| {
                                    terminal_cell_is_selected(
                                        col,
                                        runtime.snapshot.screen_row_origin + row,
                                        selection,
                                    )
                                });
                                let selection_background =
                                    selected.then(|| terminal_selection_background(cells, col));
                                let foreground = selection_background
                                    .map(contrasting_selection_foreground)
                                    .unwrap_or([
                                        cell.foreground.r,
                                        cell.foreground.g,
                                        cell.foreground.b,
                                        255,
                                    ]);
                                if needs_grid_positioned_glyph(cell.text.as_str()) {
                                    let (wide_left_offset, wide_top_offset) =
                                        wide_glyph_optical_offset(
                                            cell.text.as_str(),
                                            cell_width,
                                            self.terminal_font_size,
                                        );
                                    terminal_wide_glyphs.push(TextBlock {
                                        text: String::new(),
                                        spans: vec![TextSpan {
                                            text: cell.text.clone(),
                                            color: foreground,
                                            bold: cell.bold,
                                            italic: cell.italic,
                                        }],
                                        // glyphon constrains a fallback glyph to
                                        // one monospace advance. Center it in
                                        // the VT cell range owned by the wide
                                        // character instead of pinning it to
                                        // the leading edge.
                                        left: rect.x
                                            + 8.0
                                            + col as f32 * cell_width
                                            + wide_left_offset,
                                        top: rect.y
                                            + 25.0
                                            + row as f32 * cell_height
                                            + wide_top_offset,
                                        width: terminal_text_cell_width(cell.text.as_str()) as f32
                                            * cell_width,
                                        height: cell_height,
                                        color: theme_text,
                                        font_size: self.terminal_font_size,
                                        line_height: cell_height,
                                    });
                                }
                                if cell.background_explicit {
                                    rectangles.push(RectBlock {
                                        left: rect.x + 8.0 + col as f32 * cell_width,
                                        top: rect.y + 25.0 + row as f32 * cell_height,
                                        width: cell_width,
                                        height: cell_height,
                                        color: background_opacity(
                                            [
                                                cell.background.r,
                                                cell.background.g,
                                                cell.background.b,
                                                255,
                                            ],
                                            opacity,
                                        ),
                                    });
                                }
                                if cell.underline || cell.strikethrough {
                                    let color = foreground;
                                    if cell.underline {
                                        rectangles.push(RectBlock {
                                            left: rect.x + 8.0 + col as f32 * cell_width,
                                            top: rect.y + 25.0 + (row as f32 + 0.88) * cell_height,
                                            width: cell_width,
                                            height: 1.0,
                                            color,
                                        });
                                    }
                                    if cell.strikethrough {
                                        rectangles.push(RectBlock {
                                            left: rect.x + 8.0 + col as f32 * cell_width,
                                            top: rect.y + 25.0 + (row as f32 + 0.52) * cell_height,
                                            width: cell_width,
                                            height: 1.0,
                                            color,
                                        });
                                    }
                                }
                            }
                        }
                        if self.search_visible && !self.search_editor.text().is_empty() {
                            let needle = self.search_editor.text().to_lowercase();
                            for (row, line) in runtime.snapshot.lines.iter().enumerate() {
                                let lower = line.to_lowercase();
                                for (byte, _) in lower.match_indices(&needle) {
                                    let col = lower[..byte].chars().count();
                                    let width = needle.chars().count().max(1);
                                    rectangles.push(RectBlock {
                                        left: rect.x + 8.0 + col as f32 * cell_width,
                                        top: rect.y + 25.0 + row as f32 * cell_height,
                                        width: width as f32 * cell_width,
                                        height: cell_height,
                                        color: [249, 226, 175, 105],
                                    });
                                }
                            }
                        }
                        if let Some((start, end)) = terminal_selection {
                            let origin = runtime.snapshot.screen_row_origin;
                            let viewport_end = origin
                                .saturating_add(runtime.snapshot.rows.saturating_sub(1) as usize);
                            for screen_row in start.1.max(origin)..=end.1.min(viewport_end) {
                                let row = screen_row - origin;
                                let Some(cells) = runtime.snapshot.cells.get(row) else {
                                    continue;
                                };
                                let from = if screen_row == start.1 { start.0 } else { 0 };
                                let to = if screen_row == end.1 {
                                    end.0
                                } else {
                                    runtime.snapshot.cols as usize - 1
                                };
                                let to =
                                    wide_cell_end(cells, to).min(cells.len().saturating_sub(1));
                                if from > to {
                                    continue;
                                }
                                let mut run_start = from;
                                let mut run_color = terminal_selection_background(cells, from);
                                for col in from + 1..=to {
                                    let color = terminal_selection_background(cells, col);
                                    if color != run_color {
                                        push_terminal_selection_rect(
                                            &mut rectangles,
                                            rect,
                                            row,
                                            run_start,
                                            col - 1,
                                            cell_width,
                                            cell_height,
                                            run_color,
                                        );
                                        run_start = col;
                                        run_color = color;
                                    }
                                }
                                push_terminal_selection_rect(
                                    &mut rectangles,
                                    rect,
                                    row,
                                    run_start,
                                    to,
                                    cell_width,
                                    cell_height,
                                    run_color,
                                );
                            }
                        }
                        if let Some(cursor) = runtime.snapshot.cursor.filter(|cursor| {
                            cursor.visible
                                && (!cursor.blinking || !active || self.cursor_blink_visible)
                        }) {
                            let cursor_width = runtime
                                .snapshot
                                .cells
                                .get(cursor.y as usize)
                                .and_then(|row| row.get(cursor.x as usize))
                                .map(|cell| terminal_text_cell_width(cell.text.as_str()))
                                .unwrap_or(1);
                            rectangles.push(RectBlock {
                                left: rect.x + 8.0 + cursor.x as f32 * cell_width,
                                top: rect.y + 25.0 + cursor.y as f32 * cell_height,
                                width: cursor_width as f32 * cell_width,
                                height: cell_height,
                                color: [
                                    theme_lavender[0],
                                    theme_lavender[1],
                                    theme_lavender[2],
                                    if active && self.window_focused {
                                        theme_lavender[3]
                                    } else {
                                        145
                                    },
                                ],
                            });
                        }
                        if let Some(scrollbar) = ui::terminal_scrollbar_layout(
                            ui::Bounds {
                                x: rect.x,
                                y: rect.y,
                                width: rect.width,
                                height: rect.height,
                            },
                            runtime.snapshot.rows as usize,
                            runtime.snapshot.scrollback_rows,
                            runtime.snapshot.viewport_offset,
                        ) {
                            let scrollbar_hovered = self
                                .scrollbar_at_cursor()
                                .is_some_and(|(_, terminal, _)| terminal == id)
                                || self.scrollbar_drag.is_some_and(|drag| drag.terminal == id);
                            rectangles.push(RectBlock {
                                left: scrollbar.track.x,
                                top: scrollbar.track.y,
                                width: scrollbar.track.width,
                                height: scrollbar.track.height,
                                color: [
                                    theme_subtext[0],
                                    theme_subtext[1],
                                    theme_subtext[2],
                                    if scrollbar_hovered { 55 } else { 28 },
                                ],
                            });
                            rectangles.push(RectBlock {
                                left: scrollbar.thumb.x,
                                top: scrollbar.thumb.y,
                                width: scrollbar.thumb.width,
                                height: scrollbar.thumb.height,
                                color: if scrollbar_hovered {
                                    theme_blue
                                } else {
                                    [theme_subtext[0], theme_subtext[1], theme_subtext[2], 150]
                                },
                            });
                        }
                        let mut spans = Vec::new();
                        for (row_index, row) in runtime.snapshot.cells.iter().enumerate() {
                            for (col, cell) in row.iter().enumerate() {
                                let text = if cell.text.is_empty()
                                    || needs_grid_positioned_glyph(cell.text.as_str())
                                {
                                    " "
                                } else {
                                    cell.text.as_str()
                                };
                                let color = if terminal_selection.is_some_and(|selection| {
                                    terminal_cell_is_selected(
                                        col,
                                        runtime.snapshot.screen_row_origin + row_index,
                                        selection,
                                    )
                                }) {
                                    contrasting_selection_foreground(terminal_selection_background(
                                        row, col,
                                    ))
                                } else {
                                    [cell.foreground.r, cell.foreground.g, cell.foreground.b, 255]
                                };
                                push_terminal_span(&mut spans, text, color, cell.bold, cell.italic);
                            }
                            if row_index + 1 < runtime.snapshot.cells.len() {
                                push_terminal_span(&mut spans, "\n", theme_text, false, false);
                            }
                        }
                        (String::new(), spans)
                    })
                    .unwrap_or_default(),
                SurfaceKind::Markdown(_) => {
                    let offset = self.document_scroll.get(&surface.id).copied().unwrap_or(0);
                    let visible_rows =
                        ((rect.height - 30.0) / self.cell_height()).max(1.0) as usize;
                    let palette = DocumentColors {
                        text: if active { theme_text } else { theme_subtext },
                        muted: theme_subtext,
                        accent: theme_blue,
                        heading: theme_blue,
                        code: theme_green,
                        warning: theme_yellow,
                        error: theme_red,
                    };
                    let mut spans = Vec::new();
                    if let Some(markdown) = self
                        .surface_backends
                        .get(&surface.id)
                        .and_then(|backend| backend.as_any().downcast_ref::<MarkdownSurface>())
                    {
                        spans.extend(markdown_spans(
                            markdown.document(),
                            offset,
                            visible_rows,
                            palette,
                            self.search_visible.then_some(self.search_editor.text()),
                        ));
                    } else {
                        spans.push(plain_span(
                            self.language.load_surface_failed(),
                            palette.error,
                        ));
                    }
                    (String::new(), spans)
                }
                SurfaceKind::Logs(_) => {
                    let offset = self.document_scroll.get(&surface.id).copied().unwrap_or(0);
                    let visible_rows =
                        ((rect.height - 30.0) / self.cell_height()).max(1.0) as usize;
                    let palette = DocumentColors {
                        text: if active { theme_text } else { theme_subtext },
                        muted: theme_subtext,
                        accent: theme_blue,
                        heading: theme_blue,
                        code: theme_green,
                        warning: theme_yellow,
                        error: theme_red,
                    };
                    let mut spans = Vec::new();
                    if let Some(logs) = self
                        .surface_backends
                        .get(&surface.id)
                        .and_then(|backend| backend.as_any().downcast_ref::<LogSurface>())
                    {
                        let filter_label = format_log_filter(logs.filter());
                        let content_rows = if filter_label.is_empty() {
                            visible_rows
                        } else {
                            spans.push(TextSpan {
                                text: format!("FILTER: {filter_label}  [Ctrl+Shift+L]\n"),
                                color: palette.accent,
                                bold: true,
                                italic: false,
                            });
                            visible_rows.saturating_sub(1)
                        };
                        spans.extend(log_spans(
                            logs,
                            offset,
                            content_rows,
                            palette,
                            self.search_visible.then_some(self.search_editor.text()),
                        ));
                    } else {
                        spans.push(plain_span(
                            self.language.load_surface_failed(),
                            palette.error,
                        ));
                    }
                    (String::new(), spans)
                }
                SurfaceKind::Browser(_) => (self.language.browser_excluded().into(), vec![]),
            };
            blocks.push(TextBlock {
                text,
                spans,
                left: rect.x + 8.0,
                top: rect.y + 25.0,
                width: rect.width - 14.0,
                height: rect.height
                    - 29.0
                    - if matches!(surface.kind, SurfaceKind::Terminal(terminal) if self.terminal_notices.contains_key(&terminal)) {
                        self.cell_height() + 8.0
                    } else {
                        0.0
                    },
                color: if active { theme_text } else { theme_subtext },
                font_size: self.terminal_font_size,
                line_height: self.cell_height(),
            });
            blocks.append(&mut terminal_wide_glyphs);
            if let SurfaceKind::Terminal(terminal) = surface.kind
                && let Some(notice) = self.terminal_notices.get(&terminal)
            {
                let message = match notice {
                    TerminalNotice::Exited(status) => self.language.terminal_exited(status),
                    TerminalNotice::Warning(warning) => self.language.terminal_warning(warning),
                    TerminalNotice::Error(error) => self.language.terminal_error(error),
                    TerminalNotice::SpawnFailed(error) => {
                        self.language.terminal_spawn_failed(error)
                    }
                };
                let notice_height = self.cell_height() + 8.0;
                let notice_top = rect.y + rect.height - notice_height;
                rectangles.push(RectBlock {
                    left: rect.x + 4.0,
                    top: notice_top,
                    width: rect.width - 8.0,
                    height: notice_height,
                    color: [theme_surface[0], theme_surface[1], theme_surface[2], 248],
                });
                blocks.push(TextBlock {
                    text: ui::truncate(&message, ((rect.width - 20.0) / 8.0).max(8.0) as usize),
                    spans: vec![],
                    left: rect.x + 10.0,
                    top: notice_top + 3.0,
                    width: rect.width - 20.0,
                    height: self.cell_height(),
                    color: match notice {
                        TerminalNotice::Exited(_) | TerminalNotice::Warning(_) => theme_yellow,
                        TerminalNotice::Error(_) | TerminalNotice::SpawnFailed(_) => theme_red,
                    },
                    font_size: self.terminal_font_size,
                    line_height: self.cell_height(),
                });
            }
        }
        if let Some(workspace) = workspace
            && !self
                .state
                .panes
                .get(&workspace.active_pane)
                .is_some_and(|pane| pane.zoomed)
        {
            let area = terminal_workspace_area(
                self.window_size.width as f32,
                self.window_size.height as f32,
                self.sidebar_width(),
            );
            let mut dividers = Vec::new();
            collect_dividers(&workspace.root, area, &mut Vec::new(), &mut dividers);
            let hovered = divider_at(
                &workspace.root,
                area,
                self.cursor_position.0,
                self.cursor_position.1,
                &mut Vec::new(),
            );
            for divider in dividers {
                let dragging = self
                    .split_drag
                    .as_ref()
                    .is_some_and(|active| active.path == divider.path);
                let highlighted = dragging
                    || hovered
                        .as_ref()
                        .is_some_and(|active| active.path == divider.path);
                let line = divider_line(
                    &divider,
                    if dragging {
                        4.0
                    } else if highlighted {
                        2.0
                    } else {
                        1.0
                    },
                );
                rectangles.push(RectBlock {
                    left: line.x,
                    top: line.y,
                    width: line.width,
                    height: line.height,
                    color: if highlighted {
                        theme_blue
                    } else {
                        theme_subtext
                    },
                });
            }
        }
        if self.sidebar_visible && self.sidebar_width() > 0.0 {
            let highlighted = self.sidebar_drag
                || (!self.pointer_over_overlay()
                    && self.sidebar_resize_hit(self.cursor_position.0, self.cursor_position.1));
            let edge = ui::sidebar_edge_bounds(
                self.sidebar_width(),
                self.window_size.height as f32,
                if highlighted { 3.0 } else { 1.0 },
            );
            // Paint after all panes, but below UI overlays. Both sides of the
            // stroke reach the bottom equally; no exposed 4px stub remains.
            rectangles.push(RectBlock {
                left: edge.x,
                top: edge.y,
                width: edge.width,
                height: edge.height,
                color: if highlighted {
                    theme_blue
                } else {
                    theme_border
                },
            });
        }
        if self.search_visible {
            let status = self
                .search_result
                .as_ref()
                .map(|result| format!("{}/{}", result.current, result.total))
                .unwrap_or_else(|| "0/0".into());
            let width = 430.0_f32
                .min(self.window_size.width as f32 - 32.0)
                .max(220.0);
            let left = (self.window_size.width as f32 - width - 16.0).max(16.0);
            overlay_rectangles.push(RectBlock {
                left,
                top: 8.0,
                width,
                height: 30.0,
                color: theme_blue,
            });
            overlay_rectangles.push(RectBlock {
                left: left + 1.0,
                top: 9.0,
                width: width - 2.0,
                height: 28.0,
                color: theme_mantle,
            });
            let character_width = 14.0 * 0.6;
            let search_prefix = format!("{}: ", self.language.find());
            let search_viewport = self.search_editor.viewport(editor_columns(
                width - 20.0 - ui::display_width(&search_prefix) as f32 * character_width,
                character_width,
            ));
            push_editor_highlights(
                &mut overlay_rectangles,
                &self.search_editor,
                left + 10.0 + ui::display_width(&search_prefix) as f32 * character_width,
                10.0,
                26.0,
                EditorHighlightStyle {
                    character_width,
                    colors: (theme_selection, theme_cursor),
                    viewport: search_viewport,
                },
            );
            let mut search_spans = vec![TextSpan {
                text: search_prefix,
                color: theme_text,
                bold: false,
                italic: false,
            }];
            search_spans.extend(editor_spans(
                &self.search_editor,
                theme_text,
                theme_selection_text,
                theme_cursor_text,
                search_viewport,
            ));
            search_spans.push(TextSpan {
                text: format!("   {status}   {}", self.language.find_navigation()),
                color: theme_text,
                bold: false,
                italic: false,
            });
            overlay_blocks.push(TextBlock {
                text: String::new(),
                spans: search_spans,
                left: left + 10.0,
                top: 13.0,
                width: width - 20.0,
                height: 24.0,
                color: theme_text,
                font_size: 14.0,
                line_height: 19.0,
            });
        }
        if let Some(confirmation) = &self.codex_trust_confirmation {
            let panel = self.confirmation_panel();
            let (cancel, proceed, trust) = self.codex_trust_buttons();
            overlay_rectangles.push(RectBlock {
                left: 0.0,
                top: 40.0,
                width: self.window_size.width as f32,
                height: (self.window_size.height as f32 - 40.0).max(1.0),
                color: [0, 0, 0, 150],
            });
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_blue));
            for (bounds, hovered, color) in [
                (
                    cancel,
                    self.navigation_hover == Some(NavigationHover::ConfirmationCancel),
                    theme_hover,
                ),
                (
                    proceed,
                    self.navigation_hover == Some(NavigationHover::ConfirmationContinue),
                    theme_hover,
                ),
                (
                    trust,
                    self.navigation_hover == Some(NavigationHover::ConfirmationConfirm),
                    theme_blue,
                ),
            ] {
                overlay_rectangles.push(RectBlock {
                    left: bounds.x,
                    top: bounds.y,
                    width: bounds.width,
                    height: bounds.height,
                    color: if hovered {
                        brighten_rgba(color, 28)
                    } else {
                        color
                    },
                });
            }
            overlay_blocks.push(TextBlock {
                text: self.language.codex_project_trust_title().into(),
                spans: vec![],
                left: panel.x + 20.0,
                top: panel.y + 14.0,
                width: panel.width - 40.0,
                height: 24.0,
                color: theme_blue,
                font_size: 16.0,
                line_height: 22.0,
            });
            let mut message = self
                .language
                .codex_project_trust_warning(&confirmation.project_root);
            if let Some(error) = &confirmation.error {
                message.push('\n');
                message.push_str(&self.language.codex_project_trust_error(error));
            }
            overlay_blocks.push(TextBlock {
                text: message,
                spans: vec![],
                left: panel.x + 20.0,
                top: panel.y + 42.0,
                width: panel.width - 40.0,
                height: 62.0,
                color: if confirmation.error.is_some() {
                    theme_red
                } else {
                    theme_text
                },
                font_size: 13.0,
                line_height: 18.0,
            });
            for (bounds, label) in [
                (cancel, self.language.cancel()),
                (proceed, self.language.continue_without_trust()),
                (trust, self.language.trust_and_launch()),
            ] {
                let text_width = ui::display_width(label) as f32 * 13.0 * 0.6;
                overlay_blocks.push(TextBlock {
                    text: label.into(),
                    spans: vec![],
                    left: bounds.x + (bounds.width - text_width).max(0.0) / 2.0,
                    top: bounds.y + 7.0,
                    width: text_width.max(1.0),
                    height: 22.0,
                    color: theme_text,
                    font_size: 13.0,
                    line_height: 20.0,
                });
            }
        }
        if let Some(confirmation) = &self.close_confirmation {
            let panel = self.confirmation_panel();
            let (cancel, confirm) = self.confirmation_buttons();
            let cancel_hovered = self.navigation_hover == Some(NavigationHover::ConfirmationCancel);
            let confirm_hovered =
                self.navigation_hover == Some(NavigationHover::ConfirmationConfirm);
            overlay_rectangles.push(RectBlock {
                left: 0.0,
                top: 40.0,
                width: self.window_size.width as f32,
                height: (self.window_size.height as f32 - 40.0).max(1.0),
                color: [0, 0, 0, 150],
            });
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_blue));
            for (bounds, color) in [
                (
                    cancel,
                    if cancel_hovered {
                        theme_selection
                    } else {
                        theme_hover
                    },
                ),
                (
                    confirm,
                    if confirm_hovered {
                        brighten_rgba(theme_red, 28)
                    } else {
                        theme_red
                    },
                ),
            ] {
                overlay_rectangles.push(RectBlock {
                    left: bounds.x,
                    top: bounds.y,
                    width: bounds.width,
                    height: bounds.height,
                    color,
                });
            }
            overlay_blocks.push(TextBlock {
                text: confirmation.title.clone(),
                spans: vec![],
                left: panel.x + 20.0,
                top: panel.y + 16.0,
                width: panel.width - 40.0,
                height: 24.0,
                color: theme_red,
                font_size: 16.0,
                line_height: 22.0,
            });
            overlay_blocks.push(TextBlock {
                text: confirmation.message.clone(),
                spans: vec![],
                left: panel.x + 20.0,
                top: panel.y + 48.0,
                width: panel.width - 40.0,
                height: 48.0,
                color: theme_text,
                font_size: 14.0,
                line_height: 20.0,
            });
            for (bounds, text, color) in [
                (cancel, self.language.cancel(), theme_text),
                (
                    confirm,
                    if matches!(confirmation.target, CloseTarget::FullAccess(_)) {
                        self.language.confirm()
                    } else {
                        self.language.close_anyway()
                    },
                    theme_text,
                ),
            ] {
                let text_width = ui::display_width(text) as f32 * 14.0 * 0.6;
                overlay_blocks.push(TextBlock {
                    text: text.into(),
                    spans: vec![],
                    left: bounds.x + (bounds.width - text_width).max(0.0) / 2.0,
                    top: bounds.y + 7.0,
                    width: text_width.max(1.0),
                    height: 22.0,
                    color,
                    font_size: 14.0,
                    line_height: 20.0,
                });
            }
        }
        if let Some(prompt) = &self.prompt {
            let panel = self.prompt_panel();
            let input = self.prompt_input_bounds();
            let label = match prompt.kind {
                PromptKind::RenameWorkspace => self.language.rename_workspace().to_owned(),
                PromptKind::DescribeWorkspace => self.language.workspace_description().to_owned(),
                PromptKind::ChangeWorkspaceDirectory => {
                    self.language.change_workspace_directory().to_owned()
                }
                PromptKind::FindDirectory => self.language.find_workspace().to_owned(),
                PromptKind::FilterLogs => self.language.filter_logs().to_owned(),
            };
            overlay_rectangles.push(RectBlock {
                left: 0.0,
                top: 40.0,
                width: self.window_size.width as f32,
                height: (self.window_size.height as f32 - 40.0).max(1.0),
                color: [0, 0, 0, 125],
            });
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_blue));
            overlay_rectangles.push(RectBlock {
                left: input.x,
                top: input.y,
                width: input.width,
                height: input.height,
                color: if prompt.error.is_some() {
                    theme_red
                } else {
                    theme_border
                },
            });
            overlay_rectangles.push(RectBlock {
                left: input.x + 1.0,
                top: input.y + 1.0,
                width: (input.width - 2.0).max(1.0),
                height: (input.height - 2.0).max(1.0),
                color: theme_surface,
            });
            let character_width = 15.0 * 0.6;
            let prompt_viewport = prompt.editor.viewport(editor_columns(
                input.width - 20.0 - 2.0 * character_width,
                character_width,
            ));
            push_editor_highlights(
                &mut overlay_rectangles,
                &prompt.editor,
                input.x + 10.0 + 2.0 * character_width,
                input.y + 3.0,
                input.height - 6.0,
                EditorHighlightStyle {
                    character_width,
                    colors: (theme_selection, theme_cursor),
                    viewport: prompt_viewport,
                },
            );
            overlay_blocks.push(TextBlock {
                text: label,
                spans: vec![],
                left: panel.x + 18.0,
                top: panel.y + 14.0,
                width: panel.width - 36.0,
                height: 24.0,
                color: theme_blue,
                font_size: 14.0,
                line_height: 20.0,
            });
            let mut prompt_spans = vec![TextSpan {
                text: "› ".into(),
                color: theme_blue,
                bold: true,
                italic: false,
            }];
            prompt_spans.extend(editor_spans(
                &prompt.editor,
                theme_text,
                theme_selection_text,
                theme_cursor_text,
                prompt_viewport,
            ));
            overlay_blocks.push(TextBlock {
                text: String::new(),
                spans: prompt_spans,
                left: input.x + 10.0,
                top: input.y + 7.0,
                width: input.width - 20.0,
                height: input.height - 8.0,
                color: theme_text,
                font_size: 15.0,
                line_height: 21.0,
            });
            if let Some(error) = &prompt.error {
                overlay_blocks.push(TextBlock {
                    text: error.clone(),
                    spans: vec![],
                    left: panel.x + 18.0,
                    top: input.y + input.height + 9.0,
                    width: panel.width - 36.0,
                    height: 24.0,
                    color: theme_red,
                    font_size: 13.0,
                    line_height: 18.0,
                });
            }
        }
        if self.palette_visible {
            let registry = ActionRegistry::default();
            let actions = self.palette_actions(&registry);
            let capacity =
                (((self.window_size.height as f32 - 150.0) / 25.0) as usize).clamp(3, 12);
            let visible = ui::scrolled_range(actions.len(), self.palette_scroll, capacity);
            let panel = self.command_palette_panel(actions.len());
            overlay_rectangles.push(RectBlock {
                left: panel.x,
                top: panel.y,
                width: panel.width,
                height: panel.height,
                color: theme_blue,
            });
            overlay_rectangles.push(RectBlock {
                left: panel.x + 1.0,
                top: panel.y + 1.0,
                width: (panel.width - 2.0).max(1.0),
                height: (panel.height - 2.0).max(1.0),
                color: theme_mantle,
            });
            let palette_rows = ui::palette_rows(panel, visible.len());
            if let Some((track, thumb)) =
                ui::list_scrollbar(panel, actions.len(), visible.len(), self.palette_scroll)
            {
                overlay_rectangles.push(RectBlock {
                    left: track.x,
                    top: track.y,
                    width: track.width,
                    height: track.height,
                    color: theme_crust,
                });
                overlay_rectangles.push(RectBlock {
                    left: thumb.x,
                    top: thumb.y,
                    width: thumb.width,
                    height: thumb.height,
                    color: theme_subtext,
                });
            }
            let highlighted = self
                .palette_hover
                .filter(|index| visible.contains(index))
                .unwrap_or(self.palette_index);
            if visible.contains(&highlighted) {
                let selected_row = highlighted.saturating_sub(visible.start);
                let selected = palette_rows[selected_row];
                overlay_rectangles.push(RectBlock {
                    left: selected.x,
                    top: selected.y,
                    width: selected.width,
                    height: selected.height,
                    color: theme_selection,
                });
                overlay_rectangles.push(RectBlock {
                    left: selected.x,
                    top: selected.y,
                    width: 3.0,
                    height: selected.height,
                    color: theme_blue,
                });
            }
            for (index, action) in actions[visible.clone()].iter().enumerate() {
                let absolute = visible.start + index;
                let selected = absolute == highlighted;
                let shortcut = self.shortcut_for_action(action.id());
                let shortcut_width = shortcut
                    .as_deref()
                    .map_or(0.0, |label| ui::display_width(label) as f32 * 13.0 * 0.6);
                let spans = if selected {
                    vec![
                        TextSpan {
                            text: "> ".into(),
                            color: theme_blue,
                            bold: true,
                            italic: false,
                        },
                        TextSpan {
                            text: self
                                .language
                                .action_title(action.id(), action.title())
                                .into(),
                            color: theme_text,
                            bold: true,
                            italic: false,
                        },
                    ]
                } else {
                    vec![TextSpan {
                        text: format!(
                            "  {}",
                            self.language.action_title(action.id(), action.title())
                        ),
                        color: theme_text,
                        bold: false,
                        italic: false,
                    }]
                };
                let row = palette_rows[index];
                overlay_blocks.push(TextBlock {
                    text: String::new(),
                    spans,
                    left: panel.x + 18.0,
                    top: row.y,
                    width: (panel.width - 48.0 - shortcut_width).max(40.0),
                    height: row.height,
                    color: theme_text,
                    font_size: 16.0,
                    line_height: row.height,
                });
                if let Some(shortcut) = shortcut {
                    overlay_blocks.push(TextBlock {
                        text: shortcut,
                        spans: vec![],
                        left: panel.x + panel.width - 18.0 - shortcut_width,
                        top: row.y,
                        width: shortcut_width.max(1.0),
                        height: row.height,
                        color: if selected { theme_blue } else { theme_subtext },
                        font_size: 13.0,
                        line_height: row.height,
                    });
                }
            }
        }
        if self.notifications_visible
            && self.close_confirmation.is_none()
            && self.codex_trust_confirmation.is_none()
            && self.prompt.is_none()
            && !self.palette_visible
        {
            let panel = self.notification_panel();
            let pending_requests = self.pending_agent_request_list();
            let notification_panel = self.notification_list_panel(pending_requests.len());
            let visible = self.notification_range();
            let notification_rows = ui::notification_row_layouts(notification_panel, visible.len());
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_blue));
            if !pending_requests.is_empty() {
                let request_range = self.agent_request_range(pending_requests.len());
                let action_sets = pending_requests[request_range.clone()]
                    .iter()
                    .map(agent_approval_actions)
                    .collect::<Vec<_>>();
                let action_counts = action_sets
                    .iter()
                    .map(|actions| actions.len())
                    .collect::<Vec<_>>();
                let cards = ui::agent_approval_cards(
                    self.agent_approval_region(pending_requests.len()),
                    &action_counts,
                );
                overlay_blocks.push(TextBlock {
                    text: format!(
                        "{}  {}/{}",
                        self.language.agent_approval(),
                        request_range.start + 1,
                        pending_requests.len()
                    ),
                    spans: vec![],
                    left: panel.x + 18.0,
                    top: panel.y + 14.0,
                    width: panel.width - 36.0,
                    height: 24.0,
                    color: theme_yellow,
                    font_size: 14.0,
                    line_height: 21.0,
                });
                for (card_index, card) in cards.iter().enumerate() {
                    let absolute = request_range.start + card_index;
                    let request = &pending_requests[absolute];
                    let selected = self.agent_request_selection == absolute;
                    overlay_rectangles.push(RectBlock {
                        left: card.bounds.x,
                        top: card.bounds.y,
                        width: card.bounds.width,
                        height: card.bounds.height,
                        color: if selected {
                            theme_selection
                        } else {
                            theme_mantle
                        },
                    });
                    overlay_rectangles.push(RectBlock {
                        left: card.bounds.x,
                        top: card.bounds.y,
                        width: 3.0,
                        height: card.bounds.height,
                        color: theme_yellow,
                    });
                    overlay_blocks.push(TextBlock {
                        text: format!(
                            "{} · {}",
                            self.agent_request_source(request),
                            self.language.details()
                        ),
                        spans: vec![],
                        left: card.content.x,
                        top: card.content.y,
                        width: card.content.width,
                        height: 20.0,
                        color: theme_blue,
                        font_size: 13.0,
                        line_height: 18.0,
                    });
                    let columns = (card.content.width / (13.0 * 0.6)).floor() as usize;
                    for (line, text) in ui::notification_preview(&request.message, columns, 2, None)
                        .into_iter()
                        .enumerate()
                    {
                        overlay_blocks.push(TextBlock {
                            text,
                            spans: vec![],
                            left: card.content.x,
                            top: card.content.y + 22.0 + line as f32 * 18.0,
                            width: card.content.width,
                            height: 18.0,
                            color: theme_text,
                            font_size: 13.0,
                            line_height: 17.0,
                        });
                    }
                    for (action_index, (action, button)) in action_sets[card_index]
                        .iter()
                        .zip(&card.buttons)
                        .enumerate()
                    {
                        let hovered = self.navigation_hover
                            == Some(NavigationHover::AgentApproval(absolute, action_index));
                        let keyboard_selected =
                            selected && self.agent_request_action == action_index;
                        overlay_rectangles.push(RectBlock {
                            left: button.x,
                            top: button.y,
                            width: button.width,
                            height: button.height,
                            color: if hovered || keyboard_selected {
                                if matches!(action, AgentApprovalAction::Deny) {
                                    theme_red
                                } else {
                                    theme_blue
                                }
                            } else {
                                theme_hover
                            },
                        });
                        let label = agent_approval_action_label(self.language, *action);
                        let font_size = 12.0;
                        let (label, left) =
                            ui::centered_truncated_text_left(*button, label, font_size);
                        overlay_blocks.push(TextBlock {
                            text: label,
                            spans: vec![],
                            left,
                            top: button.y + 4.0,
                            width: button.width,
                            height: button.height - 4.0,
                            color: theme_text,
                            font_size,
                            line_height: 19.0,
                        });
                    }
                }
            }
            overlay_blocks.push(TextBlock {
                text: format!(
                    "{}  {}  {}/{}",
                    self.language.needs_attention(),
                    self.state
                        .notifications
                        .iter()
                        .filter(|item| !item.read)
                        .count(),
                    visible
                        .start
                        .saturating_add(1)
                        .min(self.state.notifications.len()),
                    self.state.notifications.len()
                ),
                spans: vec![],
                left: notification_panel.x + 18.0,
                top: notification_panel.y + 16.0,
                width: (notification_panel.width - 154.0).max(1.0),
                height: 24.0,
                color: theme_text,
                font_size: 14.0,
                line_height: 21.0,
            });
            let mark_all_read = ui::notification_mark_all_read_button(notification_panel);
            let has_unread = self.has_unread_notifications();
            let mark_all_read_hovered =
                self.navigation_hover == Some(NavigationHover::NotificationMarkAllRead);
            overlay_rectangles.push(RectBlock {
                left: mark_all_read.x,
                top: mark_all_read.y,
                width: mark_all_read.width,
                height: mark_all_read.height,
                color: if mark_all_read_hovered {
                    theme_blue
                } else if has_unread {
                    theme_hover
                } else {
                    theme_mantle
                },
            });
            let mark_all_read_label = self.language.mark_all_read();
            overlay_blocks.push(TextBlock {
                text: mark_all_read_label.into(),
                spans: vec![],
                left: ui::centered_text_left(mark_all_read, mark_all_read_label, 12.0),
                top: mark_all_read.y + 4.0,
                width: mark_all_read.width,
                height: mark_all_read.height - 4.0,
                color: if has_unread {
                    theme_text
                } else {
                    theme_subtext
                },
                font_size: 12.0,
                line_height: 19.0,
            });
            let now_ms = unix_time_ms();
            for (row_index, item) in self
                .state
                .notifications
                .iter()
                .rev()
                .skip(visible.start)
                .take(visible.len())
                .enumerate()
            {
                let row = notification_rows[row_index];
                let absolute = visible.start + row_index;
                let hovered = matches!(
                    self.navigation_hover,
                    Some(NavigationHover::Notification(index) | NavigationHover::NotificationClose(index))
                        if index == absolute
                );
                let close_hovered =
                    self.navigation_hover == Some(NavigationHover::NotificationClose(absolute));
                if hovered || self.notification_selection == absolute {
                    overlay_rectangles.push(RectBlock {
                        left: row.bounds.x,
                        top: row.bounds.y,
                        width: row.bounds.width,
                        height: row.bounds.height,
                        color: if close_hovered {
                            theme_selection
                        } else {
                            theme_hover
                        },
                    });
                }
                if !item.read {
                    overlay_rectangles.push(RectBlock {
                        left: row.accent.x,
                        top: row.accent.y,
                        width: row.accent.width,
                        height: row.accent.height,
                        color: match item.attention {
                            AttentionState::NeedsInput => theme_yellow,
                            AttentionState::Error => theme_red,
                            _ => theme_blue,
                        },
                    });
                }
                if hovered {
                    if close_hovered {
                        overlay_rectangles.push(RectBlock {
                            left: row.close.x,
                            top: row.close.y,
                            width: row.close.width,
                            height: row.close.height,
                            color: theme_red,
                        });
                    }
                    overlay_blocks.push(TextBlock {
                        text: "×".into(),
                        spans: vec![],
                        left: ui::centered_text_left(row.close, "×", 16.0),
                        top: row.close.y,
                        width: row.close.width,
                        height: row.close.height,
                        color: theme_text,
                        font_size: 16.0,
                        line_height: row.close.height,
                    });
                }
                let age = notification_age_label(
                    now_ms,
                    item.created_at_unix_ms,
                    self.language.is_chinese(),
                );
                let age_width = ui::display_width(&age) as f32 * 12.0 * 0.6;
                overlay_blocks.push(TextBlock {
                    text: ui::truncate_display_width(&item.title, 32),
                    spans: vec![],
                    left: row.content.x,
                    top: row.content.y,
                    width: (row.content.width - age_width - 8.0).max(24.0),
                    height: 22.0,
                    color: theme_text,
                    font_size: 14.0,
                    line_height: 20.0,
                });
                overlay_blocks.push(TextBlock {
                    text: age,
                    spans: vec![],
                    left: row.content.x + (row.content.width - age_width).max(0.0),
                    top: row.content.y + 1.0,
                    width: age_width.max(1.0),
                    height: 20.0,
                    color: theme_weak,
                    font_size: 12.0,
                    line_height: 18.0,
                });
                let body_columns = (row.content.width / (13.0 * 0.6)).floor() as usize;
                let scroll_line = hovered
                    .then_some(self.notification_marquee_line)
                    .filter(|line| *line > 0);
                for (line_index, text) in
                    ui::notification_preview(&item.body, body_columns, 2, scroll_line)
                        .into_iter()
                        .enumerate()
                {
                    overlay_blocks.push(TextBlock {
                        text,
                        spans: vec![],
                        left: row.content.x,
                        top: row.content.y + 26.0 + line_index as f32 * 20.0,
                        width: row.content.width,
                        height: 20.0,
                        color: theme_subtext,
                        font_size: 13.0,
                        line_height: 19.0,
                    });
                }
            }
        }
        if let Some((request_id, scroll)) = &self.agent_request_detail
            && let Ok(Some(request)) = self.pending_agent_requests.get(request_id)
        {
            let layout = self.agent_request_detail_layout(&request, *scroll);
            let panel = layout.panel;
            overlay_rectangles.push(RectBlock {
                left: 0.0,
                top: 40.0,
                width: self.window_size.width as f32,
                height: (self.window_size.height as f32 - 40.0).max(1.0),
                color: [0, 0, 0, 150],
            });
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_blue));
            overlay_blocks.push(TextBlock {
                text: format!(
                    "{} · {}",
                    self.language.agent_approval(),
                    request.kind.as_str()
                ),
                spans: vec![],
                left: panel.x + 18.0,
                top: panel.y + 14.0,
                width: panel.width - 36.0,
                height: 24.0,
                color: theme_yellow,
                font_size: 15.0,
                line_height: 21.0,
            });
            let columns = ((layout.body.width - 18.0) / (13.0 * 0.6)).floor().max(1.0) as usize;
            let lines = agent_request_detail_lines(&request.message, columns);
            let start = (*scroll).min(layout.max_scroll);
            for (index, line) in lines
                .into_iter()
                .skip(start)
                .take(layout.visible_lines)
                .enumerate()
            {
                overlay_blocks.push(TextBlock {
                    text: line,
                    spans: vec![],
                    left: panel.x + 18.0,
                    top: panel.y + 48.0 + index as f32 * 20.0,
                    width: panel.width - 54.0,
                    height: 20.0,
                    color: theme_text,
                    font_size: 13.0,
                    line_height: 19.0,
                });
            }
            if let Some((track, thumb)) = layout.scrollbar {
                overlay_rectangles.push(RectBlock {
                    left: track.x,
                    top: track.y,
                    width: track.width,
                    height: track.height,
                    color: theme_crust,
                });
                overlay_rectangles.push(RectBlock {
                    left: thumb.x - 2.0,
                    top: thumb.y,
                    width: thumb.width + 4.0,
                    height: thumb.height,
                    color: theme_subtext,
                });
                overlay_blocks.push(TextBlock {
                    text: format!(
                        "{}–{} / {}",
                        start + 1,
                        (start + layout.visible_lines).min(layout.total_lines),
                        layout.total_lines
                    ),
                    spans: vec![],
                    left: panel.x + panel.width - 130.0,
                    top: panel.y + 15.0,
                    width: 96.0,
                    height: 20.0,
                    color: theme_subtext,
                    font_size: 12.0,
                    line_height: 18.0,
                });
            }
        }
        rectangles.push(RectBlock {
            left: 0.0,
            top: 0.0,
            width: self.window_size.width as f32,
            height: 40.0,
            color: theme_mantle,
        });
        let command = self.titlebar_command_bounds();
        rectangles.push(RectBlock {
            left: command.x,
            top: command.y,
            width: command.width,
            height: command.height,
            color: if self.palette_visible {
                theme_blue
            } else {
                theme_surface
            },
        });
        rectangles.push(RectBlock {
            left: command.x + 1.0,
            top: command.y + 1.0,
            width: (command.width - 2.0).max(1.0),
            height: command.height - 2.0,
            color: theme_surface,
        });
        let palette_prefix = format!("{} > ", self.language.command_palette());
        let palette_prefix_columns = ui::display_width(&palette_prefix);
        if self.palette_visible {
            let prefix = format!("{} > ", self.language.command_palette());
            let character_width = 14.0 * 0.6;
            let palette_viewport = self.palette_query.viewport(editor_columns(
                command.width - 24.0 - ui::display_width(&prefix) as f32 * character_width,
                character_width,
            ));
            push_editor_highlights(
                &mut rectangles,
                &self.palette_query,
                command.x + 12.0 + ui::display_width(&prefix) as f32 * character_width,
                command.y + 2.0,
                command.height - 4.0,
                EditorHighlightStyle {
                    character_width,
                    colors: (theme_selection, theme_cursor),
                    viewport: palette_viewport,
                },
            );
        }
        let mut palette_spans = vec![TextSpan {
            text: palette_prefix,
            color: if self.palette_visible {
                theme_blue
            } else {
                theme_subtext
            },
            bold: false,
            italic: false,
        }];
        if self.palette_visible {
            palette_spans.extend(editor_spans(
                &self.palette_query,
                theme_blue,
                theme_selection_text,
                theme_cursor_text,
                self.palette_query.viewport(editor_columns(
                    command.width - 24.0 - palette_prefix_columns as f32 * 14.0 * 0.6,
                    14.0 * 0.6,
                )),
            ));
        }
        blocks.push(TextBlock {
            text: String::new(),
            spans: palette_spans,
            left: command.x + 12.0,
            top: command.y + 4.0,
            width: command.width - 24.0,
            height: 24.0,
            color: if self.palette_visible {
                theme_blue
            } else {
                theme_subtext
            },
            font_size: 14.0,
            line_height: 20.0,
        });
        let titlebar_unread = self
            .state
            .notifications
            .iter()
            .filter(|notification| !notification.read)
            .count()
            + self.pending_agent_requests.len().unwrap_or_default();
        for (index, button) in self.titlebar_buttons().into_iter().enumerate() {
            let hovered = self.navigation_hover == Some(NavigationHover::TitlebarButton(index));
            rectangles.push(RectBlock {
                left: button.bounds.x,
                top: button.bounds.y,
                width: button.bounds.width,
                height: button.bounds.height,
                color: if matches!(button.action, TitlebarAction::Close) && hovered {
                    theme_red
                } else if hovered {
                    theme_hover
                } else {
                    theme_mantle
                },
            });
            if button.active {
                rectangles.push(RectBlock {
                    left: button.bounds.x + 8.0,
                    top: button.bounds.y + button.bounds.height - 3.0,
                    width: button.bounds.width - 16.0,
                    height: 3.0,
                    color: theme_blue,
                });
            }
            if matches!(button.action, TitlebarAction::ToggleSidebar) {
                let center_x = button.bounds.x + button.bounds.width / 2.0;
                let center_y = button.bounds.y + button.bounds.height / 2.0;
                for offset in [-5.0_f32, 0.0, 5.0] {
                    rectangles.push(RectBlock {
                        left: center_x - 7.0,
                        top: center_y + offset - 1.0,
                        width: 14.0,
                        height: 2.0,
                        color: theme_text,
                    });
                }
                continue;
            }
            if matches!(button.action, TitlebarAction::ToggleNotifications) && titlebar_unread > 0 {
                let badge_width =
                    (ui::display_width(&button.label) as f32 * 11.0 * 0.6 + 8.0).max(18.0);
                let badge = ui::Bounds {
                    x: button.bounds.x + (button.bounds.width - badge_width) / 2.0,
                    y: button.bounds.y + (button.bounds.height - 18.0) / 2.0,
                    width: badge_width,
                    height: 18.0,
                };
                rectangles.push(RectBlock {
                    left: badge.x,
                    top: badge.y,
                    width: badge.width,
                    height: badge.height,
                    color: theme_blue,
                });
                blocks.push(TextBlock {
                    text: button.label.clone(),
                    spans: vec![],
                    left: ui::centered_text_left(badge, &button.label, 11.0),
                    top: badge.y + 1.0,
                    width: badge.width,
                    height: badge.height,
                    color: theme_crust,
                    font_size: 11.0,
                    line_height: 16.0,
                });
                continue;
            }
            blocks.push(TextBlock {
                text: button.label.clone(),
                spans: vec![],
                left: ui::centered_text_left(button.bounds, &button.label, 16.0),
                top: button.bounds.y + 10.0,
                width: 16.0,
                height: 22.0,
                color: theme_text,
                font_size: 16.0,
                line_height: 19.0,
            });
        }
        if let Some(menu) = self.tab_overflow_menu
            && let Some(pane) = self.state.panes.get(&menu.pane)
        {
            let lines = self.tab_overflow_lines(menu.pane);
            let layout = self.tab_overflow_layout(menu, &lines);
            let panel = layout.panel;
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_border));
            for (row, index) in layout.visible.clone().enumerate() {
                let selected = index == pane.active_surface;
                if self.navigation_hover == Some(NavigationHover::ContextAction(index)) {
                    overlay_rectangles.push(RectBlock {
                        left: panel.x + 3.0,
                        top: panel.y + row as f32 * ui::TAB_OVERFLOW_ROW_HEIGHT + 2.0,
                        width: panel.width - 6.0,
                        height: 26.0,
                        color: theme_hover,
                    });
                }
                overlay_blocks.push(TextBlock {
                    text: layout.line(&lines, index),
                    spans: vec![],
                    left: panel.x + 14.0,
                    top: panel.y + row as f32 * ui::TAB_OVERFLOW_ROW_HEIGHT + 5.0,
                    width: panel.width - 28.0,
                    height: 25.0,
                    color: if selected { theme_blue } else { theme_text },
                    font_size: 15.0,
                    line_height: 21.0,
                });
            }
        }
        if let Some(menu) = self.notification_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.notification_context_labels(false)),
                30.0,
                NOTIFICATION_CONTEXT_ACTIONS.len(),
            );
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_border));
            let is_read = self
                .state
                .notifications
                .iter()
                .find(|item| item.id == menu.notification)
                .is_some_and(|item| item.read);
            let labels = self.language.notification_context_labels(is_read);
            for (index, label) in labels.into_iter().enumerate() {
                if self.navigation_hover == Some(NavigationHover::ContextAction(index)) {
                    overlay_rectangles.push(RectBlock {
                        left: panel.x + 3.0,
                        top: panel.y + index as f32 * 30.0 + 2.0,
                        width: panel.width - 6.0,
                        height: 26.0,
                        color: theme_hover,
                    });
                    overlay_rectangles.push(RectBlock {
                        left: panel.x + 3.0,
                        top: panel.y + index as f32 * 30.0 + 2.0,
                        width: 3.0,
                        height: 26.0,
                        color: theme_blue,
                    });
                }
                overlay_blocks.push(TextBlock {
                    text: label.into(),
                    spans: vec![],
                    left: panel.x + 14.0,
                    top: panel.y + index as f32 * 30.0 + 5.0,
                    width: panel.width - 28.0,
                    height: 25.0,
                    color: if matches!(
                        NOTIFICATION_CONTEXT_ACTIONS[index],
                        NotificationContextAction::Clear
                    ) {
                        theme_red
                    } else {
                        theme_text
                    },
                    font_size: 15.0,
                    line_height: 21.0,
                });
            }
        }
        if let Some(drag) = self.workspace_drag.filter(|drag| drag.active) {
            let layout = self.sidebar_layout();
            if let Some(source) = layout
                .rows
                .iter()
                .find(|row| row.index == drag.source_index)
            {
                let ghost_y = (self.cursor_position.1 - drag.grab_y).clamp(
                    88.0,
                    self.window_size.height as f32 - source.bounds.height - 32.0,
                );
                overlay_rectangles.push(RectBlock {
                    left: source.bounds.x + 6.0,
                    top: ghost_y + 1.0,
                    width: (source.bounds.width - 12.0).max(1.0),
                    height: source.bounds.height - 2.0,
                    color: theme_selection,
                });
                if let Some(workspace) = self.state.workspaces.get(drag.source_index) {
                    overlay_blocks.push(TextBlock {
                        text: ui::truncate_display_width(&workspace.name, 28),
                        spans: vec![],
                        left: source.bounds.x + 16.0,
                        top: ghost_y + 3.0,
                        width: (source.bounds.width - 28.0).max(1.0),
                        height: 20.0,
                        color: theme_text,
                        font_size: 14.0,
                        line_height: 19.0,
                    });
                }
                if let Some(target) = layout
                    .rows
                    .iter()
                    .find(|row| row.index == drag.target_index)
                {
                    let insertion_y = if drag.target_index < drag.source_index {
                        target.bounds.y
                    } else {
                        target.bounds.y + target.bounds.height
                    };
                    overlay_rectangles.push(RectBlock {
                        left: target.bounds.x + 8.0,
                        top: insertion_y - 1.5,
                        width: (target.bounds.width - 16.0).max(1.0),
                        height: 3.0,
                        color: theme_blue,
                    });
                }
            }
        }
        if let Some(menu) = self.terminal_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.terminal_context_labels()),
                30.0,
                TERMINAL_CONTEXT_ACTIONS.len(),
            );
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_border));
            let labels = self.language.terminal_context_labels();
            for (index, label) in labels.into_iter().enumerate() {
                if self.navigation_hover == Some(NavigationHover::ContextAction(index)) {
                    overlay_rectangles.push(RectBlock {
                        left: panel.x + 3.0,
                        top: panel.y + index as f32 * 30.0 + 2.0,
                        width: panel.width - 6.0,
                        height: 26.0,
                        color: theme_hover,
                    });
                    overlay_rectangles.push(RectBlock {
                        left: panel.x + 3.0,
                        top: panel.y + index as f32 * 30.0 + 2.0,
                        width: 3.0,
                        height: 26.0,
                        color: theme_blue,
                    });
                }
                let enabled = terminal_context_action_enabled(
                    TERMINAL_CONTEXT_ACTIONS[index],
                    self.selection,
                    menu.pane,
                );
                overlay_blocks.push(TextBlock {
                    text: label.into(),
                    spans: vec![],
                    left: panel.x + 14.0,
                    top: panel.y + index as f32 * 30.0 + 5.0,
                    width: panel.width - 28.0,
                    height: 25.0,
                    color: if enabled { theme_text } else { theme_weak },
                    font_size: 15.0,
                    line_height: 21.0,
                });
            }
        }
        if let Some(menu) = self.tab_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.tab_context_labels()),
                30.0,
                TAB_CONTEXT_ACTIONS.len(),
            );
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_border));
            let labels = self.language.tab_context_labels();
            let tab_count = self
                .state
                .panes
                .get(&menu.pane)
                .map(|pane| pane.surfaces.len())
                .unwrap_or(0);
            for (index, label) in labels.into_iter().enumerate() {
                if self.navigation_hover == Some(NavigationHover::ContextAction(index)) {
                    overlay_rectangles.push(RectBlock {
                        left: panel.x + 3.0,
                        top: panel.y + index as f32 * 30.0 + 2.0,
                        width: panel.width - 6.0,
                        height: 26.0,
                        color: theme_hover,
                    });
                    overlay_rectangles.push(RectBlock {
                        left: panel.x + 3.0,
                        top: panel.y + index as f32 * 30.0 + 2.0,
                        width: 3.0,
                        height: 26.0,
                        color: theme_blue,
                    });
                }
                let enabled = tab_context_action_enabled(TAB_CONTEXT_ACTIONS[index], tab_count);
                overlay_blocks.push(TextBlock {
                    text: label.into(),
                    spans: vec![],
                    left: panel.x + 14.0,
                    top: panel.y + index as f32 * 30.0 + 5.0,
                    width: panel.width - 28.0,
                    height: 25.0,
                    color: if !enabled {
                        theme_weak
                    } else {
                        match TAB_CONTEXT_ACTIONS[index] {
                            TabContextAction::Close => theme_red,
                            TabContextAction::CloseOthers => theme_yellow,
                            TabContextAction::NewSurface => theme_text,
                        }
                    },
                    font_size: 15.0,
                    line_height: 21.0,
                });
            }
        }
        if let Some(menu) = self.workspace_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&self.language.workspace_context_labels()),
                30.0,
                WORKSPACE_CONTEXT_ACTIONS.len(),
            );
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_border));
            let labels = self.language.workspace_context_labels();
            for (index, label) in labels.into_iter().enumerate() {
                if self.navigation_hover == Some(NavigationHover::ContextAction(index)) {
                    overlay_rectangles.push(RectBlock {
                        left: panel.x + 3.0,
                        top: panel.y + index as f32 * 30.0 + 2.0,
                        width: panel.width - 6.0,
                        height: 26.0,
                        color: theme_hover,
                    });
                    overlay_rectangles.push(RectBlock {
                        left: panel.x + 3.0,
                        top: panel.y + index as f32 * 30.0 + 2.0,
                        width: 3.0,
                        height: 26.0,
                        color: theme_blue,
                    });
                }
                let enabled = workspace_context_action_enabled(
                    WORKSPACE_CONTEXT_ACTIONS[index],
                    self.state.workspaces.len(),
                );
                overlay_blocks.push(TextBlock {
                    text: label.into(),
                    spans: vec![],
                    left: panel.x + 14.0,
                    top: panel.y + index as f32 * 30.0 + 5.0,
                    width: panel.width - 28.0,
                    height: 25.0,
                    color: if !enabled {
                        theme_weak
                    } else if matches!(
                        WORKSPACE_CONTEXT_ACTIONS[index],
                        WorkspaceContextAction::Close
                    ) {
                        theme_red
                    } else {
                        theme_text
                    },
                    font_size: 15.0,
                    line_height: 21.0,
                });
            }
        }
        if let Some(menu) = self.sidebar_context_menu {
            let panel = ui::popup_menu(
                menu.anchor.0,
                menu.anchor.1,
                self.window_size.width as f32,
                self.window_size.height as f32,
                ui::context_menu_width(&[self.language.new_workspace()]),
                30.0,
                1,
            );
            overlay_rectangles.extend(context_menu_frame(panel, theme_surface, theme_border));
            if self.navigation_hover == Some(NavigationHover::ContextAction(0)) {
                overlay_rectangles.push(RectBlock {
                    left: panel.x + 3.0,
                    top: panel.y + 2.0,
                    width: panel.width - 6.0,
                    height: 26.0,
                    color: theme_hover,
                });
                overlay_rectangles.push(RectBlock {
                    left: panel.x + 3.0,
                    top: panel.y + 2.0,
                    width: 3.0,
                    height: 26.0,
                    color: theme_blue,
                });
            }
            overlay_blocks.push(TextBlock {
                text: self.language.new_workspace().into(),
                spans: vec![],
                left: panel.x + 14.0,
                top: panel.y + 5.0,
                width: panel.width - 28.0,
                height: 25.0,
                color: theme_text,
                font_size: 15.0,
                line_height: 21.0,
            });
        }
        self.update_ime_cursor_area();
        if let Some(renderer) = &mut self.renderer {
            renderer.set_scene_with_overlays(
                blocks,
                rectangles,
                overlay_blocks,
                overlay_rectangles,
            );
            renderer.window().request_redraw();
        }
    }

    /// Register a Codex app-server approval without granting it implicitly.
    ///
    /// Wire contract:
    /// `agent.approval.register` receives
    /// `{source,workspace_id,surface_id,request:{jsonrpc_id,method,kind,
    /// thread_id,turn_id,item_id,params},decisions:[deny|once|always|all]}`.
    /// The result is `{status:"pending",approval_id}` until a UI decision,
    /// then `{status:"resolved",decision:{mode}}`.
    fn register_agent_approval(&mut self, params: &Value) -> Result<Value, String> {
        let source = required_string(params, "source")?;
        if !matches!(source, "codex_app_server" | "claude_hook") {
            return Err("params.source must be `codex_app_server` or `claude_hook`".into());
        }
        let workspace_id = uuid::Uuid::parse_str(required_string(params, "workspace_id")?)
            .map_err(|_| "invalid params.workspace_id".to_owned())?;
        let surface_id = uuid::Uuid::parse_str(required_string(params, "surface_id")?)
            .map_err(|_| "invalid params.surface_id".to_owned())?;
        let workspace = self
            .state
            .workspaces
            .iter()
            .find(|workspace| workspace.id == workspace_id)
            .ok_or_else(|| "workspace not found".to_owned())?;
        let belongs =
            self.state.panes.values().any(|pane| {
                workspace.root.contains(pane.id) && pane.surfaces.contains(&surface_id)
            });
        if !belongs || !self.state.surfaces.contains_key(&surface_id) {
            return Err("surface does not belong to params.workspace_id".into());
        }
        let request = params
            .get("request")
            .and_then(Value::as_object)
            .ok_or_else(|| "missing params.request object".to_owned())?;
        let jsonrpc_id = request
            .get("jsonrpc_id")
            .filter(|value| value.is_string() || value.is_number())
            .ok_or_else(|| "request.jsonrpc_id must be a string or number".to_owned())?;
        let method = required_string_object(request, "method")?;
        let kind = required_string_object(request, "kind")?;
        validate_agent_approval_protocol(source, method, kind)?;
        let thread_id = required_string_object(request, "thread_id")?;
        let turn_id = required_string_object(request, "turn_id")?;
        let item_id = required_string_object(request, "item_id")?;
        let verified_claude_lifecycle = if source == "claude_hook" {
            let lifecycle_id = required_string(params, "lifecycle_id")?;
            let surface = self
                .state
                .surfaces
                .get(&surface_id)
                .ok_or_else(|| "Claude approval surface not found".to_owned())?;
            let terminal = match surface.kind {
                SurfaceKind::Terminal(terminal) => terminal,
                _ => return Err("Claude approval target must be a terminal surface".into()),
            };
            let runtime = self
                .terminals
                .get(&terminal)
                .ok_or_else(|| "Claude approval terminal is not running".to_owned())?;
            let metadata_agent = surface
                .agent
                .as_ref()
                .map(|metadata| metadata.name.as_str());
            let authoritative_agent = self
                .agent_sessions
                .get(&surface_id)
                .map(|session| session.agent_kind.as_str());
            if !claude_approval_binding_matches(
                &runtime.lifecycle_id,
                metadata_agent,
                authoritative_agent,
                lifecycle_id,
            ) {
                return Err("Claude approval does not match the active Agent lifecycle".into());
            }
            Some(lifecycle_id.to_owned())
        } else {
            None
        };
        let request_params = request
            .get("params")
            .ok_or_else(|| "missing request.params".to_owned())?;
        let decisions = params
            .get("decisions")
            .and_then(Value::as_array)
            .ok_or_else(|| "missing params.decisions array".to_owned())?;
        let mut modes = Vec::new();
        for decision in decisions {
            let mode = decision
                .as_str()
                .ok_or_else(|| "params.decisions entries must be strings".to_owned())?;
            if !matches!(mode, "deny" | "once" | "always" | "all") {
                return Err(format!("unsupported approval decision `{mode}`"));
            }
            if !modes.contains(&mode) {
                modes.push(mode);
            }
        }
        if !modes.contains(&"deny") || !modes.contains(&"once") {
            return Err("params.decisions must include `deny` and `once`".into());
        }
        let approval_id = format!(
            "approval:{source}:{surface_id}:{thread_id}:{turn_id}:{item_id}:{}",
            jsonrpc_id
        );
        if let Some(result) = self.agent_approval_results.get(&approval_id).copied() {
            return Ok(agent_approval_resolved_json(result));
        }
        let lifecycle_id = verified_claude_lifecycle.unwrap_or_else(|| {
            self.agent_sessions
                .get(&surface_id)
                .and_then(|session| session.lifecycle.external_id.clone())
                .unwrap_or_else(|| format!("{source}:{thread_id}"))
        });
        let summary = agent_approval_message(self.language, source, kind, method, request_params);
        let interaction = AgentInteractionRequest {
            request_id: approval_id.clone(),
            surface_id: surface_id.to_string(),
            lifecycle_id,
            agent_pid: None,
            pid_start_time: None,
            kind: IpcAgentRequestKind::new(IpcAgentRequestKind::PERMISSION),
            message: summary.clone(),
            choices: modes
                .into_iter()
                .map(|mode| wmcterm_ipc::AgentChoice {
                    id: mode.into(),
                    label: mode.into(),
                })
                .collect(),
            deadline_unix_ms: params.get("deadline_unix_ms").and_then(Value::as_u64),
        };
        match self
            .pending_agent_requests
            .register(interaction)
            .map_err(|error| error.to_string())?
        {
            wmcterm_ipc::RegisterOutcome::AlreadyCompleted(reply) => {
                let mode = completed_approval_action(&reply).unwrap_or(AgentApprovalAction::Deny);
                self.remember_agent_approval_result(approval_id, mode);
                self.sync_surface_agent_request_state(surface_id);
                Ok(agent_approval_resolved_json(mode))
            }
            wmcterm_ipc::RegisterOutcome::Inserted => {
                self.sync_surface_agent_request_state(surface_id);
                if approval_should_reveal_panel(
                    self.text_editor_active(),
                    self.active_surface_id(),
                    surface_id,
                ) {
                    self.notifications_visible = true;
                }
                if approval_needs_native_notification(
                    self.config.agent.notifications,
                    self.window_focused,
                    self.active_surface_id(),
                    surface_id,
                ) {
                    let preview = summary.split("\n\n").next().unwrap_or(&summary);
                    show_native_notification(self.language.agent_approval(), preview);
                }
                self.rebuild_scene();
                Ok(json!({"status":"pending","approval_id":approval_id}))
            }
            wmcterm_ipc::RegisterOutcome::DuplicatePending => {
                self.sync_surface_agent_request_state(surface_id);
                if approval_should_reveal_panel(
                    self.text_editor_active(),
                    self.active_surface_id(),
                    surface_id,
                ) {
                    self.notifications_visible = true;
                }
                self.rebuild_scene();
                Ok(json!({"status":"pending","approval_id":approval_id}))
            }
        }
    }

    fn poll_agent_approval(&self, params: &Value) -> Result<Value, String> {
        let approval_id = required_string(params, "approval_id")?;
        if let Some(action) = self.agent_approval_results.get(approval_id).copied() {
            return Ok(agent_approval_resolved_json(action));
        }
        if self
            .pending_agent_requests
            .get(approval_id)
            .map_err(|error| error.to_string())?
            .is_some()
        {
            Ok(json!({"status":"pending","approval_id":approval_id}))
        } else {
            Err("approval_id not found".into())
        }
    }

    fn handle_rpc(&mut self, request: Request) -> Response {
        let id = request.id.clone();
        let result: Result<Value, String> = (|| {
            match request.method.as_str() {
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
                "protocol_version": 4,
                "product": "wmcterm",
                "version": env!("CARGO_PKG_VERSION"),
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
                    "fail_closed": true
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
                    "authentication": "interactive OpenSSH; connecting includes authentication waits"
                },
                "methods": [
                    "ssh.connect", "ssh.status", "ssh.exec", "ssh.cancel",
                    "app.status", "workspace.list", "workspace.create", "workspace.activate",
                    "workspace.rename", "workspace.describe", "workspace.move",
                    "workspace.cwd",
                    "workspace.close", "pane.list", "pane.split", "pane.focus", "pane.zoom",
                    "pane.close", "pane.close-others", "surface.list", "surface.new", "surface.activate",
                    "surface.close", "surface.close-others", "surface.read", "surface.search", "surface.markdown",
                    "surface.logs", "screen.read", "send", "send.line", "terminal.search", "terminal.clear",
                    "terminal.scroll", "notification.list", "notification.mark", "notify",
                    "agent.update", "agent.request.list", "agent.request.reply", "agent.request.cancel",
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
            "agent.approval.register" => self.register_agent_approval(&request.params),
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
                    let terminal = if codex_hook.is_some() || codex_session_end.is_some() {
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
                    if !self.record_agent_event(terminal, &request.params, &event)? {
                        if let Some(session_id) = codex_session_end.as_deref() {
                            self.end_codex_subagent_session(terminal, session_id)?;
                        }
                        return Ok(json!({"ignored": true}));
                    }
                    let state = self.apply_agent_event(terminal, event);
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

impl ApplicationHandler<UserEvent> for WmctermApplication {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.renderer.is_some() {
            return;
        }
        let mut attributes = default_window_attributes()
            .with_inner_size(LogicalSize::new(
                self.window_size.width,
                self.window_size.height,
            ))
            .with_min_inner_size(LogicalSize::new(600.0, 360.0))
            .with_window_icon(application_icon())
            .with_decorations(false)
            .with_transparent(self.config.window.opacity < 1.0);
        #[cfg(target_os = "linux")]
        if let Some(token) = event_loop.read_token_from_env() {
            reset_activation_token_env();
            attributes = attributes.with_activation_token(token);
        }
        if let Some(monitor) = event_loop.primary_monitor() {
            let monitor_size = monitor.size();
            let monitor_position = monitor.position();
            let desired_size: PhysicalSize<u32> =
                LogicalSize::new(self.window_size.width, self.window_size.height)
                    .to_physical(monitor.scale_factor());
            let x = monitor_position.x
                + monitor_size.width.saturating_sub(desired_size.width) as i32 / 2;
            let y = monitor_position.y
                + monitor_size.height.saturating_sub(desired_size.height) as i32 / 2;
            attributes = attributes.with_position(PhysicalPosition::new(x, y));
        }
        match event_loop
            .create_window(attributes)
            .context("creating wmcterm window")
            .and_then(|window| {
                let window = Arc::new(window);
                let scale_factor = window.scale_factor();
                GpuRenderer::new(
                    window,
                    event_loop,
                    self.config.terminal.font_family.clone(),
                    self.config.window.opacity < 1.0,
                    scale_factor,
                )
            }) {
            Ok(renderer) => {
                self.scale_factor = renderer.window().scale_factor();
                self.window_size =
                    logical_window_size(renderer.window().inner_size(), self.scale_factor);
                renderer.window().set_ime_allowed(true);
                self.renderer = Some(renderer);
                self.resize_terminals();
                self.rebuild_scene();
            }
            Err(error) => {
                tracing::error!(%error, "renderer initialization failed");
                event_loop.exit();
            }
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let now = std::time::Instant::now();
        self.flush_pending_unmodified_tab();
        self.reload_config_if_changed();
        if self.expire_agent_requests() {
            self.rebuild_scene();
        }
        if self.last_metadata_refresh.elapsed() >= std::time::Duration::from_secs(5) {
            self.refresh_workspace_metadata();
            self.last_metadata_refresh = std::time::Instant::now();
        }
        let mut document_changed = false;
        for backend in self.surface_backends.values_mut() {
            match backend.refresh() {
                Ok(changed) => document_changed |= changed,
                Err(error) => tracing::warn!(%error, "refreshing document surface failed"),
            }
        }
        if document_changed {
            self.rebuild_scene();
        }
        let mut wait = std::time::Duration::from_secs(1);
        if self.window_focused
            && let Some((_, queued_at)) = &self.pending_unmodified_tab
        {
            wait = wait.min(UNMODIFIED_TAB_FOCUS_GRACE.saturating_sub(queued_at.elapsed()));
        }
        if self.update_selection_autoscroll() {
            wait = wait.min(std::time::Duration::from_millis(16));
        }
        if self.notifications_visible
            && let Some((_, started)) = self.notification_hover_started
        {
            const HOVER_DELAY: std::time::Duration = std::time::Duration::from_millis(800);
            const LINE_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1100);
            let elapsed = started.elapsed();
            let line = if elapsed < HOVER_DELAY {
                0
            } else {
                1 + ((elapsed - HOVER_DELAY).as_millis() / LINE_INTERVAL.as_millis()) as usize
            };
            if line != self.notification_marquee_line {
                self.notification_marquee_line = line;
                self.rebuild_scene();
            }
            wait = std::time::Duration::from_millis(100);
        }
        if let Some(blink_wait) = self.update_cursor_blink(now) {
            wait = wait.min(blink_wait);
        }
        self.rebuild_scene_if_due(now);
        if let Some(scene_wait) = self.scene_rebuild_wait(now) {
            wait = wait.min(scene_wait);
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(now + wait));
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::TerminalWake => self.drain_terminal_events(event_loop),
            UserEvent::Rpc(request, response) => {
                let _ = response.send(self.handle_rpc(request));
            }
            UserEvent::WorkspaceMetadata(metadata) => {
                self.metadata_refresh_in_flight = false;
                let mut changed = false;
                for update in metadata {
                    if let Some(workspace) = self
                        .state
                        .workspaces
                        .iter_mut()
                        .find(|workspace| workspace.id == update.id)
                    {
                        if workspace.git_branch != update.git_branch {
                            workspace.git_branch = update.git_branch;
                            changed = true;
                        }
                        if workspace.listening_ports != update.listening_ports {
                            workspace.listening_ports = update.listening_ports;
                            changed = true;
                        }
                    }
                }
                if changed {
                    self.rebuild_scene();
                }
            }
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.request_application_close();
                if self.quit_requested {
                    event_loop.exit();
                }
            }
            WindowEvent::ModifiersChanged(value) => {
                self.modifiers = value.state();
                self.update_cursor_icon();
            }
            WindowEvent::Ime(Ime::Commit(text)) => self.commit_ime_text(text),
            WindowEvent::Ime(Ime::Preedit(text, _)) => self.update_ime_preedit(&text),
            WindowEvent::Ime(Ime::Disabled) => self.update_ime_preedit(""),
            WindowEvent::Focused(focused) => {
                self.window_focused = focused;
                self.reset_cursor_blink();
                self.rebuild_scene();
                if focused {
                    self.window_focus_gained_at = std::time::Instant::now();
                    // System overlays can return focus before the shortcut
                    // that dismissed them has been released. Windows may then
                    // deliver the trailing character (for example the `c` in
                    // a screenshot tool's Ctrl+C) to this window. Quarantine
                    // all keyboard and IME input through a short cross-platform
                    // focus grace period. On Windows, keep it active longer
                    // while a physical key remains held. A subsequent mouse
                    // press proves that the user intentionally reactivated the
                    // window and clears the quarantine immediately.
                    self.focus_input_quarantine = true;
                } else {
                    // A window manager may consume the matching Alt release
                    // after Alt+Tab has transferred focus.
                    self.physical_alt_pressed = false;
                    self.modifiers = ModifiersState::empty();
                    self.pending_unmodified_tab = None;
                    self.focus_input_quarantine = false;
                    if self.notifications_visible {
                        self.notifications_visible = false;
                        self.notification_scroll = 0;
                        self.notification_selection = 0;
                        self.notification_context_menu = None;
                        self.rebuild_scene();
                    }
                    self.sidebar_drag = false;
                    self.sidebar_scrollbar_drag = None;
                    self.workspace_drag = None;
                    self.scrollbar_drag = None;
                    self.text_edit_drag = None;
                    if self.terminal_mouse_capture.is_some() {
                        self.send_captured_terminal_mouse(
                            MouseActionInput::Release,
                            Some(MouseButtonInput::Left),
                        );
                        self.terminal_mouse_capture = None;
                    }
                    self.mouse_pressed = false;
                    self.selection_autoscroll_last = None;
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                let position: LogicalPosition<f64> = position.to_logical(self.scale_factor);
                self.cursor_position = (position.x as f32, position.y as f32);
                self.update_palette_hover();
                self.update_navigation_hover();
                if self.mouse_pressed {
                    self.update_mouse_action();
                }
                self.update_cursor_icon();
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Left,
                ..
            } => {
                self.focus_input_quarantine = false;
                self.begin_mouse_action();
                if self.quit_requested {
                    event_loop.exit();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Released,
                button: MouseButton::Left,
                ..
            } => self.finish_mouse_action(),
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Right,
                ..
            } => {
                self.focus_input_quarantine = false;
                if self.close_confirmation.is_some() || self.codex_trust_confirmation.is_some() {
                    return;
                }
                self.dismiss_notifications_outside_cursor();
                if self.open_notification_context_menu()
                    || self.open_workspace_context_menu()
                    || self.open_sidebar_context_menu()
                    || self.open_tab_context_menu()
                    || self.pointer_over_overlay()
                {
                    return;
                }
                let Some((_, terminal)) = self.terminal_at_cursor() else {
                    return;
                };
                let mouse_tracking = self
                    .terminals
                    .get(&terminal)
                    .is_some_and(|runtime| runtime.snapshot.mouse_tracking);
                if terminal_right_click_opens_context_menu(
                    self.modifiers.shift_key(),
                    mouse_tracking,
                ) {
                    self.open_terminal_context_menu();
                    return;
                }
                self.focus_at_cursor();
                if self.mark_terminal_notifications_read(terminal) {
                    self.rebuild_scene();
                }
                if self.send_terminal_mouse(MouseActionInput::Press, Some(MouseButtonInput::Right))
                {
                    self.send_terminal_mouse(
                        MouseActionInput::Release,
                        Some(MouseButtonInput::Right),
                    );
                } else {
                    self.open_terminal_context_menu();
                }
            }
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                button: MouseButton::Middle,
                ..
            } => {
                self.focus_input_quarantine = false;
                if self.close_confirmation.is_some() || self.codex_trust_confirmation.is_some() {
                    return;
                }
                self.dismiss_notifications_outside_cursor();
                if self.pointer_over_overlay() || self.scrollbar_at_cursor().is_some() {
                    return;
                }
                if !self.focus_at_cursor() && self.terminal_at_cursor().is_some() {
                    self.paste_clipboard(ClipboardTarget::Primary);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let cell_height = self.cell_height() as f64;
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => {
                        self.pixel_scroll_remainder = 0.0;
                        -(y * 3.0) as isize
                    }
                    MouseScrollDelta::PixelDelta(position) => accumulate_pixel_scroll(
                        &mut self.pixel_scroll_remainder,
                        -position.y / self.scale_factor,
                        cell_height,
                    ),
                };
                if let Some(menu) = self.tab_overflow_menu {
                    let menu_lines = self.tab_overflow_lines(menu.pane);
                    let layout = self.tab_overflow_layout(menu, &menu_lines);
                    if layout
                        .panel
                        .contains(self.cursor_position.0, self.cursor_position.1)
                    {
                        if lines != 0 {
                            let capacity = layout.visible.len();
                            if let Some(open_menu) = &mut self.tab_overflow_menu {
                                open_menu.scroll_offset = ui::tab_overflow_scroll_offset(
                                    menu_lines.len(),
                                    capacity,
                                    open_menu.scroll_offset,
                                    lines,
                                );
                            }
                            self.navigation_hover = self.navigation_hover_at_cursor();
                            self.rebuild_scene();
                        }
                        return;
                    }
                }
                if self.agent_request_detail.is_some() {
                    if lines != 0 {
                        self.scroll_agent_request_detail(lines);
                        self.rebuild_scene();
                    }
                    return;
                }
                if self.notifications_visible
                    && self
                        .notification_panel()
                        .contains(self.cursor_position.0, self.cursor_position.1)
                {
                    if lines != 0 {
                        let pending_count = self.pending_agent_requests.len().unwrap_or_default();
                        if pending_count > 0
                            && self
                                .agent_approval_region(pending_count)
                                .contains(self.cursor_position.0, self.cursor_position.1)
                        {
                            self.scroll_agent_requests(lines);
                        } else {
                            self.scroll_notifications(lines);
                        }
                    }
                    return;
                }
                if self.palette_visible {
                    let registry = ActionRegistry::default();
                    let actions = self.palette_actions(&registry);
                    if self
                        .command_palette_panel(actions.len())
                        .contains(self.cursor_position.0, self.cursor_position.1)
                    {
                        if lines != 0 {
                            self.scroll_palette(lines);
                        }
                        return;
                    }
                }
                if self.pointer_over_overlay() {
                    return;
                }
                if self.sidebar_visible && self.cursor_position.0 < self.sidebar_width() {
                    if lines != 0 {
                        self.scroll_sidebar(lines);
                    }
                    return;
                }
                if lines != 0 && self.scroll_tabs_at_cursor(lines) {
                    return;
                }
                if lines != 0 {
                    let button = if lines < 0 {
                        MouseButtonInput::WheelUp
                    } else {
                        MouseButtonInput::WheelDown
                    };
                    let has_local_scrollback = self
                        .terminal_at_cursor()
                        .and_then(|(_, terminal)| self.terminals.get(&terminal))
                        .is_some_and(|runtime| runtime.snapshot.scrollback_rows > 0);
                    if self.modifiers.shift_key()
                        || has_local_scrollback
                        || !self.send_terminal_mouse(MouseActionInput::Press, Some(button))
                    {
                        self.scroll_surface_at_cursor(lines);
                    }
                }
            }
            WindowEvent::Resized(size) => {
                self.window_size = logical_window_size(size, self.scale_factor);
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
                self.resize_terminals();
                self.rebuild_scene();
            }
            WindowEvent::ScaleFactorChanged {
                scale_factor,
                mut inner_size_writer,
            } => {
                self.scale_factor = scale_factor.max(f64::EPSILON);
                let size: PhysicalSize<u32> =
                    LogicalSize::new(self.window_size.width, self.window_size.height)
                        .to_physical(self.scale_factor);
                let _ = inner_size_writer.request_inner_size(size);
                if let Some(renderer) = &mut self.renderer {
                    renderer.set_scale_factor(self.scale_factor);
                    renderer.resize(size.width, size.height);
                }
                self.pixel_scroll_remainder = 0.0;
                self.resize_terminals();
                self.rebuild_scene();
            }
            WindowEvent::DroppedFile(path) => {
                let text = shell_quote_path(&path);
                if let Some((_, terminal)) =
                    self.terminal_at_point(self.cursor_position.0, self.cursor_position.1)
                {
                    let notifications_changed = self.mark_terminal_notifications_read(terminal);
                    if let Some(runtime) = self.terminals.get(&terminal) {
                        let _ = runtime
                            .session
                            .send(SessionCommand::Input(text.into_bytes()));
                    }
                    if notifications_changed {
                        self.rebuild_scene();
                    }
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if should_suppress_recent_focus_tab(
                    event.state,
                    event.physical_key,
                    self.window_focus_gained_at.elapsed(),
                ) {
                    return;
                }
                if should_defer_unmodified_tab(
                    self.window_focused,
                    &event,
                    self.modifiers,
                    self.physical_alt_pressed,
                ) {
                    self.pending_unmodified_tab = Some((event, std::time::Instant::now()));
                    event_loop.set_control_flow(ControlFlow::WaitUntil(
                        std::time::Instant::now() + UNMODIFIED_TAB_FOCUS_GRACE,
                    ));
                    return;
                }
                self.send_key(&event);
                if self.quit_requested {
                    event_loop.exit();
                }
            }
            WindowEvent::RedrawRequested => {
                if let Some(renderer) = &mut self.renderer {
                    let started = std::time::Instant::now();
                    let result = renderer.render();
                    let render_duration = started.elapsed();
                    self.observe_render_duration(render_duration);
                    if let Err(error) = result {
                        tracing::error!(%error, "render failed");
                        event_loop.exit();
                    }
                }
            }
            _ => {}
        }
    }
}

fn shell_quote_path(path: &std::path::Path) -> String {
    shell_quote_command(&path.to_string_lossy())
}

fn logical_window_size(size: PhysicalSize<u32>, scale_factor: f64) -> PhysicalSize<u32> {
    let logical: LogicalSize<f64> = size.to_logical(scale_factor.max(f64::EPSILON));
    PhysicalSize::new(
        logical.width.round().max(1.0) as u32,
        logical.height.round().max(1.0) as u32,
    )
}

fn terminal_mouse_geometry(
    x: f32,
    y: f32,
    cell_width: f32,
    cell_height: f32,
    scale_factor: f64,
) -> (f32, f32, u32, u32) {
    let scale_factor = scale_factor.max(f64::EPSILON);
    let physical_cell_width = (cell_width as f64 * scale_factor).round().max(1.0) as u32;
    let physical_cell_height = (cell_height as f64 * scale_factor).round().max(1.0) as u32;
    (
        x / cell_width.max(f32::EPSILON) * physical_cell_width as f32,
        y / cell_height.max(f32::EPSILON) * physical_cell_height as f32,
        physical_cell_width,
        physical_cell_height,
    )
}

const fn terminal_right_click_opens_context_menu(shift: bool, mouse_tracking: bool) -> bool {
    shift || !mouse_tracking
}

#[cfg(target_os = "linux")]
fn set_arboard_text(
    clipboard: &mut arboard::Clipboard,
    text: String,
    target: ClipboardTarget,
) -> Result<(), String> {
    use arboard::{LinuxClipboardKind, SetExtLinux};
    let target = match target {
        ClipboardTarget::Clipboard => LinuxClipboardKind::Clipboard,
        ClipboardTarget::Primary => LinuxClipboardKind::Primary,
    };
    clipboard
        .set()
        .clipboard(target)
        .text(text)
        .map_err(|error| error.to_string())
}

#[cfg(all(not(target_os = "linux"), not(windows)))]
fn set_arboard_text(
    clipboard: &mut arboard::Clipboard,
    text: String,
    _target: ClipboardTarget,
) -> Result<(), String> {
    clipboard.set_text(text).map_err(|error| error.to_string())
}

#[cfg(target_os = "linux")]
fn get_arboard_text(
    clipboard: &mut arboard::Clipboard,
    target: ClipboardTarget,
) -> Result<String, String> {
    use arboard::{GetExtLinux, LinuxClipboardKind};
    let target = match target {
        ClipboardTarget::Clipboard => LinuxClipboardKind::Clipboard,
        ClipboardTarget::Primary => LinuxClipboardKind::Primary,
    };
    clipboard
        .get()
        .clipboard(target)
        .text()
        .map_err(|error| error.to_string())
}

#[cfg(not(target_os = "linux"))]
fn get_arboard_text(
    clipboard: &mut arboard::Clipboard,
    _target: ClipboardTarget,
) -> Result<String, String> {
    clipboard.get_text().map_err(|error| error.to_string())
}

fn application_socket_path() -> Option<std::path::PathBuf> {
    let path = default_socket_path()?;
    #[cfg(windows)]
    {
        let name = format!("wmcterm-{}.endpoint", std::process::id());
        Some(path.with_file_name(name))
    }
    #[cfg(not(windows))]
    {
        Some(path)
    }
}

fn shell_quote_command(value: &str) -> String {
    #[cfg(windows)]
    {
        format!("'{}'", value.replace('\'', "''"))
    }
    #[cfg(not(windows))]
    {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

fn terminal_command_spec(command: Option<&str>) -> CommandSpec {
    #[cfg(windows)]
    {
        let shell = env::var_os("SHELL").unwrap_or_else(|| "powershell.exe".into());
        let name = std::path::Path::new(&shell)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        match (name.as_str(), command) {
            ("powershell" | "pwsh", Some(command)) => {
                CommandSpec::new(shell).args(["-NoLogo", "-Command", command])
            }
            ("powershell" | "pwsh", None) => CommandSpec::new(shell).args(["-NoLogo"]),
            ("cmd", Some(command)) => CommandSpec::new(shell).args(["/D", "/S", "/C", command]),
            ("cmd", None) => CommandSpec::new(shell).args(["/D"]),
            (_, Some(command)) => CommandSpec::new(shell).args(["-lc", command]),
            (_, None) => CommandSpec::new(shell),
        }
    }
    #[cfg(not(windows))]
    {
        let shell = env::var_os("SHELL").unwrap_or_else(|| "sh".into());
        match command {
            Some(command) => CommandSpec::new(shell).args(["-lc", command]),
            None => CommandSpec::new(shell),
        }
    }
}

fn managed_agent_command(mode: AgentRunMode, program: &str) -> String {
    let arguments = format!("agent --mode {mode} {program}");
    std::env::current_exe()
        .map(|path| {
            #[cfg(windows)]
            return powershell_executable_command(&path, &arguments);
            #[cfg(not(windows))]
            format!(
                "{} {arguments}",
                shell_quote_command(&path.to_string_lossy()),
            )
        })
        .unwrap_or_else(|_| format!("wmcterm {arguments}"))
}

#[cfg(any(windows, test))]
fn powershell_executable_command(path: &std::path::Path, argument: &str) -> String {
    format!(
        "& {} {argument}",
        shell_quote_command(&path.to_string_lossy())
    )
}

#[cfg(windows)]
fn set_windows_clipboard_text(text: &str) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use std::ptr;
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    let mut opened = false;
    for _ in 0..5 {
        if unsafe { OpenClipboard(ptr::null_mut()) } != 0 {
            opened = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if !opened {
        return Err(std::io::Error::last_os_error().into());
    }

    let utf16 = text
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let byte_len = utf16.len() * std::mem::size_of::<u16>();
    let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, byte_len) };
    if handle.is_null() {
        unsafe { CloseClipboard() };
        return Err(std::io::Error::last_os_error().into());
    }
    let memory = unsafe { GlobalLock(handle) }.cast::<u16>();
    if memory.is_null() {
        unsafe {
            GlobalFree(handle);
            CloseClipboard();
        }
        return Err(std::io::Error::last_os_error().into());
    }
    unsafe {
        ptr::copy_nonoverlapping(utf16.as_ptr(), memory, utf16.len());
        GlobalUnlock(handle);
    }
    let stored = unsafe {
        EmptyClipboard();
        SetClipboardData(CF_UNICODETEXT as u32, handle)
    };
    unsafe { CloseClipboard() };
    if stored.is_null() {
        unsafe { GlobalFree(handle) };
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

const MAX_SCREEN_READ_LINES: usize = 100_000;

fn optional_screen_lines(params: &Value) -> Result<Option<usize>, String> {
    match params.get("lines") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let lines = value
                .as_u64()
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| "params.lines must be a positive integer".to_owned())?;
            if lines == 0 || lines > MAX_SCREEN_READ_LINES {
                return Err(format!(
                    "params.lines must be between 1 and {MAX_SCREEN_READ_LINES}"
                ));
            }
            Ok(Some(lines))
        }
    }
}

fn tail_lines(text: &str, maximum: usize) -> String {
    let lines = text.lines().collect::<Vec<_>>();
    lines[lines.len().saturating_sub(maximum)..].join("\n")
}

fn optional_uuid_param(params: &Value, name: &str) -> Result<Option<uuid::Uuid>, String> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => uuid::Uuid::parse_str(value)
            .map(Some)
            .map_err(|_| format!("invalid params.{name}")),
        Some(_) => Err(format!("params.{name} must be a UUID string")),
    }
}

fn explicit_terminal_target(
    params: &Value,
) -> Result<(Option<TerminalId>, Option<SurfaceId>), String> {
    let terminal = optional_uuid_param(params, "terminal")?;
    let surface = optional_uuid_param(params, "surface")?;
    if terminal.is_none() && surface.is_none() {
        return Err("missing params.terminal or params.surface".into());
    }
    Ok((terminal, surface))
}

fn rpc_target_pane_in_state(state: &AppState, params: &Value) -> Result<(usize, PaneId), String> {
    let requested_pane = optional_uuid_param(params, "pane")?;
    let requested_surface = optional_uuid_param(params, "surface")?;
    let requested_workspace = optional_uuid_param(params, "workspace")?;
    let surface_pane = requested_surface
        .map(|surface| {
            state
                .panes
                .iter()
                .find(|(_, pane)| pane.surfaces.contains(&surface))
                .map(|(id, _)| *id)
                .ok_or_else(|| "target surface not found".to_owned())
        })
        .transpose()?;
    if let (Some(pane), Some(surface_pane)) = (requested_pane, surface_pane)
        && pane != surface_pane
    {
        return Err("target pane does not contain target surface".into());
    }
    let workspace_index = requested_workspace
        .map(|id| {
            state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == id)
                .ok_or_else(|| "target workspace not found".to_owned())
        })
        .transpose()?;
    let pane = requested_pane
        .or(surface_pane)
        .or_else(|| workspace_index.map(|index| state.workspaces[index].active_pane))
        .ok_or_else(|| "missing pane, surface, or workspace target".to_owned())?;
    if !state.panes.contains_key(&pane) {
        return Err("target pane not found".into());
    }
    let workspace = state
        .workspaces
        .iter()
        .position(|workspace| workspace.root.contains(pane))
        .ok_or_else(|| "target pane is not in a workspace".to_owned())?;
    if workspace_index.is_some_and(|requested| requested != workspace) {
        return Err("target pane is not in target workspace".into());
    }
    Ok((workspace, pane))
}

fn focus_created_surface(
    state: &mut AppState,
    workspace_index: usize,
    pane_id: PaneId,
    surface_id: Option<SurfaceId>,
    focus: bool,
) {
    if !focus {
        return;
    }
    state.active_workspace = workspace_index;
    if let Some(workspace) = state.workspaces.get_mut(workspace_index) {
        workspace.active_pane = pane_id;
    }
    if let (Some(surface), Some(pane)) = (surface_id, state.panes.get_mut(&pane_id))
        && let Some(index) = pane.surfaces.iter().position(|id| *id == surface)
    {
        pane.active_surface = index;
    }
}

fn parse_terminal_create_options(params: &Value) -> Result<TerminalCreateOptions, String> {
    let command = optional_limited_string(params, "command", 65_536)?;
    let title = optional_limited_string(params, "title", 256)?.map(|title| title.trim().to_owned());
    if title.as_deref().is_some_and(str::is_empty) {
        return Err("params.title must not be empty".into());
    }
    if title
        .as_deref()
        .is_some_and(|title| title.chars().any(char::is_control))
    {
        return Err("params.title must not contain control characters".into());
    }
    let cwd = match params.get("cwd") {
        None | Some(Value::Null) => None,
        Some(Value::String(value)) if value.len() <= 4096 => {
            let path = std::path::PathBuf::from(value);
            if !path.is_dir() {
                return Err("params.cwd must name an existing directory".into());
            }
            Some(path)
        }
        Some(Value::String(_)) => return Err("params.cwd is too long".into()),
        Some(_) => return Err("params.cwd must be a string".into()),
    };
    let environment = parse_environment(params.get("environment"))?;
    Ok(TerminalCreateOptions {
        command,
        cwd,
        environment,
        title,
    })
}

fn optional_limited_string(
    params: &Value,
    name: &str,
    maximum: usize,
) -> Result<Option<String>, String> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) if value.len() <= maximum && !value.contains('\0') => {
            Ok(Some(value.clone()))
        }
        Some(Value::String(_)) => Err(format!("params.{name} is too long or contains NUL")),
        Some(_) => Err(format!("params.{name} must be a string")),
    }
}

fn parse_environment(value: Option<&Value>) -> Result<Vec<(String, String)>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let object = value
        .as_object()
        .ok_or_else(|| "params.environment must be an object of strings".to_owned())?;
    if object.len() > 64 {
        return Err("params.environment may contain at most 64 entries".into());
    }
    object
        .iter()
        .map(|(key, value)| {
            if !valid_environment_key(key) {
                return Err(format!("unsafe environment key: {key}"));
            }
            let value = value
                .as_str()
                .ok_or_else(|| format!("environment value for {key} must be a string"))?;
            if value.len() > 32_768 || value.contains('\0') {
                return Err(format!(
                    "environment value for {key} is too long or contains NUL"
                ));
            }
            Ok((key.clone(), value.to_owned()))
        })
        .collect()
}

fn valid_environment_key(key: &str) -> bool {
    if key.is_empty() || key.len() > 128 {
        return false;
    }
    let mut chars = key.chars();
    if !chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        || !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
    {
        return false;
    }
    let upper = key.to_ascii_uppercase();
    upper != "WMCTERM"
        && !upper.starts_with("WMCTERM_")
        && upper != "LD_PRELOAD"
        && !upper.starts_with("DYLD_")
}

fn resolve_existing_directory(
    input: &str,
    relative_to: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    if input.trim().is_empty() {
        return Err("path cannot be empty".into());
    }
    let expanded = wmcterm_config::expand_home_path(std::path::Path::new(input.trim()));
    let candidate = if expanded.is_absolute() {
        expanded
    } else {
        relative_to.join(expanded)
    };
    let resolved = candidate
        .canonicalize()
        .map_err(|error| format!("{} ({error})", candidate.display()))?;
    if !resolved.is_dir() {
        return Err(format!("{} is not a directory", resolved.display()));
    }
    Ok(wmcterm_config::normalize_windows_path(resolved))
}

fn select_new_workspace_cwd(
    inherit_cwd: bool,
    active: Option<&std::path::Path>,
    default: Option<&std::path::Path>,
    process: &std::path::Path,
) -> std::path::PathBuf {
    inherit_cwd
        .then_some(active)
        .flatten()
        .or(default)
        .unwrap_or(process)
        .to_path_buf()
}

fn split_direction(params: &Value) -> Result<SplitPlacement, String> {
    match params
        .get("direction")
        .and_then(Value::as_str)
        .unwrap_or("right")
    {
        "right" | "vertical" => Ok(SplitPlacement {
            axis: SplitAxis::Vertical,
            new_first: false,
        }),
        "left" => Ok(SplitPlacement {
            axis: SplitAxis::Vertical,
            new_first: true,
        }),
        "down" | "horizontal" => Ok(SplitPlacement {
            axis: SplitAxis::Horizontal,
            new_first: false,
        }),
        "up" => Ok(SplitPlacement {
            axis: SplitAxis::Horizontal,
            new_first: true,
        }),
        _ => Err("params.direction must be left, right, up, down, vertical, or horizontal".into()),
    }
}

fn place_new_split_first(node: &mut PaneNode, target: PaneId, new_pane: PaneId) -> bool {
    let PaneNode::Split { first, second, .. } = node else {
        return false;
    };
    if matches!(&**first, PaneNode::Leaf { pane } if *pane == target)
        && matches!(&**second, PaneNode::Leaf { pane } if *pane == new_pane)
    {
        std::mem::swap(first, second);
        return true;
    }
    place_new_split_first(first, target, new_pane)
        || place_new_split_first(second, target, new_pane)
}

#[derive(Clone, Copy)]
struct DocumentColors {
    text: [u8; 4],
    muted: [u8; 4],
    accent: [u8; 4],
    heading: [u8; 4],
    code: [u8; 4],
    warning: [u8; 4],
    error: [u8; 4],
}

const fn neutral_document_colors() -> DocumentColors {
    DocumentColors {
        text: [255; 4],
        muted: [160, 160, 160, 255],
        accent: [120, 160, 255, 255],
        heading: [255, 220, 120, 255],
        code: [140, 220, 140, 255],
        warning: [255, 180, 100, 255],
        error: [255, 100, 100, 255],
    }
}

const fn tab_context_action_enabled(action: TabContextAction, tab_count: usize) -> bool {
    match action {
        TabContextAction::Close => tab_count > 0,
        TabContextAction::CloseOthers => tab_count > 1,
        TabContextAction::NewSurface => true,
    }
}

fn terminal_context_action_enabled(
    action: TerminalContextAction,
    selection: Option<MouseSelection>,
    pane: PaneId,
) -> bool {
    match action {
        TerminalContextAction::Copy => match selection {
            Some(selection) => selection.pane == pane && selection.start != selection.end,
            None => false,
        },
        TerminalContextAction::Paste
        | TerminalContextAction::CopyAll
        | TerminalContextAction::ClearScrollback => true,
    }
}

const fn workspace_context_action_enabled(
    action: WorkspaceContextAction,
    workspace_count: usize,
) -> bool {
    match action {
        WorkspaceContextAction::Close => workspace_count > 0,
        WorkspaceContextAction::NewSurface
        | WorkspaceContextAction::OpenFileManager
        | WorkspaceContextAction::OpenTerminal
        | WorkspaceContextAction::Rename
        | WorkspaceContextAction::Describe
        | WorkspaceContextAction::ChangeDirectory => true,
    }
}

fn open_directory_in_file_manager(path: &Path) -> Result<()> {
    anyhow::ensure!(
        path.is_dir(),
        "workspace directory does not exist: {}",
        path.display()
    );
    #[cfg(target_os = "linux")]
    ProcessCommand::new("xdg-open")
        .arg(path)
        .spawn()
        .context("starting xdg-open")?;
    #[cfg(target_os = "macos")]
    ProcessCommand::new("open")
        .arg(path)
        .spawn()
        .context("starting open")?;
    #[cfg(windows)]
    ProcessCommand::new("explorer.exe")
        .arg(path)
        .spawn()
        .context("starting Explorer")?;
    Ok(())
}

fn open_directory_in_terminal(path: &Path) -> Result<()> {
    anyhow::ensure!(
        path.is_dir(),
        "workspace directory does not exist: {}",
        path.display()
    );
    #[cfg(target_os = "linux")]
    {
        let directory_argument = format!("--working-directory={}", path.display());
        let candidates: [(&str, &[&std::ffi::OsStr]); 6] = [
            ("x-terminal-emulator", &[]),
            ("gnome-terminal", &[directory_argument.as_ref()]),
            ("konsole", &["--workdir".as_ref(), path.as_os_str()]),
            ("xfce4-terminal", &[directory_argument.as_ref()]),
            ("kitty", &["--directory".as_ref(), path.as_os_str()]),
            (
                "alacritty",
                &["--working-directory".as_ref(), path.as_os_str()],
            ),
        ];
        let mut last_error = None;
        for (program, arguments) in candidates {
            match ProcessCommand::new(program)
                .args(arguments)
                .current_dir(path)
                .spawn()
            {
                Ok(_) => return Ok(()),
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow::anyhow!("no supported terminal emulator found")))
        .context("starting an external terminal")
    }
    #[cfg(target_os = "macos")]
    ProcessCommand::new("open")
        .args(["-a", "Terminal"])
        .arg(path)
        .spawn()
        .context("starting Terminal")?;
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_NEW_CONSOLE;

        // Launch a real console process instead of the wt.exe app-execution
        // alias: an alias can be created successfully and then exit without a
        // window. Windows routes this new console through the configured
        // default terminal host (Windows Terminal on systems that select it).
        let mut powershell = ProcessCommand::new("powershell.exe");
        powershell
            .args(["-NoLogo", "-NoExit"])
            .current_dir(path)
            .creation_flags(CREATE_NEW_CONSOLE);
        if powershell.spawn().is_err() {
            let mut command_prompt = ProcessCommand::new("cmd.exe");
            command_prompt
                .arg("/D")
                .current_dir(path)
                .creation_flags(CREATE_NEW_CONSOLE);
            command_prompt
                .spawn()
                .context("starting the fallback Windows terminal")?;
        }
    }
    #[cfg(not(target_os = "linux"))]
    Ok(())
}

fn context_menu_frame(panel: ui::Bounds, background: [u8; 4], border: [u8; 4]) -> [RectBlock; 3] {
    [
        RectBlock {
            left: panel.x + 4.0,
            top: panel.y + 4.0,
            width: panel.width,
            height: panel.height,
            color: [0, 0, 0, 105],
        },
        RectBlock {
            left: panel.x,
            top: panel.y,
            width: panel.width,
            height: panel.height,
            color: border,
        },
        RectBlock {
            left: panel.x + 1.0,
            top: panel.y + 1.0,
            width: (panel.width - 2.0).max(1.0),
            height: (panel.height - 2.0).max(1.0),
            color: background,
        },
    ]
}

fn notification_destination(
    state: &AppState,
    notification: uuid::Uuid,
) -> Option<NotificationDestination> {
    let notification = state
        .notifications
        .iter()
        .find(|item| item.id == notification)?;
    let workspace_index = state
        .workspaces
        .iter()
        .position(|workspace| workspace.id == notification.workspace)?;
    let workspace = &state.workspaces[workspace_index];
    let surface = state.panes.iter().find_map(|(pane_id, pane)| {
        if !workspace.root.contains(*pane_id) {
            return None;
        }
        pane.surfaces
            .iter()
            .position(|surface| *surface == notification.surface)
            .map(|index| (*pane_id, index))
    });
    Some(NotificationDestination {
        workspace_index,
        pane: surface,
        surface: notification.surface,
    })
}

fn non_actionable_agent_exit(event: &AgentEvent) -> bool {
    let AgentEvent::Stop {
        success: true,
        message: Some(message),
    } = event
    else {
        return false;
    };
    matches!(
        message
            .trim()
            .to_ascii_lowercase()
            .replace(['-', '_'], " ")
            .as_str(),
        "other" | "exit" | "exited" | "user exit" | "user requested" | "logout"
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StyledDocumentLine {
    text: String,
    color: [u8; 4],
    bold: bool,
    italic: bool,
}

fn plain_span(text: impl Into<String>, color: [u8; 4]) -> TextSpan {
    TextSpan {
        text: text.into(),
        color,
        bold: false,
        italic: false,
    }
}

fn markdown_lines(document: &MarkdownDocument, colors: DocumentColors) -> Vec<StyledDocumentLine> {
    let mut lines = Vec::new();
    for block in &document.blocks {
        match block {
            MarkdownBlock::Heading { level, text } => lines.push(StyledDocumentLine {
                text: format!("{} {text}", if *level <= 2 { "▌" } else { "·" }),
                color: if *level == 1 {
                    colors.heading
                } else {
                    colors.accent
                },
                bold: true,
                italic: false,
            }),
            MarkdownBlock::Paragraph(text) => lines.push(StyledDocumentLine {
                text: text.clone(),
                color: colors.text,
                bold: false,
                italic: false,
            }),
            MarkdownBlock::CodeBlock { language, code } => {
                lines.push(StyledDocumentLine {
                    text: format!("┌─ {}", language.as_deref().unwrap_or("code")),
                    color: colors.muted,
                    bold: true,
                    italic: false,
                });
                lines.extend(code.lines().map(|line| StyledDocumentLine {
                    text: format!("│ {line}"),
                    color: colors.code,
                    bold: false,
                    italic: false,
                }));
                lines.push(StyledDocumentLine {
                    text: "└─".into(),
                    color: colors.muted,
                    bold: false,
                    italic: false,
                });
            }
            MarkdownBlock::List { ordered, items } => {
                lines.extend(
                    items
                        .iter()
                        .enumerate()
                        .map(|(index, item)| StyledDocumentLine {
                            text: if *ordered {
                                format!("  {}. {item}", index + 1)
                            } else {
                                format!("  • {item}")
                            },
                            color: colors.text,
                            bold: false,
                            italic: false,
                        }),
                );
            }
            MarkdownBlock::Quote(text) => {
                lines.extend(text.lines().map(|line| StyledDocumentLine {
                    text: format!("│ {line}"),
                    color: colors.muted,
                    bold: false,
                    italic: true,
                }));
            }
            MarkdownBlock::ThematicBreak => lines.push(StyledDocumentLine {
                text: "────────────────────────────────".into(),
                color: colors.muted,
                bold: false,
                italic: false,
            }),
        }
        lines.push(StyledDocumentLine {
            text: String::new(),
            color: colors.text,
            bold: false,
            italic: false,
        });
    }
    lines
}

fn markdown_spans(
    document: &MarkdownDocument,
    offset: usize,
    limit: usize,
    colors: DocumentColors,
    search: Option<&str>,
) -> Vec<TextSpan> {
    styled_lines_to_spans(
        markdown_lines(document, colors),
        offset,
        limit,
        search,
        colors.heading,
    )
}

fn log_level_color(level: LogLevel, colors: DocumentColors) -> [u8; 4] {
    match level {
        LogLevel::Trace | LogLevel::Debug => colors.muted,
        LogLevel::Info | LogLevel::Unknown => colors.text,
        LogLevel::Warn => colors.warning,
        LogLevel::Error | LogLevel::Fatal => colors.error,
    }
}

fn log_lines(logs: &LogSurface, colors: DocumentColors) -> Vec<StyledDocumentLine> {
    logs.visible_entries()
        .map(|entry| StyledDocumentLine {
            text: entry.text.clone(),
            color: log_level_color(entry.level, colors),
            bold: matches!(entry.level, LogLevel::Error | LogLevel::Fatal),
            italic: false,
        })
        .collect()
}

fn log_spans(
    logs: &LogSurface,
    offset: usize,
    limit: usize,
    colors: DocumentColors,
    search: Option<&str>,
) -> Vec<TextSpan> {
    styled_lines_to_spans(
        log_lines(logs, colors),
        offset,
        limit,
        search,
        colors.heading,
    )
}

fn styled_lines_to_spans(
    lines: Vec<StyledDocumentLine>,
    offset: usize,
    limit: usize,
    search: Option<&str>,
    match_color: [u8; 4],
) -> Vec<TextSpan> {
    let query = search
        .filter(|query| !query.is_empty())
        .map(str::to_lowercase);
    let visible: Vec<_> = lines.into_iter().skip(offset).take(limit).collect();
    let visible_len = visible.len();
    visible
        .into_iter()
        .enumerate()
        .map(|(index, line)| TextSpan {
            text: format!(
                "{}{}",
                line.text,
                if index + 1 < visible_len { "\n" } else { "" }
            ),
            color: if query
                .as_ref()
                .is_some_and(|query| line.text.to_lowercase().contains(query))
            {
                match_color
            } else {
                line.color
            },
            bold: line.bold,
            italic: line.italic,
        })
        .collect()
}

fn document_search_offsets(backend: &dyn SurfaceBackend, query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    let query = query.to_lowercase();
    if let Some(markdown) = backend.as_any().downcast_ref::<MarkdownSurface>() {
        return markdown_lines(markdown.document(), neutral_document_colors())
            .iter()
            .enumerate()
            .filter_map(|(index, line)| line.text.to_lowercase().contains(&query).then_some(index))
            .collect();
    }
    if let Some(logs) = backend.as_any().downcast_ref::<LogSurface>() {
        return logs
            .visible_entries()
            .enumerate()
            .filter_map(|(index, entry)| {
                entry.text.to_lowercase().contains(&query).then_some(index)
            })
            .collect();
    }
    backend
        .render_text()
        .lines()
        .enumerate()
        .filter_map(|(index, line)| line.to_lowercase().contains(&query).then_some(index))
        .collect()
}

fn parse_log_filter(input: &str) -> LogFilter {
    let mut levels = None;
    let mut query = Vec::new();
    for token in input.split_whitespace() {
        if let Some(values) = token.strip_prefix("level:") {
            let parsed: Vec<_> = values.split(',').filter_map(parse_log_level).collect();
            if !parsed.is_empty() {
                levels = Some(parsed);
            }
        } else {
            query.push(token);
        }
    }
    LogFilter {
        query: (!query.is_empty()).then(|| query.join(" ")),
        levels,
    }
}

fn parse_log_level(value: &str) -> Option<LogLevel> {
    match value.to_ascii_lowercase().as_str() {
        "trace" => Some(LogLevel::Trace),
        "debug" => Some(LogLevel::Debug),
        "info" => Some(LogLevel::Info),
        "warn" | "warning" => Some(LogLevel::Warn),
        "error" | "err" => Some(LogLevel::Error),
        "fatal" | "panic" => Some(LogLevel::Fatal),
        "unknown" => Some(LogLevel::Unknown),
        _ => None,
    }
}

fn format_log_filter(filter: &LogFilter) -> String {
    let mut parts = Vec::new();
    if let Some(levels) = &filter.levels {
        let values = levels
            .iter()
            .map(|level| format!("{level:?}").to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(",");
        parts.push(format!("level:{values}"));
    }
    if let Some(query) = &filter.query {
        parts.push(query.clone());
    }
    parts.join(" ")
}

fn editor_spans(
    editor: &TextEditor,
    normal: [u8; 4],
    selected: [u8; 4],
    cursor_text: [u8; 4],
    viewport: TextViewport,
) -> Vec<TextSpan> {
    fn push(spans: &mut Vec<TextSpan>, text: &str, color: [u8; 4]) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = spans.last_mut()
            && last.color == color
            && !last.bold
            && !last.italic
        {
            last.text.push_str(text);
        } else {
            spans.push(TextSpan {
                text: text.into(),
                color,
                bold: false,
                italic: false,
            });
        }
    }

    let selection = editor.selection_range();
    let cursor = editor.cursor_byte();
    let mut spans = Vec::new();
    for (byte, character) in editor
        .text()
        .char_indices()
        .filter(|(byte, _)| *byte >= viewport.start_byte && *byte < viewport.end_byte)
    {
        if byte == cursor && editor.has_preedit() {
            push(&mut spans, &format!("「{}」", editor.preedit()), normal);
        }
        let color = if selection
            .as_ref()
            .is_some_and(|range| range.contains(&byte))
        {
            selected
        } else if selection.is_none() && byte == cursor && !editor.has_preedit() {
            cursor_text
        } else {
            normal
        };
        push(&mut spans, &character.to_string(), color);
    }
    if cursor == editor.text().len()
        && cursor >= viewport.start_byte
        && cursor <= viewport.end_byte
        && editor.has_preedit()
    {
        push(&mut spans, &format!("「{}」", editor.preedit()), normal);
    }
    spans
}

struct EditorHighlightStyle {
    character_width: f32,
    colors: ([u8; 4], [u8; 4]),
    viewport: TextViewport,
}

fn push_editor_highlights(
    rectangles: &mut Vec<RectBlock>,
    editor: &TextEditor,
    left: f32,
    top: f32,
    height: f32,
    style: EditorHighlightStyle,
) {
    let EditorHighlightStyle {
        character_width,
        colors: (selection, cursor),
        viewport,
    } = style;
    if let Some((start, end)) = editor.selection_display_columns() {
        let start = start.max(viewport.start_column);
        let end = end.min(viewport.start_column + viewport.columns);
        if start >= end {
            return;
        }
        rectangles.push(RectBlock {
            left: left + (start - viewport.start_column) as f32 * character_width,
            top,
            width: ((end - start) as f32 * character_width).max(1.0),
            height,
            color: selection,
        });
    } else if !editor.has_preedit() {
        rectangles.push(RectBlock {
            left: left
                + (editor.cursor_display_column() - viewport.start_column) as f32 * character_width,
            top,
            width: editor.cursor_cell_width() as f32 * character_width,
            height,
            color: cursor,
        });
    }
}

fn editor_columns(width: f32, character_width: f32) -> usize {
    (width.max(character_width) / character_width).floor() as usize
}

const fn theme_rgba(color: ThemeColor) -> [u8; 4] {
    [color.r, color.g, color.b, color.a]
}

fn background_opacity(mut color: [u8; 4], opacity: f32) -> [u8; 4] {
    color[3] = (color[3] as f32 * opacity.clamp(0.0, 1.0)).round() as u8;
    color
}

fn push_terminal_span(
    spans: &mut Vec<TextSpan>,
    text: &str,
    color: [u8; 4],
    bold: bool,
    italic: bool,
) {
    if let Some(last) = spans.last_mut()
        && last.color == color
        && last.bold == bold
        && last.italic == italic
    {
        last.text.push_str(text);
    } else {
        spans.push(TextSpan {
            text: text.to_owned(),
            color,
            bold,
            italic,
        });
    }
}

fn terminal_cell_is_selected(
    column: usize,
    screen_row: usize,
    (start, end): ((usize, usize), (usize, usize)),
) -> bool {
    screen_row >= start.1
        && screen_row <= end.1
        && (screen_row != start.1 || column >= start.0)
        && (screen_row != end.1 || column <= end.0)
}

fn terminal_selection_background(row: &[CellSnapshot], column: usize) -> [u8; 4] {
    let cell = &row[wide_cell_lead(row, column)];
    [cell.foreground.r, cell.foreground.g, cell.foreground.b, 255]
}

fn contrasting_selection_foreground(background: [u8; 4]) -> [u8; 4] {
    let linear = |component: u8| {
        let value = component as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * linear(background[0])
        + 0.7152 * linear(background[1])
        + 0.0722 * linear(background[2]);
    if luminance > 0.179 {
        [0, 0, 0, 255]
    } else {
        [255, 255, 255, 255]
    }
}

#[allow(clippy::too_many_arguments)]
fn push_terminal_selection_rect(
    rectangles: &mut Vec<RectBlock>,
    terminal: Rect,
    row: usize,
    from: usize,
    to: usize,
    cell_width: f32,
    cell_height: f32,
    color: [u8; 4],
) {
    rectangles.push(RectBlock {
        left: terminal.x + 8.0 + from as f32 * cell_width,
        top: terminal.y + 25.0 + row as f32 * cell_height,
        width: (to - from + 1) as f32 * cell_width,
        height: cell_height,
        color,
    });
}

const fn brighten_rgba([red, green, blue, alpha]: [u8; 4], amount: u8) -> [u8; 4] {
    [
        red.saturating_add(amount),
        green.saturating_add(amount),
        blue.saturating_add(amount),
        alpha,
    ]
}

fn terminal_theme(theme: &ThemeConfig) -> TerminalTheme {
    let rgb = |color: ThemeColor| Rgb {
        r: color.r,
        g: color.g,
        b: color.b,
    };
    TerminalTheme {
        foreground: rgb(theme.foreground),
        background: rgb(theme.background),
        cursor: rgb(theme.cursor),
        palette: theme.palette.iter().copied().map(rgb).collect(),
    }
}

fn terminal_background_is_light(color: ThemeColor) -> bool {
    (0.299 * f32::from(color.r) + 0.587 * f32::from(color.g) + 0.114 * f32::from(color.b)) / 255.0
        > 0.5
}

fn application_icon() -> Option<Icon> {
    let rgba = image::load_from_memory(include_bytes!("../../../assets/icons/icon.png"))
        .ok()?
        .into_rgba8();
    let (width, height) = rgba.dimensions();
    Icon::from_rgba(rgba.into_raw(), width, height).ok()
}

fn physical_ascii_character(key: PhysicalKey) -> Option<char> {
    let character = match key {
        PhysicalKey::Code(KeyCode::KeyA) => 'a',
        PhysicalKey::Code(KeyCode::KeyB) => 'b',
        PhysicalKey::Code(KeyCode::KeyC) => 'c',
        PhysicalKey::Code(KeyCode::KeyD) => 'd',
        PhysicalKey::Code(KeyCode::KeyE) => 'e',
        PhysicalKey::Code(KeyCode::KeyF) => 'f',
        PhysicalKey::Code(KeyCode::KeyG) => 'g',
        PhysicalKey::Code(KeyCode::KeyH) => 'h',
        PhysicalKey::Code(KeyCode::KeyI) => 'i',
        PhysicalKey::Code(KeyCode::KeyJ) => 'j',
        PhysicalKey::Code(KeyCode::KeyK) => 'k',
        PhysicalKey::Code(KeyCode::KeyL) => 'l',
        PhysicalKey::Code(KeyCode::KeyM) => 'm',
        PhysicalKey::Code(KeyCode::KeyN) => 'n',
        PhysicalKey::Code(KeyCode::KeyO) => 'o',
        PhysicalKey::Code(KeyCode::KeyP) => 'p',
        PhysicalKey::Code(KeyCode::KeyQ) => 'q',
        PhysicalKey::Code(KeyCode::KeyR) => 'r',
        PhysicalKey::Code(KeyCode::KeyS) => 's',
        PhysicalKey::Code(KeyCode::KeyT) => 't',
        PhysicalKey::Code(KeyCode::KeyU) => 'u',
        PhysicalKey::Code(KeyCode::KeyV) => 'v',
        PhysicalKey::Code(KeyCode::KeyW) => 'w',
        PhysicalKey::Code(KeyCode::KeyX) => 'x',
        PhysicalKey::Code(KeyCode::KeyY) => 'y',
        PhysicalKey::Code(KeyCode::KeyZ) => 'z',
        _ => return None,
    };
    Some(character)
}

fn terminal_input_key(logical_key: &Key, physical_character: Option<char>) -> Option<InputKey> {
    match logical_key {
        Key::Character(value) => value
            .chars()
            .next()
            .filter(|character| !character.is_control())
            .or(physical_character)
            .map(InputKey::Character),
        Key::Named(NamedKey::Enter) => Some(InputKey::Enter),
        Key::Named(NamedKey::Backspace) => Some(InputKey::Backspace),
        Key::Named(NamedKey::Tab) => Some(InputKey::Tab),
        Key::Named(NamedKey::Escape) => Some(InputKey::Escape),
        Key::Named(NamedKey::ArrowUp) => Some(InputKey::Up),
        Key::Named(NamedKey::ArrowDown) => Some(InputKey::Down),
        Key::Named(NamedKey::ArrowRight) => Some(InputKey::Right),
        Key::Named(NamedKey::ArrowLeft) => Some(InputKey::Left),
        Key::Named(NamedKey::Home) => Some(InputKey::Home),
        Key::Named(NamedKey::End) => Some(InputKey::End),
        Key::Named(NamedKey::PageUp) => Some(InputKey::PageUp),
        Key::Named(NamedKey::PageDown) => Some(InputKey::PageDown),
        Key::Named(NamedKey::Delete) => Some(InputKey::Delete),
        Key::Named(NamedKey::Insert) => Some(InputKey::Insert),
        Key::Named(NamedKey::Space) => Some(InputKey::Space),
        _ => None,
    }
}

fn is_window_switch_shortcut(
    logical_key: &Key,
    physical_key: PhysicalKey,
    modifiers: ModifiersState,
    physical_alt_pressed: bool,
) -> bool {
    let tab = matches!(logical_key, Key::Named(NamedKey::Tab))
        || matches!(physical_key, PhysicalKey::Code(KeyCode::Tab));
    tab && (modifiers.alt_key() || modifiers.super_key() || physical_alt_pressed)
}

#[cfg(windows)]
fn platform_keyboard_has_pressed_keys() -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    // Virtual-key values 0x08 through 0xfe cover keyboard keys while excluding
    // the mouse-button range. The high bit reports the current physical state;
    // the low bit is a process-global "pressed since last query" flag and must
    // not keep the quarantine active.
    (0x08..=0xfe).any(|virtual_key| unsafe { GetAsyncKeyState(virtual_key) } < 0)
}

#[cfg(not(windows))]
fn platform_keyboard_has_pressed_keys() -> bool {
    false
}

fn focus_input_should_remain_quarantined(
    focused_for: std::time::Duration,
    platform_keyboard_has_pressed_keys: bool,
) -> bool {
    platform_keyboard_has_pressed_keys || focused_for < FOCUS_INPUT_QUARANTINE_GRACE
}

fn should_suppress_focus_transition_key(
    window_focused: bool,
    logical_key: &Key,
    physical_key: PhysicalKey,
    modifiers: ModifiersState,
    physical_alt_pressed: bool,
) -> bool {
    !window_focused
        || is_window_switch_shortcut(logical_key, physical_key, modifiers, physical_alt_pressed)
}

fn should_defer_unmodified_tab(
    window_focused: bool,
    event: &KeyEvent,
    modifiers: ModifiersState,
    physical_alt_pressed: bool,
) -> bool {
    window_focused
        && event.state == ElementState::Pressed
        && matches!(event.physical_key, PhysicalKey::Code(KeyCode::Tab))
        && !modifiers.alt_key()
        && !modifiers.super_key()
        && !physical_alt_pressed
}

fn should_suppress_recent_focus_tab(
    state: ElementState,
    physical_key: PhysicalKey,
    focused_for: std::time::Duration,
) -> bool {
    state == ElementState::Pressed
        && matches!(physical_key, PhysicalKey::Code(KeyCode::Tab))
        && focused_for < UNMODIFIED_TAB_FOCUS_GRACE
}

fn text_edit_shortcut(event: &KeyEvent, modifiers: ModifiersState) -> Option<TextEditShortcut> {
    if !(modifiers.control_key() || modifiers.super_key()) || modifiers.alt_key() {
        return None;
    }
    let character = match &event.logical_key {
        Key::Character(value) if value.len() == 1 => value.chars().next(),
        _ => physical_ascii_character(event.physical_key),
    }?;
    match character.to_ascii_lowercase() {
        'a' => Some(TextEditShortcut::SelectAll),
        'c' => Some(TextEditShortcut::Copy),
        'x' => Some(TextEditShortcut::Cut),
        _ => None,
    }
}

fn apply_text_edit_shortcut(editor: &mut TextEditor, shortcut: TextEditShortcut) -> Option<String> {
    match shortcut {
        TextEditShortcut::SelectAll => {
            editor.select_all();
            None
        }
        TextEditShortcut::Copy => editor.selected_text().map(ToOwned::to_owned),
        TextEditShortcut::Cut => editor.take_selection(),
    }
}

fn shortcut_for_event(event: &KeyEvent, modifiers: ModifiersState) -> Option<Shortcut> {
    let key = match &event.logical_key {
        Key::Character(value) if !value.chars().any(char::is_control) => value.to_ascii_lowercase(),
        Key::Named(NamedKey::Enter) => "enter".into(),
        Key::Named(NamedKey::Tab) => "tab".into(),
        Key::Named(NamedKey::Space) => "space".into(),
        Key::Named(NamedKey::Escape) => "escape".into(),
        Key::Named(NamedKey::Backspace) => "backspace".into(),
        Key::Named(NamedKey::Delete) => "delete".into(),
        Key::Named(NamedKey::Insert) => "insert".into(),
        Key::Named(NamedKey::Home) => "home".into(),
        Key::Named(NamedKey::End) => "end".into(),
        Key::Named(NamedKey::PageUp) => "pageup".into(),
        Key::Named(NamedKey::PageDown) => "pagedown".into(),
        Key::Named(NamedKey::ArrowUp) => "up".into(),
        Key::Named(NamedKey::ArrowDown) => "down".into(),
        Key::Named(NamedKey::ArrowLeft) => "left".into(),
        Key::Named(NamedKey::ArrowRight) => "right".into(),
        _ => physical_ascii_character(event.physical_key)?.to_string(),
    };
    Some(Shortcut {
        ctrl: modifiers.control_key(),
        alt: modifiers.alt_key(),
        shift: modifiers.shift_key(),
        super_key: modifiers.super_key(),
        key,
    })
}

fn format_shortcut_hint(shortcut: &Shortcut) -> String {
    let mut parts = Vec::new();
    if shortcut.ctrl {
        parts.push("Ctrl".to_owned());
    }
    if shortcut.alt {
        parts.push("Alt".to_owned());
    }
    if shortcut.shift {
        parts.push("Shift".to_owned());
    }
    if shortcut.super_key {
        parts.push("Super".to_owned());
    }
    let key = match shortcut.key.as_str() {
        "escape" => "Esc".to_owned(),
        "pageup" => "PageUp".to_owned(),
        "pagedown" => "PageDown".to_owned(),
        "backspace" => "Backspace".to_owned(),
        "delete" => "Delete".to_owned(),
        "insert" => "Insert".to_owned(),
        "enter" => "Enter".to_owned(),
        "space" => "Space".to_owned(),
        "left" => "←".to_owned(),
        "right" => "→".to_owned(),
        "up" => "↑".to_owned(),
        "down" => "↓".to_owned(),
        key if key.len() == 1 => key.to_ascii_uppercase(),
        key => {
            let mut characters = key.chars();
            characters
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + characters.as_str())
                .unwrap_or_default()
        }
    };
    parts.push(key);
    parts.join("+")
}

fn workspace_requires_close_confirmation(terminal_count: usize, process_count: usize) -> bool {
    terminal_count > 1 || process_count > 0
}

fn application_requires_close_confirmation(
    process_count: usize,
    pending_request_count: usize,
) -> bool {
    process_count > 0 || pending_request_count > 0
}

fn confirmation_cursor_icon(cancel: ui::Bounds, confirm: ui::Bounds, x: f32, y: f32) -> CursorIcon {
    if confirmation_hover(cancel, confirm, x, y).is_some() {
        CursorIcon::Pointer
    } else {
        CursorIcon::Default
    }
}

fn confirmation_hover(
    cancel: ui::Bounds,
    confirm: ui::Bounds,
    x: f32,
    y: f32,
) -> Option<NavigationHover> {
    if cancel.contains(x, y) {
        Some(NavigationHover::ConfirmationCancel)
    } else if confirm.contains(x, y) {
        Some(NavigationHover::ConfirmationConfirm)
    } else {
        None
    }
}

fn codex_trust_confirmation_hover(
    cancel: ui::Bounds,
    proceed: ui::Bounds,
    trust: ui::Bounds,
    x: f32,
    y: f32,
) -> Option<NavigationHover> {
    if cancel.contains(x, y) {
        Some(NavigationHover::ConfirmationCancel)
    } else if proceed.contains(x, y) {
        Some(NavigationHover::ConfirmationContinue)
    } else if trust.contains(x, y) {
        Some(NavigationHover::ConfirmationConfirm)
    } else {
        None
    }
}

fn agent_state_priority(state: wmcterm_core::AgentState) -> u8 {
    match state {
        wmcterm_core::AgentState::Error => 5,
        wmcterm_core::AgentState::WaitingForInput
        | wmcterm_core::AgentState::WaitingForPermission => 4,
        wmcterm_core::AgentState::Running => 3,
        wmcterm_core::AgentState::Completed => 2,
        wmcterm_core::AgentState::Idle | wmcterm_core::AgentState::Unknown => 1,
    }
}

fn detected_foreground_agent_metadata(
    agent: AgentKind,
    run_mode: AgentRunMode,
) -> wmcterm_core::AgentMetadata {
    wmcterm_core::AgentMetadata {
        name: agent.as_str().to_owned(),
        state: wmcterm_core::AgentState::Idle,
        session_id: None,
        run_mode,
    }
}

const MAX_TRACKED_AGENT_INPUT_BYTES: usize = 4096;

fn append_bounded_agent_input(input: &mut String, pasted: &str) {
    let remaining = MAX_TRACKED_AGENT_INPUT_BYTES.saturating_sub(input.len());
    let mut end = pasted.len().min(remaining);
    while !pasted.is_char_boundary(end) {
        end -= 1;
    }
    input.push_str(&pasted[..end]);
}

fn agent_submission_starts_turn(input: &str) -> bool {
    let input = input.trim_start();
    if !input.starts_with('/') {
        return !input.is_empty();
    }

    let mut words = input.split_whitespace();
    matches!(
        (words.next(), words.next()),
        (Some("/goal"), Some("resume"))
    )
}

fn is_agent_interrupt(physical_key: PhysicalKey, modifiers: ModifiersState) -> bool {
    matches!(physical_key, PhysicalKey::Code(KeyCode::KeyC))
        && modifiers.control_key()
        && !modifiers.alt_key()
        && !modifiers.super_key()
}

fn pending_agent_state_for_surface(
    requests: &[AgentInteractionRequest],
    surface: SurfaceId,
) -> Option<wmcterm_core::AgentState> {
    let surface = surface.to_string();
    let mut has_request = false;
    for request in requests
        .iter()
        .filter(|request| request.surface_id == surface)
    {
        has_request = true;
        if request.kind.as_str() == IpcAgentRequestKind::PERMISSION {
            return Some(wmcterm_core::AgentState::WaitingForPermission);
        }
    }
    has_request.then_some(wmcterm_core::AgentState::WaitingForInput)
}

fn effective_agent_state(
    pending: Option<wmcterm_core::AgentState>,
    session: Option<wmcterm_core::AgentState>,
    current: Option<wmcterm_core::AgentState>,
) -> Option<wmcterm_core::AgentState> {
    pending.or(session).or_else(|| {
        current.map(|state| match state {
            wmcterm_core::AgentState::WaitingForInput
            | wmcterm_core::AgentState::WaitingForPermission => wmcterm_core::AgentState::Running,
            state => state,
        })
    })
}

fn agent_state_attention(state: wmcterm_core::AgentState) -> AttentionState {
    match state {
        wmcterm_core::AgentState::WaitingForInput
        | wmcterm_core::AgentState::WaitingForPermission => AttentionState::NeedsInput,
        wmcterm_core::AgentState::Error => AttentionState::Error,
        wmcterm_core::AgentState::Completed => AttentionState::Activity,
        wmcterm_core::AgentState::Unknown
        | wmcterm_core::AgentState::Idle
        | wmcterm_core::AgentState::Running => AttentionState::None,
    }
}

fn attention_priority(attention: AttentionState) -> u8 {
    match attention {
        AttentionState::None => 0,
        AttentionState::Activity => 1,
        AttentionState::NeedsInput => 2,
        AttentionState::Error => 3,
    }
}

fn workspace_status_for_agent_priority(priority: u8) -> wmcterm_core::WorkspaceStatus {
    match priority {
        5 => wmcterm_core::WorkspaceStatus::Error,
        4 => wmcterm_core::WorkspaceStatus::NeedsInput,
        3 => wmcterm_core::WorkspaceStatus::Running,
        2 => wmcterm_core::WorkspaceStatus::Completed,
        _ => wmcterm_core::WorkspaceStatus::Idle,
    }
}

fn aggregate_agent_priority(states: impl IntoIterator<Item = wmcterm_core::AgentState>) -> u8 {
    states
        .into_iter()
        .map(agent_state_priority)
        .max()
        .unwrap_or(0)
}

fn unread_badge_label(count: usize) -> Option<String> {
    match count {
        0 => None,
        1..=99 => Some(count.to_string()),
        _ => Some("99+".to_owned()),
    }
}

fn workspace_sidebar_row_height(has_agent: bool, has_git_branch: bool) -> f32 {
    48.0 + if has_agent { 19.0 } else { 0.0 } + if has_git_branch { 19.0 } else { 0.0 }
}

fn layout(node: &PaneNode, rect: Rect, output: &mut Vec<(PaneId, Rect)>) {
    match node {
        PaneNode::Leaf { pane } => output.push((*pane, rect)),
        PaneNode::Split {
            axis,
            ratio,
            first,
            second,
        } => {
            let ratio = ratio.clamp(0.1, 0.9);
            let (a, b) = split_rects(*axis, ratio, rect);
            layout(first, a, output);
            layout(second, b, output);
        }
    }
}

fn empty_pane_button(rect: Rect) -> ui::Bounds {
    let width = 210.0_f32.min((rect.width - 32.0).max(1.0));
    ui::Bounds {
        x: rect.x + (rect.width - width) / 2.0,
        y: rect.y + (rect.height - 36.0) / 2.0 + 18.0,
        width,
        height: 36.0,
    }
}

fn ordered_cells(a: (usize, usize), b: (usize, usize)) -> ((usize, usize), (usize, usize)) {
    if (a.1, a.0) <= (b.1, b.0) {
        (a, b)
    } else {
        (b, a)
    }
}

fn snapshot_selection_text(
    snapshot: &ScreenSnapshot,
    start: (usize, usize),
    end: (usize, usize),
) -> Option<String> {
    let (start, end) = ordered_cells(start, end);
    let first_row = start.1.checked_sub(snapshot.screen_row_origin)?;
    let last_row = end.1.checked_sub(snapshot.screen_row_origin)?;
    if first_row >= snapshot.cells.len() || last_row >= snapshot.cells.len() {
        return None;
    }
    let mut lines = Vec::with_capacity(last_row - first_row + 1);
    for row_index in first_row..=last_row {
        let row = &snapshot.cells[row_index];
        let first_column = if row_index == first_row { start.0 } else { 0 };
        let last_column = if row_index == last_row {
            end.0
        } else {
            row.len().saturating_sub(1)
        };
        let mut line = String::new();
        if first_column <= last_column {
            for cell in row.iter().take(last_column + 1).skip(first_column) {
                line.push_str(&cell.text);
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    Some(lines.join("\n"))
}

fn divider_at(
    node: &PaneNode,
    rect: Rect,
    x: f32,
    y: f32,
    path: &mut Vec<bool>,
) -> Option<SplitDrag> {
    let PaneNode::Split {
        axis,
        ratio,
        first,
        second,
    } = node
    else {
        return None;
    };
    let center = match axis {
        SplitAxis::Vertical => rect.x + rect.width * ratio,
        SplitAxis::Horizontal => rect.y + rect.height * ratio,
    };
    let hit = match axis {
        SplitAxis::Vertical => {
            (x - center).abs() <= 6.0 && y >= rect.y && y <= rect.y + rect.height
        }
        SplitAxis::Horizontal => {
            (y - center).abs() <= 6.0 && x >= rect.x && x <= rect.x + rect.width
        }
    };
    if hit {
        return Some(SplitDrag {
            path: path.clone(),
            axis: *axis,
            container: rect,
            center,
        });
    }
    let ratio = ratio.clamp(0.1, 0.9);
    let (a, b) = split_rects(*axis, ratio, rect);
    path.push(false);
    if let Some(found) = divider_at(first, a, x, y, path) {
        path.pop();
        return Some(found);
    }
    path.pop();
    path.push(true);
    let found = divider_at(second, b, x, y, path);
    path.pop();
    found
}

fn collect_dividers(
    node: &PaneNode,
    rect: Rect,
    path: &mut Vec<bool>,
    output: &mut Vec<SplitDrag>,
) {
    let PaneNode::Split {
        axis,
        ratio,
        first,
        second,
    } = node
    else {
        return;
    };
    let ratio = ratio.clamp(0.1, 0.9);
    let center = match axis {
        SplitAxis::Vertical => rect.x + rect.width * ratio,
        SplitAxis::Horizontal => rect.y + rect.height * ratio,
    };
    output.push(SplitDrag {
        path: path.clone(),
        axis: *axis,
        container: rect,
        center,
    });
    let (first_rect, second_rect) = split_rects(*axis, ratio, rect);
    path.push(false);
    collect_dividers(first, first_rect, path, output);
    path.pop();
    path.push(true);
    collect_dividers(second, second_rect, path, output);
    path.pop();
}

fn divider_line(divider: &SplitDrag, thickness: f32) -> Rect {
    let thickness = thickness.max(1.0);
    match divider.axis {
        SplitAxis::Vertical => Rect {
            x: divider.center - thickness / 2.0,
            y: divider.container.y,
            width: thickness,
            height: divider.container.height,
        },
        SplitAxis::Horizontal => Rect {
            x: divider.container.x,
            y: divider.center - thickness / 2.0,
            width: divider.container.width,
            height: thickness,
        },
    }
}

fn split_rects(axis: SplitAxis, ratio: f32, rect: Rect) -> (Rect, Rect) {
    match axis {
        SplitAxis::Vertical => (
            Rect {
                width: rect.width * ratio - 3.0,
                ..rect
            },
            Rect {
                x: rect.x + rect.width * ratio + 3.0,
                width: rect.width * (1.0 - ratio) - 3.0,
                ..rect
            },
        ),
        SplitAxis::Horizontal => (
            Rect {
                height: rect.height * ratio - 3.0,
                ..rect
            },
            Rect {
                y: rect.y + rect.height * ratio + 3.0,
                height: rect.height * (1.0 - ratio) - 3.0,
                ..rect
            },
        ),
    }
}

fn set_split_ratio(node: &mut PaneNode, path: &[bool], ratio: f32) {
    if path.is_empty() {
        if let PaneNode::Split { ratio: current, .. } = node {
            *current = ratio;
        }
        return;
    }
    if let PaneNode::Split { first, second, .. } = node {
        set_split_ratio(if path[0] { second } else { first }, &path[1..], ratio);
    }
}

fn terminal_workspace_area(window_width: f32, window_height: f32, sidebar_width: f32) -> Rect {
    const TITLEBAR_HEIGHT: f32 = 40.0;
    const BOTTOM_MARGIN: f32 = 4.0;
    Rect {
        x: sidebar_width,
        y: TITLEBAR_HEIGHT,
        width: (window_width - sidebar_width).max(1.0),
        height: (window_height - TITLEBAR_HEIGHT - BOTTOM_MARGIN).max(1.0),
    }
}

fn terminal_content_contains(rect: Rect, x: f32, y: f32) -> bool {
    const CONTENT_LEFT_INSET: f32 = 8.0;
    const TAB_BAR_HEIGHT: f32 = 25.0;
    x >= rect.x + CONTENT_LEFT_INSET
        && x < rect.x + rect.width
        && y >= rect.y + TAB_BAR_HEIGHT
        && y < rect.y + rect.height
}

fn selection_autoscroll_lines(
    pointer_y: f32,
    content_top: f32,
    content_bottom: f32,
    cell_height: f32,
) -> Option<isize> {
    const MAX_LINES_PER_TICK: isize = 8;
    let distance = if pointer_y < content_top {
        content_top - pointer_y
    } else if pointer_y >= content_bottom {
        pointer_y - content_bottom
    } else {
        return None;
    };
    let magnitude = (distance / cell_height.max(1.0)).floor() as isize + 1;
    Some(if pointer_y < content_top {
        -magnitude.clamp(1, MAX_LINES_PER_TICK)
    } else {
        magnitude.clamp(1, MAX_LINES_PER_TICK)
    })
}

fn selection_redraw_due(previous: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    frame_rebuild_due(previous, now)
}

fn frame_rebuild_due(previous: Option<std::time::Instant>, now: std::time::Instant) -> bool {
    frame_rebuild_due_with_interval(previous, now, FRAME_INTERVAL)
}

fn frame_rebuild_due_with_interval(
    previous: Option<std::time::Instant>,
    now: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    previous.is_none_or(|previous| now.saturating_duration_since(previous) >= interval)
}

fn adaptive_frame_interval(work: std::time::Duration) -> std::time::Duration {
    work.saturating_mul(2)
        .clamp(FRAME_INTERVAL, MAX_FRAME_INTERVAL)
}

fn smooth_frame_work(
    previous: std::time::Duration,
    observed: std::time::Duration,
) -> std::time::Duration {
    if previous.is_zero() {
        observed
    } else {
        previous.saturating_mul(3) / 4 + observed / 4
    }
}

fn accumulate_pixel_scroll(remainder: &mut f64, delta: f64, cell_height: f64) -> isize {
    *remainder += delta;
    let lines = (*remainder / cell_height.max(1.0)).trunc() as isize;
    *remainder -= lines as f64 * cell_height.max(1.0);
    lines
}

fn advance_cursor_blink(
    visible: &mut bool,
    last_toggle: &mut std::time::Instant,
    now: std::time::Instant,
) -> bool {
    let elapsed = now.saturating_duration_since(*last_toggle);
    if elapsed < CURSOR_BLINK_INTERVAL {
        return false;
    }
    let intervals = (elapsed.as_millis() / CURSOR_BLINK_INTERVAL.as_millis()).max(1);
    if intervals % 2 == 1 {
        *visible = !*visible;
    }
    *last_toggle += CURSOR_BLINK_INTERVAL * intervals as u32;
    intervals % 2 == 1
}

fn notification_arrives_read(
    window_focused: bool,
    active_terminal: Option<TerminalId>,
    terminal: TerminalId,
) -> bool {
    window_focused && active_terminal == Some(terminal)
}

fn approval_needs_native_notification(
    notifications_enabled: bool,
    window_focused: bool,
    active_surface: Option<SurfaceId>,
    target_surface: SurfaceId,
) -> bool {
    notifications_enabled && (!window_focused || active_surface != Some(target_surface))
}

fn approval_should_reveal_panel(
    text_editor_active: bool,
    active_surface: Option<SurfaceId>,
    request_surface: SurfaceId,
) -> bool {
    !text_editor_active && active_surface == Some(request_surface)
}

fn mark_surface_notifications_read(notifications: &mut [Notification], surface: SurfaceId) -> bool {
    let mut changed = false;
    for notification in notifications
        .iter_mut()
        .filter(|notification| notification.surface == surface && !notification.read)
    {
        notification.read = true;
        changed = true;
    }
    changed
}

fn spawn_event_bridge(
    terminal: TerminalId,
    events: flume::Receiver<SessionEvent>,
    pending: Arc<Mutex<VecDeque<TaggedEvent>>>,
    wake_pending: Arc<AtomicBool>,
    proxy: EventLoopProxy<UserEvent>,
) {
    thread::Builder::new()
        .name(format!("wmcterm-event-{terminal}"))
        .spawn(move || {
            while let Ok(event) = events.recv() {
                let mut queue = pending.lock().expect("PTY event queue poisoned");
                enqueue_terminal_event(&mut queue, terminal, event);
                drop(queue);
                if !wake_pending.swap(true, Ordering::AcqRel) {
                    let _ = proxy.send_event(UserEvent::TerminalWake);
                }
            }
        })
        .expect("failed to spawn terminal event bridge");
}

fn enqueue_terminal_event(
    queue: &mut VecDeque<TaggedEvent>,
    terminal: TerminalId,
    event: SessionEvent,
) {
    if matches!(&event, SessionEvent::Snapshot(_))
        && let Some(previous) = queue
            .iter_mut()
            .rev()
            .find(|previous| previous.terminal == terminal)
        && matches!(&previous.event, SessionEvent::Snapshot(_))
    {
        previous.event = event;
    } else {
        queue.push_back(TaggedEvent { terminal, event });
    }
}

fn effective_agent_lifecycle(
    runtime_lifecycle: &str,
    provided_lifecycle: Option<&str>,
) -> Result<String, String> {
    if provided_lifecycle.is_some_and(|provided| provided != runtime_lifecycle) {
        return Err("stale agent event for a previous terminal lifecycle".into());
    }
    Ok(provided_lifecycle.unwrap_or(runtime_lifecycle).to_owned())
}

fn agent_session_event_kind(
    event: &AgentEvent,
    request_suffix: &str,
    run_mode: AgentRunMode,
) -> Option<AgentSessionEventKind> {
    match event {
        AgentEvent::SessionStart { .. } => None,
        AgentEvent::ToolUse { tool } => {
            Some(AgentSessionEventKind::Telemetry(AgentTelemetry::Activity {
                summary: Some(tool.clone()),
            }))
        }
        // Hook and OSC permission events are notifications that the provider's
        // own UI is waiting. They have no reply channel, so exposing them as
        // actionable requests would create approval buttons whose decisions
        // can never reach the provider. Real Claude and Codex approvals enter
        // through `agent.approval.register` instead.
        AgentEvent::PermissionRequest { .. } => Some(AgentSessionEventKind::Telemetry(
            AgentTelemetry::StateObserved {
                // `--approve-for-me` resolves this request inside Codex's
                // automatic reviewer, while full-access mode bypasses the
                // approval flow entirely. The hook has no matching resolved
                // event, so projecting either mode as a human wait would
                // leave the workspace stuck until the turn's final Stop hook.
                state: if run_mode != AgentRunMode::Interactive {
                    wmcterm_core::AgentState::Running
                } else {
                    wmcterm_core::AgentState::WaitingForPermission
                },
            },
        )),
        AgentEvent::NeedsInput { message } => {
            Some(AgentSessionEventKind::ActionRequired(AgentActionRequest {
                request_id: format!("input:{request_suffix}"),
                kind: AgentRequestKind::Input,
                message: message.clone(),
                choices: Vec::new(),
            }))
        }
        AgentEvent::TurnComplete { .. } => Some(AgentSessionEventKind::Telemetry(
            AgentTelemetry::StateObserved {
                state: wmcterm_core::AgentState::Idle,
            },
        )),
        // Hook Stop means the current turn has returned to the interactive
        // prompt; it does not mean that the long-lived Agent process exited.
        // The PTY exit path owns the actual lifecycle transition.
        AgentEvent::Stop { success, .. } => Some(AgentSessionEventKind::Telemetry(
            AgentTelemetry::StateObserved {
                state: if *success {
                    wmcterm_core::AgentState::Completed
                } else {
                    wmcterm_core::AgentState::Error
                },
            },
        )),
    }
}

fn agent_transition_for_run_mode(
    event: &AgentEvent,
    run_mode: AgentRunMode,
) -> wmcterm_agent::AgentTransition {
    if run_mode != AgentRunMode::Interactive
        && matches!(event, AgentEvent::PermissionRequest { .. })
    {
        return wmcterm_agent::AgentTransition {
            state: wmcterm_core::AgentState::Running,
            attention: AttentionState::None,
            notification: None,
        };
    }
    reduce_agent(event)
}

struct PersistedAgentResume {
    agent: AgentResumeInfo,
    managed: bool,
}

fn resume_info(
    state: &AppState,
    sessions: &HashMap<SurfaceId, AgentSession>,
    foreground_agents: &HashMap<TerminalId, (AgentKind, AgentRunMode)>,
    terminal: TerminalId,
) -> Option<PersistedAgentResume> {
    let (surface_id, metadata) = state
        .surfaces
        .iter()
        .find(|(_, surface)| matches!(surface.kind, SurfaceKind::Terminal(id) if id == terminal))
        .and_then(|(surface_id, surface)| {
            surface
                .agent
                .as_ref()
                .map(|metadata| (*surface_id, metadata))
        })?;
    let session = sessions.get(&surface_id)?;
    let managed = agent_launch_is_managed(session.launch_command.as_deref(), &metadata.name);
    let foreground_matches = foreground_agents
        .get(&terminal)
        .zip(metadata.name.parse::<AgentKind>().ok())
        .is_some_and(|((foreground, _), observed)| *foreground == observed);
    if !should_persist_agent_resume(managed, session.lifecycle.phase, foreground_matches) {
        return None;
    }
    let session_id = metadata.session_id.clone()?;
    let agent = match metadata.name.to_ascii_lowercase().as_str() {
        "claude" | "claude code" => Some(AgentResumeInfo::Claude { session_id }),
        "codex" => Some(AgentResumeInfo::Codex { session_id }),
        "opencode" => Some(AgentResumeInfo::OpenCode { session_id }),
        _ => None,
    }?;
    Some(PersistedAgentResume { agent, managed })
}

fn should_persist_agent_resume(
    managed: bool,
    phase: AgentLifecyclePhase,
    foreground_matches: bool,
) -> bool {
    managed || (phase == AgentLifecyclePhase::Running && foreground_matches)
}

fn agent_launch_is_managed(launch_command: Option<&str>, observed_agent: &str) -> bool {
    launch_command
        .filter(|command| !command.contains(MANUAL_AGENT_RESUME_MARKER))
        .and_then(AgentKind::from_process_command)
        .zip(observed_agent.parse::<AgentKind>().ok())
        .is_some_and(|(launched, observed)| launched == observed)
}

const MANUAL_AGENT_RESUME_MARKER: &str = "WMCTERM_MANUAL_AGENT_RESUME";

fn is_manual_agent_resume_command(command: &str) -> bool {
    command.contains(MANUAL_AGENT_RESUME_MARKER)
}

fn manual_agent_resume_command(resume: &str) -> String {
    #[cfg(windows)]
    {
        let shell = env::var_os("SHELL").unwrap_or_else(|| "powershell.exe".into());
        let name = std::path::Path::new(&shell)
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let shell = shell.to_string_lossy();
        match name.as_str() {
            "powershell" | "pwsh" => format!(
                "$env:{MANUAL_AGENT_RESUME_MARKER}='1'; {resume}; Remove-Item Env:{MANUAL_AGENT_RESUME_MARKER} -ErrorAction SilentlyContinue; & {} -NoLogo",
                shell_quote_command(&shell)
            ),
            "cmd" => format!(
                "set {MANUAL_AGENT_RESUME_MARKER}=1 && {resume} & set {MANUAL_AGENT_RESUME_MARKER}= & \"{shell}\" /D"
            ),
            _ => format!(
                "{MANUAL_AGENT_RESUME_MARKER}=1 {resume}; exec {} -l",
                shell_quote_command(&shell)
            ),
        }
    }
    #[cfg(not(windows))]
    {
        let shell = env::var_os("SHELL").unwrap_or_else(|| "sh".into());
        format!(
            "{MANUAL_AGENT_RESUME_MARKER}=1 {resume}; exec {} -l",
            shell_quote_command(&shell.to_string_lossy())
        )
    }
}

fn show_native_notification(title: &str, body: &str) {
    let title = title.to_owned();
    let body = body.to_owned();
    let _ = thread::Builder::new()
        .name("wmcterm-notification".into())
        .spawn(move || {
            #[cfg(target_os = "linux")]
            let _ = ProcessCommand::new("notify-send")
                .args(["--app-name=wmcterm", &title, &body])
                .status();
            #[cfg(target_os = "macos")]
            let _ = ProcessCommand::new("osascript")
                .args([
                    "-e",
                    &format!("display notification {:?} with title {:?}", body, title),
                ])
                .status();
            #[cfg(target_os = "windows")]
            let _ = (title, body);
        });
}

#[cfg(target_os = "linux")]
fn listening_ports_for(cwd: &std::path::Path) -> Vec<u16> {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut socket_inodes = std::collections::HashSet::new();
    for process in processes.flatten() {
        if !process
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|character| character.is_ascii_digit())
        {
            continue;
        }
        let process_path = process.path();
        let Ok(process_cwd) = std::fs::read_link(process_path.join("cwd")) else {
            continue;
        };
        if !process_cwd.starts_with(cwd) {
            continue;
        }
        let Ok(descriptors) = std::fs::read_dir(process_path.join("fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            let Ok(target) = std::fs::read_link(descriptor.path()) else {
                continue;
            };
            let text = target.to_string_lossy();
            if let Some(inode) = text
                .strip_prefix("socket:[")
                .and_then(|value| value.strip_suffix(']'))
            {
                socket_inodes.insert(inode.to_owned());
            }
        }
    }
    let mut ports = std::collections::HashSet::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(contents) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in contents.lines().skip(1) {
            let columns: Vec<_> = line.split_whitespace().collect();
            if columns.len() < 10 || columns[3] != "0A" || !socket_inodes.contains(columns[9]) {
                continue;
            }
            if let Some(port) = columns[1]
                .split(':')
                .nth(1)
                .and_then(|value| u16::from_str_radix(value, 16).ok())
            {
                ports.insert(port);
            }
        }
    }
    let mut ports: Vec<_> = ports.into_iter().collect();
    ports.sort_unstable();
    ports
}

#[cfg(not(target_os = "linux"))]
fn listening_ports_for(_cwd: &std::path::Path) -> Vec<u16> {
    Vec::new()
}

fn should_notify_terminal_bell(
    mode: BellNotificationMode,
    window_focused: bool,
    terminal_active: bool,
    since_last: Option<std::time::Duration>,
    debounce: std::time::Duration,
) -> bool {
    let allowed_by_context = match mode {
        BellNotificationMode::Never => false,
        BellNotificationMode::Background => !window_focused || !terminal_active,
        BellNotificationMode::Always => true,
    };
    allowed_by_context && since_last.is_none_or(|elapsed| elapsed >= debounce)
}

fn unix_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn required_string<'a>(params: &'a Value, field: &str) -> Result<&'a str, String> {
    params
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing or empty params.{field}"))
}

fn required_string_object<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<&'a str, String> {
    object
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("missing or empty request.{field}"))
}

fn validate_agent_approval_protocol(source: &str, method: &str, kind: &str) -> Result<(), String> {
    let valid = match source {
        "codex_app_server" => matches!(
            (method, kind),
            ("item/commandExecution/requestApproval", "command_execution")
                | ("item/commandExecution/requestApproval", "write_stdin")
                | ("item/fileChange/requestApproval", "file_change")
                | ("item/permissions/requestApproval", "permissions")
        ),
        "claude_hook" => method == "PermissionRequest" && kind == "permission",
        _ => false,
    };
    valid
        .then_some(())
        .ok_or_else(|| format!("unsupported {source} approval method/kind `{method}`/`{kind}`"))
}

fn agent_approval_message(
    language: Language,
    source: &str,
    kind: &str,
    method: &str,
    params: &Value,
) -> String {
    let chinese = language.is_chinese();
    let agent = match source {
        "codex_app_server" => "Codex",
        "claude_hook" => "Claude",
        _ => "Agent",
    };
    let mut lines = Vec::new();
    match (source, kind) {
        ("codex_app_server", "command_execution") => lines.push(if chinese {
            format!("{agent} 请求执行命令")
        } else {
            format!("{agent} requests to run a command")
        }),
        ("codex_app_server", "write_stdin") => lines.push(if chinese {
            format!("{agent} 请求向运行中的命令发送输入")
        } else {
            format!("{agent} requests to send input to a running command")
        }),
        ("codex_app_server", "file_change") => lines.push(if chinese {
            format!("{agent} 请求修改文件")
        } else {
            format!("{agent} requests to change files")
        }),
        ("codex_app_server", "permissions") => lines.push(if chinese {
            format!("{agent} 请求额外权限")
        } else {
            format!("{agent} requests additional permissions")
        }),
        ("claude_hook", "permission") => {
            let tool =
                approval_text_field(params, &["tool_name", "toolName"]).unwrap_or("unknown tool");
            lines.push(if chinese {
                format!("{agent} 请求使用工具：{tool}")
            } else {
                format!("{agent} requests to use tool: {tool}")
            });
        }
        _ => lines.push(if chinese {
            format!("{agent} 请求授权")
        } else {
            format!("{agent} requests approval")
        }),
    }

    let tool_input = params.get("tool_input").or_else(|| params.get("toolInput"));
    let command = approval_text_field(params, &["command", "cmd"])
        .or_else(|| tool_input.and_then(|input| approval_text_field(input, &["command", "cmd"])));
    if let Some(command) = command {
        lines.push(format!(
            "{}: {command}",
            if chinese { "命令" } else { "Command" }
        ));
    }
    if kind == "write_stdin" {
        if let Some(stdin) = approval_text_field(params, &["stdin", "input"]) {
            lines.push(format!(
                "{}: {stdin}",
                if chinese { "输入" } else { "Input" }
            ));
        }
        if let Some(process_id) = approval_text_field(params, &["processId", "process_id"]) {
            lines.push(format!(
                "{}: {process_id}",
                if chinese { "进程" } else { "Process" }
            ));
        }
    }
    if let Some(cwd) =
        approval_text_field(params, &["cwd", "working_directory", "workingDirectory"])
    {
        lines.push(format!(
            "{}: {cwd}",
            if chinese {
                "工作目录"
            } else {
                "Working directory"
            }
        ));
    }
    if let Some(reason) = approval_text_field(params, &["reason", "justification", "description"]) {
        lines.push(format!(
            "{}: {reason}",
            if chinese { "原因" } else { "Reason" }
        ));
    }
    if kind == "permissions"
        && let Some(permissions) = params.get("permissions").and_then(Value::as_object)
        && !permissions.is_empty()
    {
        lines.push(format!(
            "{}: {}",
            if chinese { "权限" } else { "Permissions" },
            permissions.keys().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    let technical = serde_json::to_string_pretty(params).unwrap_or_else(|_| params.to_string());
    lines.push(String::new());
    lines.push(if chinese {
        format!("技术详情（{method}）：")
    } else {
        format!("Technical details ({method}):")
    });
    lines.push(technical);
    lines.join("\n")
}

fn approval_text_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|name| {
        value
            .get(*name)
            .and_then(Value::as_str)
            .filter(|text| !text.trim().is_empty())
    })
}

fn claude_approval_binding_matches(
    runtime_lifecycle: &str,
    metadata_agent: Option<&str>,
    authoritative_agent: Option<&str>,
    requested_lifecycle: &str,
) -> bool {
    let is_claude =
        |name: &str| matches!(name.to_ascii_lowercase().as_str(), "claude" | "claude code");
    let is_unknown = |name: &str| name.is_empty() || name.eq_ignore_ascii_case("agent");
    runtime_lifecycle == requested_lifecycle
        && metadata_agent.is_none_or(|agent| is_unknown(agent) || is_claude(agent))
        && authoritative_agent.is_none_or(|agent| is_unknown(agent) || is_claude(agent))
}

fn apply_claude_subagent_status(
    statuses: &mut HashMap<String, ClaudeSubagentStatus>,
    surface: SurfaceId,
    hook: &ClaudeHookEvent,
) {
    let route_key = format!("{surface}:{}", hook.route_key());
    match hook.kind {
        ClaudeHookKind::SubagentStart => {
            statuses.insert(
                route_key,
                ClaudeSubagentStatus {
                    surface,
                    session_id: hook.session_id.clone(),
                },
            );
        }
        ClaudeHookKind::SubagentStop => {
            statuses.remove(&route_key);
        }
        ClaudeHookKind::SessionEnd => {
            statuses.retain(|_, subagent| {
                subagent.surface != surface || subagent.session_id != hook.session_id
            });
        }
        ClaudeHookKind::SessionStart
        | ClaudeHookKind::PreToolUse
        | ClaudeHookKind::PostToolUse
        | ClaudeHookKind::PostToolUseFailure
        | ClaudeHookKind::PermissionRequest
        | ClaudeHookKind::Notification
        | ClaudeHookKind::Stop => {}
    }
}

fn apply_codex_subagent_status(
    statuses: &mut CodexSubagentTracker,
    surface: SurfaceId,
    hook: &CodexSubagentHookEvent,
) {
    let key = CodexSubagentKey {
        surface,
        session_id: hook.session_id.clone(),
        agent_id: hook.agent_id.clone(),
    };
    match hook.kind {
        CodexSubagentHookKind::Start => {
            // A recent Stop wins over a delayed Start for the same stable key. Stopped entries
            // are bounded per session below, so an event older than that replay horizon can be
            // accepted again after its tombstone is evicted. Active entries are never evicted.
            statuses
                .states
                .entry(key)
                .or_insert(CodexSubagentState::Active);
        }
        CodexSubagentHookKind::Stop => {
            statuses.states.insert(key, CodexSubagentState::Stopped);
            let session = (surface, hook.session_id.clone());
            let stopped = statuses.stopped_order.entry(session).or_default();
            stopped.retain(|agent_id| agent_id != &hook.agent_id);
            stopped.push_back(hook.agent_id.clone());
            while stopped.len() > MAX_CODEX_STOPPED_TOMBSTONES_PER_SESSION {
                let Some(agent_id) = stopped.pop_front() else {
                    break;
                };
                let expired = CodexSubagentKey {
                    surface,
                    session_id: hook.session_id.clone(),
                    agent_id,
                };
                if statuses.states.get(&expired) == Some(&CodexSubagentState::Stopped) {
                    statuses.states.remove(&expired);
                }
            }
        }
    }
}

fn clear_codex_subagent_session(
    statuses: &mut CodexSubagentTracker,
    surface: SurfaceId,
    session_id: &str,
) {
    statuses
        .states
        .retain(|subagent, _| subagent.surface != surface || subagent.session_id != session_id);
    statuses
        .stopped_order
        .remove(&(surface, session_id.to_owned()));
}

fn clear_codex_subagent_sessions_except(
    statuses: &mut CodexSubagentTracker,
    surface: SurfaceId,
    current_session_id: &str,
) {
    statuses.states.retain(|subagent, _| {
        subagent.surface != surface || subagent.session_id == current_session_id
    });
    statuses
        .stopped_order
        .retain(|(stopped_surface, session_id), _| {
            *stopped_surface != surface || session_id == current_session_id
        });
}

fn clear_codex_subagent_surface(statuses: &mut CodexSubagentTracker, surface: SurfaceId) {
    statuses
        .states
        .retain(|subagent, _| subagent.surface != surface);
    statuses
        .stopped_order
        .retain(|(stopped_surface, _), _| *stopped_surface != surface);
}

fn codex_subagent_count(
    statuses: &CodexSubagentTracker,
    surface: SurfaceId,
    session_id: &str,
) -> usize {
    statuses
        .states
        .iter()
        .filter(|(subagent, state)| {
            **state == CodexSubagentState::Active
                && subagent.surface == surface
                && subagent.session_id == session_id
        })
        .count()
}

fn effective_codex_agent_state(
    parent: wmcterm_core::AgentState,
    has_active_subagent: bool,
) -> wmcterm_core::AgentState {
    if has_active_subagent
        && agent_state_priority(parent) < agent_state_priority(wmcterm_core::AgentState::Running)
    {
        wmcterm_core::AgentState::Running
    } else {
        parent
    }
}

fn agent_approval_mode(action: AgentApprovalAction) -> &'static str {
    match action {
        AgentApprovalAction::Deny => "deny",
        AgentApprovalAction::AllowOnce => "once",
        AgentApprovalAction::AlwaysAllow => "always",
    }
}

fn remember_bounded_approval_result(
    results: &mut HashMap<String, AgentApprovalAction>,
    order: &mut VecDeque<String>,
    request_id: String,
    action: AgentApprovalAction,
    capacity: usize,
) {
    if !results.contains_key(&request_id) {
        order.push_back(request_id.clone());
    }
    results.insert(request_id, action);
    while order.len() > capacity {
        if let Some(expired) = order.pop_front() {
            results.remove(&expired);
        }
    }
}

fn agent_approval_resolved_json(action: AgentApprovalAction) -> Value {
    json!({"status":"resolved","decision":{"mode":agent_approval_mode(action)}})
}

fn completed_approval_action(
    reply: &wmcterm_ipc::AgentInteractionReply,
) -> Option<AgentApprovalAction> {
    let wmcterm_ipc::AgentReplyStatus::Resolved { resolution } = &reply.status else {
        return Some(AgentApprovalAction::Deny);
    };
    match resolution.choice_id.as_deref() {
        Some("deny") => Some(AgentApprovalAction::Deny),
        Some("once") => Some(AgentApprovalAction::AllowOnce),
        Some("always") => Some(AgentApprovalAction::AlwaysAllow),
        Some("all") => Some(AgentApprovalAction::AlwaysAllow),
        _ if resolution.approved == Some(false) => Some(AgentApprovalAction::Deny),
        _ if resolution.approved == Some(true) => Some(AgentApprovalAction::AllowOnce),
        _ => None,
    }
}

fn agent_approval_actions(request: &AgentInteractionRequest) -> Vec<AgentApprovalAction> {
    let mut actions = vec![AgentApprovalAction::Deny, AgentApprovalAction::AllowOnce];
    if request.choices.iter().any(|choice| choice.id == "always") {
        actions.push(AgentApprovalAction::AlwaysAllow);
    }
    actions
}

fn agent_choice_has(choice: &wmcterm_ipc::AgentChoice, needles: &[&str]) -> bool {
    let value = choice.label.to_lowercase();
    needles.iter().any(|needle| value.contains(needle))
}

fn agent_approval_resolution(
    request: &AgentInteractionRequest,
    action: AgentApprovalAction,
) -> AgentResolution {
    let choice = match action {
        AgentApprovalAction::Deny => request
            .choices
            .iter()
            .find(|choice| choice.id == "deny")
            .or_else(|| {
                request
                    .choices
                    .iter()
                    .find(|choice| agent_choice_has(choice, &["deny", "reject", "cancel", "拒绝"]))
            }),
        AgentApprovalAction::AllowOnce => request
            .choices
            .iter()
            .find(|choice| choice.id == "once")
            .or_else(|| {
                request.choices.iter().find(|choice| {
                    choice.id != "always"
                        && agent_choice_has(choice, &["allow", "approve", "once", "允许", "批准"])
                })
            }),
        AgentApprovalAction::AlwaysAllow => request
            .choices
            .iter()
            .find(|choice| choice.id == "always")
            .or_else(|| {
                request.choices.iter().find(|choice| {
                    choice.id == "all"
                        || agent_choice_has(choice, &["always", "permanent", "始终", "永久"])
                })
            }),
    };
    AgentResolution {
        choice_id: choice.map(|choice| choice.id.clone()),
        text: None,
        approved: Some(!matches!(action, AgentApprovalAction::Deny)),
    }
}

fn agent_approval_action_label(language: Language, action: AgentApprovalAction) -> &'static str {
    match action {
        AgentApprovalAction::Deny => language.deny(),
        AgentApprovalAction::AllowOnce => language.allow_once(),
        AgentApprovalAction::AlwaysAllow => language.always_allow(),
    }
}

fn agent_request_detail_layout(
    window_size: PhysicalSize<u32>,
    message: &str,
    scroll: usize,
) -> AgentRequestDetailLayout {
    let width = 620.0_f32.min((window_size.width as f32 - 40.0).max(260.0));
    let height = 420.0_f32.min((window_size.height as f32 - 80.0).max(180.0));
    let panel = ui::Bounds {
        x: (window_size.width as f32 - width) / 2.0,
        y: (window_size.height as f32 - height) / 2.0,
        width,
        height,
    };
    let body = ui::Bounds {
        x: panel.x + 12.0,
        y: panel.y + 42.0,
        width: (panel.width - 24.0).max(1.0),
        height: (panel.height - 54.0).max(1.0),
    };
    let columns = ((body.width - 18.0) / (13.0 * 0.6)).floor().max(1.0) as usize;
    let total_lines = agent_request_detail_lines(message, columns).len();
    let visible_lines = (body.height / 20.0).floor().max(1.0) as usize;
    let max_scroll = total_lines.saturating_sub(visible_lines);
    let scrollbar = ui::list_scrollbar(body, total_lines, visible_lines, scroll);
    AgentRequestDetailLayout {
        panel,
        body,
        total_lines,
        visible_lines,
        max_scroll,
        scrollbar,
    }
}

fn scrollbar_scroll_for_pointer(
    track: ui::Bounds,
    thumb: ui::Bounds,
    max_scroll: usize,
    pointer_y: f32,
    grab_y: f32,
) -> usize {
    let travel = (track.height - thumb.height).max(0.0);
    if max_scroll == 0 || travel <= f32::EPSILON {
        return 0;
    }
    let position = (pointer_y - grab_y - track.y).clamp(0.0, travel);
    (position / travel * max_scroll as f32).round() as usize
}

fn agent_request_detail_lines(text: &str, max_columns: usize) -> Vec<String> {
    if max_columns == 0 {
        return Vec::new();
    }
    let mut lines = Vec::new();
    for source_line in text.split('\n') {
        if source_line.is_empty() {
            lines.push(String::new());
        } else {
            let mut line = String::new();
            let mut width = 0;
            for character in source_line.chars() {
                let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
                if !line.is_empty() && width + character_width > max_columns {
                    lines.push(std::mem::take(&mut line));
                    width = 0;
                }
                line.push(character);
                width += character_width;
            }
            lines.push(line);
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn notification_age_label(now_ms: u64, created_at_ms: u64, chinese: bool) -> String {
    let seconds = now_ms.saturating_sub(created_at_ms) / 1_000;
    match seconds {
        0..=59 => {
            if chinese {
                "刚刚".into()
            } else {
                "now".into()
            }
        }
        60..=3_599 => {
            let minutes = seconds / 60;
            if chinese {
                format!("{minutes} 分钟")
            } else {
                format!("{minutes}m")
            }
        }
        3_600..=86_399 => {
            let hours = seconds / 3_600;
            if chinese {
                format!("{hours} 小时")
            } else {
                format!("{hours}h")
            }
        }
        _ => {
            let days = seconds / 86_400;
            if chinese {
                format!("{days} 天")
            } else {
                format!("{days}d")
            }
        }
    }
}

fn notification_overlay_press(
    visible: bool,
    panel: ui::Bounds,
    x: f32,
    y: f32,
) -> NotificationOverlayPress {
    if !visible {
        NotificationOverlayPress::Inactive
    } else if panel.contains(x, y) {
        NotificationOverlayPress::Inside
    } else {
        NotificationOverlayPress::DismissAndPropagate
    }
}

fn notification_toggle_press(visible: bool, toggle_button_hit: bool) -> bool {
    visible && toggle_button_hit
}

fn titlebar_double_click(
    previous: std::time::Instant,
    previous_position: (f32, f32),
    current: std::time::Instant,
    current_position: (f32, f32),
) -> bool {
    const MAX_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
    const MAX_DISTANCE: f32 = 6.0;
    current.saturating_duration_since(previous) <= MAX_DELAY
        && (current_position.0 - previous_position.0).abs() <= MAX_DISTANCE
        && (current_position.1 - previous_position.1).abs() <= MAX_DISTANCE
}

fn terminal_double_click(previous: std::time::Instant, current: std::time::Instant) -> bool {
    const MAX_DELAY: std::time::Duration = std::time::Duration::from_millis(500);
    current.saturating_duration_since(previous) <= MAX_DELAY
}

fn terminal_target_from_osc8(uri: &str, cwd: &Path) -> Option<TerminalOpenTarget> {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        Some(TerminalOpenTarget::Url(uri.to_owned()))
    } else if let Some(path) = uri.strip_prefix("file://") {
        resolve_terminal_path(path, cwd).map(TerminalOpenTarget::Path)
    } else {
        None
    }
}

fn terminal_target_in_line(
    line: &str,
    clicked_byte: usize,
    cwd: &Path,
) -> Option<TerminalOpenTarget> {
    if clicked_byte > line.len() || !line.is_char_boundary(clicked_byte) {
        return None;
    }
    let start = line[..clicked_byte]
        .char_indices()
        .rev()
        .find_map(|(index, character)| {
            character
                .is_whitespace()
                .then_some(index + character.len_utf8())
        })
        .unwrap_or(0);
    let end = line[clicked_byte..]
        .char_indices()
        .find_map(|(index, character)| character.is_whitespace().then_some(clicked_byte + index))
        .unwrap_or(line.len());
    let token = &line[start..end];
    let trimmed = token.trim_start_matches(|character: char| {
        matches!(character, '"' | '\'' | '<' | '(' | '[' | '{')
    });
    let trimmed = trimmed.trim_end_matches(|character: char| {
        matches!(
            character,
            '"' | '\'' | '>' | ')' | ']' | '}' | '.' | ',' | ';' | '!' | '?'
        )
    });
    let trimmed_start = start + token.find(trimmed)?;
    if clicked_byte < trimmed_start || clicked_byte >= trimmed_start + trimmed.len() {
        return None;
    }
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Some(TerminalOpenTarget::Url(trimmed.to_owned()));
    }
    resolve_terminal_path(trimmed, cwd).map(TerminalOpenTarget::Path)
}

fn resolve_terminal_path(value: &str, cwd: &Path) -> Option<PathBuf> {
    let value = strip_source_position(value);
    let looks_like_path = value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with("~/")
        || value.as_bytes().get(1) == Some(&b':')
        || value.contains('/');
    if !looks_like_path || value.contains("://") {
        return None;
    }
    if let Some(relative) = value.strip_prefix("~/") {
        let home = env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })?;
        return Some(PathBuf::from(home).join(relative));
    }
    let path = PathBuf::from(value);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn strip_source_position(value: &str) -> &str {
    let Some((without_last, last)) = value.rsplit_once(':') else {
        return value;
    };
    if !last.chars().all(|character| character.is_ascii_digit()) {
        return value;
    }
    if let Some((without_both, line)) = without_last.rsplit_once(':')
        && line.chars().all(|character| character.is_ascii_digit())
    {
        without_both
    } else {
        without_last
    }
}

fn open_terminal_target(target: TerminalOpenTarget) {
    thread::spawn(move || {
        let result = match target {
            TerminalOpenTarget::Url(url) => open_url_with_system(&url),
            TerminalOpenTarget::Path(path) => open_path_in_file_manager(&path),
        };
        if let Err(error) = result {
            tracing::warn!(%error, "opening terminal link target failed");
        }
    });
}

fn open_url_with_system(url: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    ProcessCommand::new("xdg-open")
        .arg(url)
        .spawn()
        .context("starting xdg-open")?;
    #[cfg(target_os = "macos")]
    ProcessCommand::new("open")
        .arg(url)
        .spawn()
        .context("starting open")?;
    #[cfg(windows)]
    ProcessCommand::new("rundll32.exe")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .context("starting browser")?;
    Ok(())
}

fn open_path_in_file_manager(path: &Path) -> Result<()> {
    if path.is_dir() {
        return open_directory_in_file_manager(path);
    }
    anyhow::ensure!(path.is_file(), "path does not exist: {}", path.display());
    #[cfg(target_os = "linux")]
    ProcessCommand::new("xdg-open")
        .arg(path.parent().unwrap_or(path))
        .spawn()
        .context("starting xdg-open")?;
    #[cfg(target_os = "macos")]
    ProcessCommand::new("open")
        .arg("-R")
        .arg(path)
        .spawn()
        .context("starting open")?;
    #[cfg(windows)]
    ProcessCommand::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn()
        .context("starting Explorer")?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CellWordClass {
    Whitespace,
    Word,
    Punctuation,
}

fn cell_word_class(cell: &CellSnapshot) -> Option<CellWordClass> {
    if cell.text.is_empty() {
        return None;
    }
    if cell.text.chars().all(char::is_whitespace) {
        Some(CellWordClass::Whitespace)
    } else if cell
        .text
        .chars()
        .any(|character| character.is_alphanumeric() || character == '_')
    {
        Some(CellWordClass::Word)
    } else {
        Some(CellWordClass::Punctuation)
    }
}

/// Returns an inclusive cell range. Empty cells adjacent to a glyph are wide-cell
/// continuations and belong to that glyph; classification never crosses a row.
fn word_cell_bounds(row: &[CellSnapshot], clicked: usize) -> Option<(usize, usize)> {
    if clicked >= row.len() {
        return None;
    }
    let mut owner = clicked;
    while owner > 0 && row[owner].text.is_empty() {
        owner -= 1;
    }
    let class = cell_word_class(&row[owner])?;

    let mut start = owner;
    while start > 0 {
        let mut previous = start - 1;
        while previous > 0 && row[previous].text.is_empty() {
            previous -= 1;
        }
        if cell_word_class(&row[previous]) != Some(class) {
            break;
        }
        start = previous;
    }

    let mut end = owner;
    let mut next = owner + 1;
    while next < row.len() {
        if row[next].text.is_empty() {
            end = next;
            next += 1;
            continue;
        }
        if cell_word_class(&row[next]) != Some(class) {
            break;
        }
        end = next;
        next += 1;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_spawn_proxy_refresh_coalesces_bursts_and_keeps_last_good_on_failure() {
        let refreshed = system_proxy::SystemProxy {
            http: Some("http://desktop:8080".into()),
            ..Default::default()
        };
        let expected = refreshed.clone();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let detector_calls = Arc::clone(&calls);
        let cache =
            SystemProxyCache::with_detector(system_proxy::SystemProxy::default(), move || {
                if detector_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(refreshed.clone())
                } else {
                    Err(())
                }
            });
        *cache
            .last_attempt
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            std::time::Instant::now() - SYSTEM_PROXY_SPAWN_COALESCE_INTERVAL;

        let _ = cache.environment_for_child(&[]);
        assert_eq!(
            *cache
                .current
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            expected
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = cache.environment_for_child(&[]);
        assert_eq!(
            *cache
                .current
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            expected
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "bursty child spawns should share the freshly detected snapshot"
        );
        *cache
            .last_attempt
            .lock()
            .unwrap_or_else(|error| error.into_inner()) =
            std::time::Instant::now() - SYSTEM_PROXY_SPAWN_COALESCE_INTERVAL;
        let _ = cache.environment_for_child(&[]);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            *cache
                .current
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
            expected
        );
    }

    fn terminal_resume(command: Option<&str>, agent: Option<AgentResumeInfo>) -> TerminalResume {
        TerminalResume {
            terminal_id: TerminalId::new_v4(),
            cwd: PathBuf::from("/tmp/project"),
            command: command.map(str::to_owned),
            agent,
            scrollback: None,
        }
    }

    #[test]
    fn codex_resume_requires_a_nonempty_matching_rollout() {
        let home =
            std::env::temp_dir().join(format!("wmcterm-rollout-test-{}", uuid::Uuid::new_v4()));
        let id = uuid::Uuid::new_v4().to_string();
        let folder = home.join("sessions/2026/09/07");
        std::fs::create_dir_all(&folder).unwrap();
        assert!(!codex_rollout_exists(&home, &id));
        let file = folder.join(format!("rollout-2026-09-07T00-00-00-{id}.jsonl"));
        std::fs::write(&file, "").unwrap();
        assert!(!codex_rollout_exists(&home, &id));
        std::fs::write(&file, "{\"type\":\"session_meta\"}\n").unwrap();
        assert!(codex_rollout_exists(&home, &id));
        assert!(!codex_rollout_exists(
            &home,
            &uuid::Uuid::new_v4().to_string()
        ));
        assert!(!codex_rollout_exists(&home, "../invalid"));
        std::fs::create_dir_all(home.join("archived_sessions")).unwrap();
        std::fs::rename(
            &file,
            home.join("archived_sessions")
                .join(file.file_name().unwrap()),
        )
        .unwrap();
        assert!(codex_rollout_exists(&home, &id));
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn missing_codex_record_restores_shell_without_stale_agent_state() {
        for command in [
            None,
            Some(manual_agent_resume_command("codex resume empty-thread")),
            Some("codex resume empty-thread".into()),
        ] {
            let mut app = AppState::new("empty", PathBuf::from("/tmp"));
            let surface = app.surfaces.values_mut().next().unwrap();
            let SurfaceKind::Terminal(terminal_id) = surface.kind else {
                panic!("terminal")
            };
            let surface_id = surface.id;
            surface.agent = Some(wmcterm_core::AgentMetadata {
                name: "codex".into(),
                session_id: Some("empty-thread".into()),
                state: AgentState::Running,
                run_mode: AgentRunMode::Unattended,
            });
            surface.attention = AttentionState::Activity;
            app.workspaces[0].status = wmcterm_core::WorkspaceStatus::NeedsInput;
            let mut terminal = terminal_resume(
                command.as_deref(),
                Some(AgentResumeInfo::Codex {
                    session_id: "empty-thread".into(),
                }),
            );
            terminal.terminal_id = terminal_id;
            terminal.scrollback = Some("previous terminal output".into());
            let mut session = Session {
                version: SESSION_VERSION,
                app,
                terminals: vec![terminal],
                agent_sessions: vec![AgentSession::new(surface_id)],
                window: None,
            };
            discard_codex_resumes_without_record(&mut session, |_| true);
            assert!(session.terminals[0].agent.is_some());
            let mut explicit = session.clone();
            explicit.terminals[0].command = Some("codex --approve-for-me".into());
            discard_codex_resumes_without_record(&mut explicit, |_| false);
            assert_eq!(
                explicit.terminals[0].command.as_deref(),
                Some("codex --approve-for-me")
            );
            assert!(explicit.app.surfaces[&surface_id].agent.is_some());
            let mut unbound = session.clone();
            unbound.terminals[0].agent = None;
            unbound.terminals[0].command = None;
            unbound
                .app
                .surfaces
                .get_mut(&surface_id)
                .unwrap()
                .agent
                .as_mut()
                .unwrap()
                .session_id = None;
            discard_codex_resumes_without_record(&mut unbound, |_| true);
            assert!(unbound.app.surfaces[&surface_id].agent.is_none());
            assert!(unbound.agent_sessions.is_empty());
            discard_codex_resumes_without_record(&mut session, |_| false);
            assert!(session.terminals[0].agent.is_none());
            assert!(session.terminals[0].command.is_none());
            assert_eq!(
                session.terminals[0].scrollback.as_deref(),
                Some("previous terminal output")
            );
            assert!(session.app.surfaces[&surface_id].agent.is_none());
            assert_eq!(
                session.app.surfaces[&surface_id].attention,
                AttentionState::None
            );
            assert_eq!(
                session.app.workspaces[0].status,
                wmcterm_core::WorkspaceStatus::Idle
            );
            assert!(session.agent_sessions.is_empty());
        }
    }

    #[test]
    fn restored_managed_and_manual_codex_drop_stale_modes_without_losing_session_state() {
        for mode in [AgentRunMode::Unattended, AgentRunMode::FullAccess] {
            for command in [
                None,
                Some("codex resume manual-thread".to_owned()),
                Some(manual_agent_resume_command("codex resume manual-thread")),
            ] {
                let mut state = AppState::new("restore", PathBuf::from("/tmp"));
                let surface = state.surfaces.values_mut().next().unwrap();
                let SurfaceKind::Terminal(terminal_id) = surface.kind else {
                    panic!("terminal")
                };
                surface.agent = Some(wmcterm_core::AgentMetadata {
                    name: "codex".into(),
                    state: AgentState::Running,
                    session_id: Some("manual-thread".into()),
                    run_mode: mode,
                });
                let surface_id = surface.id;
                let mut resume = terminal_resume(
                    command.as_deref(),
                    Some(AgentResumeInfo::Codex {
                        session_id: "manual-thread".into(),
                    }),
                );
                resume.terminal_id = terminal_id;
                reconcile_restored_agent_modes(&mut state, &[resume]);
                let metadata = state.surfaces[&surface_id].agent.as_ref().unwrap();
                assert_eq!(metadata.run_mode, AgentRunMode::Interactive);
                assert_eq!(metadata.session_id.as_deref(), Some("manual-thread"));
                assert_eq!(metadata.state, AgentState::Running);
            }
        }
    }

    #[test]
    fn restoring_explicit_agent_launch_preserves_its_mode() {
        let mut state = AppState::new("restore", PathBuf::from("/tmp"));
        let surface = state.surfaces.values_mut().next().unwrap();
        let SurfaceKind::Terminal(terminal_id) = surface.kind else {
            panic!("terminal")
        };
        surface.agent = Some(detected_foreground_agent_metadata(
            AgentKind::Codex,
            AgentRunMode::Unattended,
        ));
        let surface_id = surface.id;
        let mut resume = terminal_resume(
            Some("codex --approve-for-me"),
            Some(AgentResumeInfo::Codex {
                session_id: "manual-thread".into(),
            }),
        );
        resume.terminal_id = terminal_id;
        reconcile_restored_agent_modes(&mut state, &[resume]);
        assert_eq!(
            state.surfaces[&surface_id].agent.as_ref().unwrap().run_mode,
            AgentRunMode::Unattended
        );
    }

    #[test]
    fn detected_mode_updates_preserve_hook_task_state_and_session_identity() {
        let mut metadata = wmcterm_core::AgentMetadata {
            name: "codex".into(),
            state: AgentState::WaitingForPermission,
            session_id: Some("thread".into()),
            run_mode: AgentRunMode::Unattended,
        };
        assert!(sync_detected_agent_mode(
            &mut metadata,
            AgentKind::Codex,
            AgentRunMode::Interactive
        ));
        assert_eq!(metadata.state, AgentState::WaitingForPermission);
        assert_eq!(metadata.session_id.as_deref(), Some("thread"));
        assert!(!sync_detected_agent_mode(
            &mut metadata,
            AgentKind::Claude,
            AgentRunMode::FullAccess
        ));
        assert!(!sync_detected_agent_mode(
            &mut metadata,
            AgentKind::Codex,
            AgentRunMode::Interactive
        ));
        assert!(sync_detected_agent_mode(
            &mut metadata,
            AgentKind::Codex,
            AgentRunMode::Unattended
        ));
    }

    #[test]
    fn restored_codex_teams_child_degrades_to_standalone_resume() {
        let terminal = terminal_resume(
            Some("/opt/codex resume --remote ws://127.0.0.1:44459 child-thread"),
            Some(AgentResumeInfo::Codex {
                session_id: "child-thread".into(),
            }),
        );
        assert_eq!(
            restored_terminal_command(&terminal).as_deref(),
            Some("codex resume child-thread")
        );
    }

    #[test]
    fn legacy_direct_manual_resume_returns_to_a_shell_after_agent_exit() {
        let terminal = terminal_resume(
            Some("codex resume manual-thread"),
            Some(AgentResumeInfo::Codex {
                session_id: "manual-thread".into(),
            }),
        );
        let command = restored_terminal_command(&terminal).unwrap();
        assert!(command.contains(MANUAL_AGENT_RESUME_MARKER));
        assert!(command.contains("codex resume manual-thread"));
        #[cfg(not(windows))]
        assert!(command.contains("; exec "));
    }

    #[test]
    fn agent_resume_policy_distinguishes_managed_and_manual_exits() {
        assert!(agent_launch_is_managed(
            Some("wmcterm agent --mode interactive codex"),
            "codex"
        ));
        assert!(agent_launch_is_managed(
            Some("claude --dangerously-skip-permissions"),
            "Claude Code"
        ));
        assert!(!agent_launch_is_managed(Some("bash"), "codex"));
        assert!(!agent_launch_is_managed(None, "claude"));
        let manual_resume = manual_agent_resume_command("codex resume session-one");
        assert!(manual_resume.contains(MANUAL_AGENT_RESUME_MARKER));
        assert!(manual_resume.contains("codex resume session-one"));
        assert!(is_manual_agent_resume_command(&manual_resume));
        assert!(!is_manual_agent_resume_command("codex"));
        assert!(!is_manual_agent_resume_command("/bin/bash -l"));
        assert!(!agent_launch_is_managed(Some(&manual_resume), "codex"));
        #[cfg(not(windows))]
        assert!(manual_resume.contains("; exec "));

        assert!(should_persist_agent_resume(
            false,
            AgentLifecyclePhase::Running,
            true
        ));
        assert!(!should_persist_agent_resume(
            false,
            AgentLifecyclePhase::Running,
            false
        ));
        assert!(!should_persist_agent_resume(
            false,
            AgentLifecyclePhase::Exited,
            true
        ));
        assert!(should_persist_agent_resume(
            true,
            AgentLifecyclePhase::Exited,
            false
        ));
    }

    #[test]
    fn restored_commands_keep_roots_and_unverified_remote_commands() {
        let root = terminal_resume(
            Some("wmcterm codex-teams --model gpt-5"),
            Some(AgentResumeInfo::Codex {
                session_id: "root-thread".into(),
            }),
        );
        assert_eq!(
            restored_terminal_command(&root).as_deref(),
            Some("wmcterm codex-teams --model gpt-5")
        );

        let mismatched = terminal_resume(
            Some("codex resume --remote ws://127.0.0.1:44459 other-thread"),
            Some(AgentResumeInfo::Codex {
                session_id: "expected-thread".into(),
            }),
        );
        assert_eq!(
            restored_terminal_command(&mismatched).as_deref(),
            Some("codex resume --remote ws://127.0.0.1:44459 other-thread")
        );
    }

    #[test]
    fn restored_claude_command_remains_stable() {
        let terminal = terminal_resume(
            Some("wmcterm agent claude"),
            Some(AgentResumeInfo::Claude {
                session_id: "claude-session".into(),
            }),
        );
        assert_eq!(
            restored_terminal_command(&terminal).as_deref(),
            Some("wmcterm agent claude")
        );

        let resume_only = terminal_resume(
            None,
            Some(AgentResumeInfo::Claude {
                session_id: "claude-session".into(),
            }),
        );
        assert_eq!(
            restored_terminal_command(&resume_only).as_deref(),
            Some("claude --resume claude-session")
        );
    }

    #[test]
    fn screen_read_line_limit_returns_the_tail_without_changing_terminal_state() {
        assert_eq!(tail_lines("one\ntwo\nthree", 2), "two\nthree");
        assert_eq!(tail_lines("中文\n🙂", 10), "中文\n🙂");
        assert_eq!(optional_screen_lines(&json!({"lines": 2})), Ok(Some(2)));
        assert!(optional_screen_lines(&json!({"lines": 0})).is_err());
    }
    use wmcterm_core::AgentState;

    #[test]
    fn bundled_application_icon_decodes_for_winit() {
        assert!(application_icon().is_some());
    }

    #[test]
    fn windows_install_directory_is_replaced_by_user_profile() {
        let install = Path::new(r"C:\Program Files\wmcterm");
        let profile = Path::new(r"C:\Users\wangxh");
        assert_eq!(
            safe_windows_startup_cwd(
                PathBuf::from(r"c:/program files/wmcterm/"),
                Some(install),
                Some(profile),
            ),
            profile
        );
        assert_eq!(
            safe_windows_startup_cwd(
                PathBuf::from(r"D:\code\wmcterm"),
                Some(install),
                Some(profile),
            ),
            PathBuf::from(r"D:\code\wmcterm")
        );
    }

    #[test]
    fn wide_terminal_glyphs_are_centered_and_use_a_wide_cursor() {
        assert_eq!(terminal_text_cell_width("a"), 1);
        assert_eq!(terminal_text_cell_width("汉"), 2);
        assert!(!needs_grid_positioned_glyph("│"));
        assert!(!needs_grid_positioned_glyph("●"));
        assert!(needs_grid_positioned_glyph("汉"));
        assert_eq!(wide_glyph_optical_offset("a", 9.6, 16.0), (0.0, 0.0));
        let (left, top) = wide_glyph_optical_offset("汉", 9.6, 16.0);
        assert!((left - 0.8).abs() < 0.001);
        assert!((top - 0.4).abs() < 0.001);

        let row = vec![
            selection_cell("汉"),
            selection_cell(""),
            selection_cell("a"),
        ];
        assert_eq!(wide_cell_lead(&row, 0), 0);
        assert_eq!(wide_cell_lead(&row, 1), 0);
        assert_eq!(wide_cell_end(&row, 0), 1);
        assert_eq!(wide_cell_end(&row, 2), 2);
        assert!(!needs_grid_positioned_glyph("❯"));
        assert!(needs_grid_positioned_glyph("汉"));
        assert!(!needs_grid_positioned_glyph(">"));
        assert!(!needs_grid_positioned_glyph("a"));
    }

    #[test]
    fn notification_age_is_relative_and_locale_aware() {
        let now = 10 * 86_400_000;
        assert_eq!(notification_age_label(now, now - 30_000, false), "now");
        assert_eq!(notification_age_label(now, now - 120_000, false), "2m");
        assert_eq!(notification_age_label(now, now - 7_200_000, true), "2 小时");
        assert_eq!(notification_age_label(now, now - 172_800_000, true), "2 天");
    }

    #[test]
    fn normal_agent_exit_reasons_do_not_create_notifications() {
        for message in ["other", "exit", "user_requested", "logout"] {
            assert!(non_actionable_agent_exit(&AgentEvent::Stop {
                success: true,
                message: Some(message.into()),
            }));
        }
        assert!(!non_actionable_agent_exit(&AgentEvent::Stop {
            success: true,
            message: Some("Task completed".into()),
        }));
        assert!(!non_actionable_agent_exit(&AgentEvent::Stop {
            success: false,
            message: Some("other".into()),
        }));
    }

    #[test]
    fn titlebar_double_click_requires_nearby_clicks_within_half_a_second() {
        let first = std::time::Instant::now();
        assert!(titlebar_double_click(
            first,
            (100.0, 20.0),
            first + std::time::Duration::from_millis(450),
            (104.0, 23.0),
        ));
        assert!(!titlebar_double_click(
            first,
            (100.0, 20.0),
            first + std::time::Duration::from_millis(501),
            (100.0, 20.0),
        ));
        assert!(!titlebar_double_click(
            first,
            (100.0, 20.0),
            first + std::time::Duration::from_millis(200),
            (120.0, 20.0),
        ));
    }

    fn selection_cell(text: &str) -> CellSnapshot {
        CellSnapshot {
            text: text.into(),
            hyperlink: None,
            foreground: Rgb { r: 0, g: 0, b: 0 },
            background: Rgb { r: 0, g: 0, b: 0 },
            background_explicit: false,
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
        }
    }

    #[test]
    fn terminal_link_detection_trims_delimiters_and_rejects_unsafe_schemes() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            terminal_target_in_line("see (https://example.com/a).", 10, cwd),
            Some(TerminalOpenTarget::Url("https://example.com/a".into()))
        );
        assert_eq!(
            terminal_target_in_line("see (https://example.com/a).", 27, cwd),
            None
        );
        assert_eq!(
            terminal_target_in_line("[http://localhost:8080/x]", 8, cwd),
            Some(TerminalOpenTarget::Url("http://localhost:8080/x".into()))
        );
        assert_eq!(terminal_target_in_line("javascript:alert(1)", 3, cwd), None);
        assert_eq!(terminal_target_in_line("ssh://host/path", 5, cwd), None);
    }

    #[test]
    fn terminal_path_detection_resolves_cwd_and_source_positions() {
        let cwd = Path::new("/work/project");
        assert_eq!(
            terminal_target_in_line("src/main.rs:12:3", 4, cwd),
            Some(TerminalOpenTarget::Path(cwd.join("src/main.rs")))
        );
        assert_eq!(
            terminal_target_in_line("../README.md:9", 3, cwd),
            Some(TerminalOpenTarget::Path(cwd.join("../README.md")))
        );
        assert_eq!(
            terminal_target_from_osc8("file:///tmp/report.txt", cwd),
            Some(TerminalOpenTarget::Path(PathBuf::from("/tmp/report.txt")))
        );
        assert_eq!(terminal_target_in_line("ordinary", 3, cwd), None);
    }

    #[test]
    fn terminal_double_click_selects_word_without_crossing_boundaries() {
        let row = ["a", "b", "_", "中", "", ".", ".", " ", "x"].map(selection_cell);

        assert_eq!(word_cell_bounds(&row, 1), Some((0, 4)));
        assert_eq!(word_cell_bounds(&row, 4), Some((0, 4)));
        assert_eq!(word_cell_bounds(&row, 5), Some((5, 6)));
        assert_eq!(word_cell_bounds(&row, 7), Some((7, 7)));
        assert_eq!(word_cell_bounds(&row, 8), Some((8, 8)));
    }

    #[test]
    fn terminal_double_click_rejects_stale_second_press() {
        let first = std::time::Instant::now();
        assert!(terminal_double_click(
            first,
            first + std::time::Duration::from_millis(500)
        ));
        assert!(!terminal_double_click(
            first,
            first + std::time::Duration::from_millis(501)
        ));
    }

    #[test]
    fn first_press_outside_notifications_dismisses_and_propagates() {
        let panel = ui::Bounds {
            x: 650.0,
            y: 48.0,
            width: 350.0,
            height: 500.0,
        };
        assert_eq!(
            notification_overlay_press(true, panel, 100.0, 100.0),
            NotificationOverlayPress::DismissAndPropagate
        );
        assert_eq!(
            notification_overlay_press(true, panel, 700.0, 100.0),
            NotificationOverlayPress::Inside
        );
        assert_eq!(
            notification_overlay_press(false, panel, 100.0, 100.0),
            NotificationOverlayPress::Inactive
        );
        assert!(notification_toggle_press(true, true));
        assert!(!notification_toggle_press(true, false));
        assert!(!notification_toggle_press(false, true));
    }

    #[test]
    fn agent_approval_only_exposes_supported_decisions_and_maps_wire_modes() {
        let request = AgentInteractionRequest {
            request_id: "approval:test".into(),
            surface_id: uuid::Uuid::new_v4().to_string(),
            lifecycle_id: "lifecycle".into(),
            agent_pid: None,
            pid_start_time: None,
            kind: IpcAgentRequestKind::new(IpcAgentRequestKind::PERMISSION),
            message: "run cargo test".into(),
            choices: ["deny", "once", "always", "all"]
                .into_iter()
                .map(|mode| wmcterm_ipc::AgentChoice {
                    id: mode.into(),
                    label: mode.into(),
                })
                .collect(),
            deadline_unix_ms: None,
        };
        assert_eq!(
            agent_approval_actions(&request),
            vec![
                AgentApprovalAction::Deny,
                AgentApprovalAction::AllowOnce,
                AgentApprovalAction::AlwaysAllow,
            ]
        );
        let resolution = agent_approval_resolution(&request, AgentApprovalAction::AlwaysAllow);
        assert_eq!(resolution.choice_id.as_deref(), Some("always"));
        assert_eq!(resolution.approved, Some(true));
        assert_eq!(
            agent_approval_resolved_json(AgentApprovalAction::Deny),
            json!({"status":"resolved","decision":{"mode":"deny"}})
        );

        let limited = AgentInteractionRequest {
            choices: vec![],
            ..request
        };
        assert_eq!(
            agent_approval_actions(&limited),
            vec![AgentApprovalAction::Deny, AgentApprovalAction::AllowOnce]
        );
    }

    #[test]
    fn agent_approval_resolution_prefers_exact_ids_before_label_matching() {
        let request = AgentInteractionRequest {
            request_id: "approval:ids".into(),
            surface_id: uuid::Uuid::new_v4().to_string(),
            lifecycle_id: "lifecycle".into(),
            agent_pid: None,
            pid_start_time: None,
            kind: IpcAgentRequestKind::new(IpcAgentRequestKind::PERMISSION),
            message: "request".into(),
            choices: [
                ("deny", "Approve"),
                ("once", "Deny"),
                ("always", "all tools"),
                ("all", "all tools"),
            ]
            .into_iter()
            .map(|(id, label)| wmcterm_ipc::AgentChoice {
                id: id.into(),
                label: label.into(),
            })
            .collect(),
            deadline_unix_ms: None,
        };
        assert_eq!(
            agent_approval_resolution(&request, AgentApprovalAction::Deny).choice_id,
            Some("deny".into())
        );
        assert_eq!(
            agent_approval_resolution(&request, AgentApprovalAction::AllowOnce).choice_id,
            Some("once".into())
        );
        assert_eq!(
            agent_approval_resolution(&request, AgentApprovalAction::AlwaysAllow).choice_id,
            Some("always".into())
        );
    }

    #[test]
    fn agent_approval_messages_lead_with_readable_summaries_and_keep_json_details() {
        let codex = agent_approval_message(
            Language::SimplifiedChinese,
            "codex_app_server",
            "command_execution",
            "item/commandExecution/requestApproval",
            &json!({
                "command": "cargo test --workspace",
                "cwd": "/work/wmcterm",
                "reason": "验证修改"
            }),
        );
        assert!(codex.starts_with("Codex 请求执行命令\n命令: cargo test --workspace"));
        assert!(codex.contains("工作目录: /work/wmcterm"));
        assert!(codex.contains("原因: 验证修改"));
        assert!(codex.contains("技术详情（item/commandExecution/requestApproval）："));
        assert!(codex.contains("\n  \"command\": \"cargo test --workspace\""));

        let write_stdin = agent_approval_message(
            Language::English,
            "codex_app_server",
            "write_stdin",
            "item/commandExecution/requestApproval",
            &json!({
                "kind": "writeStdin",
                "stdin": "y\n",
                "processId": "process-42",
                "cwd": "/work/wmcterm"
            }),
        );
        assert!(write_stdin.starts_with(
            "Codex requests to send input to a running command\nInput: y\n\nProcess: process-42"
        ));
        assert!(write_stdin.contains("Working directory: /work/wmcterm"));

        let wire_write_stdin = agent_approval_message(
            Language::English,
            "codex_app_server",
            "write_stdin",
            "item/commandExecution/requestApproval",
            &json!({
                "kind": "writeStdin",
                "threadId": "thread-a",
                "turnId": "turn-a",
                "itemId": "item-a",
                "approvalId": "approval-a",
                "startedAtMs": 42,
                "availableDecisions": ["accept", "decline"]
            }),
        );
        assert!(
            wire_write_stdin.starts_with(
                "Codex requests to send input to a running command\n\nTechnical details"
            )
        );
        assert!(wire_write_stdin.contains("\"kind\": \"writeStdin\""));

        let claude = agent_approval_message(
            Language::English,
            "claude_hook",
            "permission",
            "PermissionRequest",
            &json!({
                "tool_name": "Bash",
                "tool_input": {"command": "git status"},
                "cwd": "/work/wmcterm"
            }),
        );
        assert!(claude.starts_with("Claude requests to use tool: Bash\nCommand: git status"));
        assert!(claude.contains("Technical details (PermissionRequest):"));
    }

    #[test]
    fn approval_detail_scrollbar_tracks_bounded_line_progress() {
        let message = (0..80)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let top = agent_request_detail_layout(PhysicalSize::new(1000, 650), &message, 0);
        assert!(top.total_lines > top.visible_lines);
        assert!(top.max_scroll > 0);
        let (track, thumb) = top.scrollbar.expect("long details have a scrollbar");
        assert_eq!(thumb.y, track.y);
        assert_eq!(
            scrollbar_scroll_for_pointer(
                track,
                thumb,
                top.max_scroll,
                track.y + track.height,
                thumb.height,
            ),
            top.max_scroll
        );

        let bottom =
            agent_request_detail_layout(PhysicalSize::new(1000, 650), &message, usize::MAX);
        let (bottom_track, bottom_thumb) = bottom.scrollbar.unwrap();
        assert!(
            (bottom_thumb.y + bottom_thumb.height - bottom_track.y - bottom_track.height).abs()
                < 0.001
        );
    }

    #[test]
    fn pending_agent_state_is_surface_scoped_and_permission_wins() {
        let target = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let request = |id: &str, surface: SurfaceId, kind: &str| AgentInteractionRequest {
            request_id: id.into(),
            surface_id: surface.to_string(),
            lifecycle_id: "lifecycle".into(),
            agent_pid: None,
            pid_start_time: None,
            kind: IpcAgentRequestKind::new(kind),
            message: id.into(),
            choices: Vec::new(),
            deadline_unix_ms: None,
        };
        let input = request("input", target, IpcAgentRequestKind::INPUT);
        let permission = request("permission", target, IpcAgentRequestKind::PERMISSION);
        let unrelated = request("other", other, IpcAgentRequestKind::PERMISSION);

        assert_eq!(
            pending_agent_state_for_surface(&[input.clone(), unrelated.clone()], target),
            Some(AgentState::WaitingForInput)
        );
        assert_eq!(
            pending_agent_state_for_surface(&[input, permission, unrelated], target),
            Some(AgentState::WaitingForPermission)
        );
        assert_eq!(pending_agent_state_for_surface(&[], target), None);
    }

    #[test]
    fn resolved_agent_state_restores_session_without_overwriting_newer_state() {
        assert_eq!(
            effective_agent_state(
                Some(AgentState::WaitingForPermission),
                Some(AgentState::Running),
                Some(AgentState::Completed),
            ),
            Some(AgentState::WaitingForPermission)
        );
        assert_eq!(
            effective_agent_state(None, Some(AgentState::Completed), Some(AgentState::Running)),
            Some(AgentState::Completed)
        );
        assert_eq!(
            effective_agent_state(None, None, Some(AgentState::WaitingForPermission)),
            Some(AgentState::Running)
        );
    }

    #[test]
    fn derived_attention_and_workspace_status_preserve_severity() {
        assert!(
            attention_priority(AttentionState::Error)
                > attention_priority(AttentionState::NeedsInput)
        );
        assert!(
            attention_priority(AttentionState::NeedsInput)
                > attention_priority(AttentionState::Activity)
        );
        assert_eq!(
            workspace_status_for_agent_priority(agent_state_priority(
                AgentState::WaitingForPermission
            )),
            wmcterm_core::WorkspaceStatus::NeedsInput
        );
    }

    #[test]
    fn recoverable_terminal_notice_does_not_become_an_error_and_clears_on_snapshot() {
        let terminal = uuid::Uuid::new_v4();
        let mut notices = HashMap::from([(
            terminal,
            TerminalNotice::Warning("copy selection: invalid value".into()),
        )]);

        assert_eq!(
            terminal_notice_attention(notices.get(&terminal).unwrap()),
            None
        );
        assert!(!terminal_notice_is_error(notices.get(&terminal).unwrap()));
        assert!(clear_recoverable_terminal_notice(&mut notices, terminal));
        assert!(!notices.contains_key(&terminal));

        notices.insert(terminal, TerminalNotice::Error("PTY closed".into()));
        assert!(!clear_recoverable_terminal_notice(&mut notices, terminal));
        assert!(terminal_notice_is_error(notices.get(&terminal).unwrap()));
    }

    #[test]
    fn newly_detected_foreground_agent_is_idle_until_work_is_submitted() {
        let metadata =
            detected_foreground_agent_metadata(AgentKind::Codex, AgentRunMode::Interactive);
        assert_eq!(metadata.name, "codex");
        assert_eq!(metadata.state, AgentState::Idle);
        assert_eq!(
            workspace_status_for_agent_priority(agent_state_priority(metadata.state)),
            wmcterm_core::WorkspaceStatus::Idle
        );
    }

    #[test]
    fn codex_local_commands_do_not_start_an_agent_turn_except_goal_resume() {
        assert!(!agent_submission_starts_turn(""));
        assert!(!agent_submission_starts_turn("   /model"));
        assert!(!agent_submission_starts_turn("/compact now"));
        assert!(!agent_submission_starts_turn("/goal"));
        assert!(!agent_submission_starts_turn("/goal status"));
        assert!(agent_submission_starts_turn("/goal resume"));
        assert!(agent_submission_starts_turn("  /goal resume goal-id"));
        assert!(agent_submission_starts_turn("explain this failure"));
        assert!(is_agent_interrupt(
            PhysicalKey::Code(KeyCode::KeyC),
            ModifiersState::CONTROL
        ));
        assert!(!is_agent_interrupt(
            PhysicalKey::Code(KeyCode::KeyC),
            ModifiersState::CONTROL | ModifiersState::ALT
        ));
    }

    #[test]
    fn agent_paste_tracking_keeps_only_a_bounded_utf8_prefix() {
        let mut tracked = String::from("prompt ");
        append_bounded_agent_input(&mut tracked, &"中文🙂".repeat(2048));
        assert!(tracked.len() <= MAX_TRACKED_AGENT_INPUT_BYTES);
        assert!(tracked.is_char_boundary(tracked.len()));
        assert!(tracked.starts_with("prompt 中文🙂"));

        let full = tracked.clone();
        append_bounded_agent_input(&mut tracked, "ignored");
        assert!(tracked.starts_with(&full));
        assert!(tracked.len() <= MAX_TRACKED_AGENT_INPUT_BYTES);
        assert!(agent_submission_starts_turn(&tracked));
    }

    #[test]
    fn agent_approval_result_cache_is_bounded_and_updates_in_place() {
        let mut results = HashMap::new();
        let mut order = VecDeque::new();
        remember_bounded_approval_result(
            &mut results,
            &mut order,
            "first".into(),
            AgentApprovalAction::Deny,
            2,
        );
        remember_bounded_approval_result(
            &mut results,
            &mut order,
            "second".into(),
            AgentApprovalAction::AllowOnce,
            2,
        );
        remember_bounded_approval_result(
            &mut results,
            &mut order,
            "second".into(),
            AgentApprovalAction::AlwaysAllow,
            2,
        );
        remember_bounded_approval_result(
            &mut results,
            &mut order,
            "third".into(),
            AgentApprovalAction::Deny,
            2,
        );

        assert_eq!(order, VecDeque::from(["second".into(), "third".into()]));
        assert!(!results.contains_key("first"));
        assert_eq!(
            results.get("second"),
            Some(&AgentApprovalAction::AlwaysAllow)
        );
    }

    #[test]
    fn approval_protocol_accepts_only_known_codex_and_claude_shapes() {
        assert!(
            validate_agent_approval_protocol(
                "codex_app_server",
                "item/commandExecution/requestApproval",
                "command_execution"
            )
            .is_ok()
        );
        assert!(
            validate_agent_approval_protocol(
                "codex_app_server",
                "item/commandExecution/requestApproval",
                "write_stdin"
            )
            .is_ok()
        );
        assert!(
            validate_agent_approval_protocol("claude_hook", "PermissionRequest", "permission")
                .is_ok()
        );
        assert!(
            validate_agent_approval_protocol("claude_hook", "PreToolUse", "permission").is_err()
        );
        assert!(
            validate_agent_approval_protocol("unknown", "PermissionRequest", "permission").is_err()
        );
    }

    #[test]
    fn claude_approval_binding_requires_current_terminal_and_rejects_other_agents() {
        assert!(claude_approval_binding_matches(
            "lifecycle-1",
            Some("Claude Code"),
            Some("claude"),
            "lifecycle-1"
        ));
        // SessionStart telemetry is best-effort and may race the first
        // PermissionRequest. A missing display/session record must not make
        // the bridge permanently unusable; the user still decides in the UI.
        assert!(claude_approval_binding_matches(
            "lifecycle-1",
            None,
            Some("agent"),
            "lifecycle-1"
        ));
        assert!(!claude_approval_binding_matches(
            "lifecycle-1",
            Some("claude"),
            Some("claude"),
            "lifecycle-old"
        ));
        assert!(!claude_approval_binding_matches(
            "lifecycle-1",
            Some("codex"),
            Some("claude"),
            "lifecycle-1",
        ));
    }

    #[test]
    fn claude_subagent_status_is_scoped_by_session_and_surface() {
        let surface = SurfaceId::new_v4();
        let other_surface = SurfaceId::new_v4();
        let start = |session: &str, agent: &str| {
            wmcterm_agent::normalize_claude_hook_event(
                Some("SubagentStart"),
                &json!({
                    "hook_event_name": "SubagentStart",
                    "session_id": session,
                    "agent_id": agent,
                    "agent_type": "Explore"
                }),
            )
            .unwrap()
        };
        let stop = |session: &str, agent: &str| {
            wmcterm_agent::normalize_claude_hook_event(
                Some("SubagentStop"),
                &json!({
                    "hook_event_name": "SubagentStop",
                    "session_id": session,
                    "agent_id": agent,
                    "agent_type": "Explore"
                }),
            )
            .unwrap()
        };
        let mut statuses = HashMap::new();
        apply_claude_subagent_status(&mut statuses, surface, &start("one", "worker"));
        apply_claude_subagent_status(&mut statuses, other_surface, &start("two", "worker"));
        apply_claude_subagent_status(&mut statuses, other_surface, &start("one", "worker"));
        assert_eq!(statuses.len(), 3);

        apply_claude_subagent_status(&mut statuses, surface, &stop("one", "worker"));
        assert_eq!(statuses.len(), 2);
        assert!(
            statuses
                .values()
                .all(|status| status.surface == other_surface)
        );

        let goal_stop = wmcterm_agent::normalize_claude_hook_event(
            Some("Stop"),
            &json!({
                "hook_event_name": "Stop",
                "session_id": "one",
                "stop_hook_active": true
            }),
        )
        .unwrap();
        apply_claude_subagent_status(&mut statuses, other_surface, &goal_stop);
        assert_eq!(statuses.len(), 2);

        let session_end = wmcterm_agent::normalize_claude_hook_event(
            Some("SessionEnd"),
            &json!({
                "hook_event_name": "SessionEnd",
                "session_id": "two",
                "reason": "logout"
            }),
        )
        .unwrap();
        apply_claude_subagent_status(&mut statuses, other_surface, &session_end);
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses.values().next().unwrap().session_id, "one");

        let final_end = wmcterm_agent::normalize_claude_hook_event(
            Some("SessionEnd"),
            &json!({
                "hook_event_name": "SessionEnd",
                "session_id": "one",
                "reason": "logout"
            }),
        )
        .unwrap();
        apply_claude_subagent_status(&mut statuses, other_surface, &final_end);
        assert!(statuses.is_empty());
    }

    #[test]
    fn codex_subagents_keep_parent_running_until_the_last_matching_stop() {
        let surface = SurfaceId::new_v4();
        let hook = |kind: &str, agent: &str| {
            wmcterm_agent::normalize_codex_subagent_hook_event(
                Some(kind),
                &json!({
                    "hook_event_name": kind,
                    "session_id": "session-one",
                    "turn_id": "turn-one",
                    "agent_id": agent,
                    "agent_type": "worker"
                }),
            )
            .unwrap()
        };
        let mut statuses = CodexSubagentTracker::default();
        apply_codex_subagent_status(&mut statuses, surface, &hook("SubagentStart", "one"));
        apply_codex_subagent_status(&mut statuses, surface, &hook("SubagentStart", "two"));
        assert_eq!(codex_subagent_count(&statuses, surface, "session-one"), 2);
        assert_eq!(
            effective_codex_agent_state(AgentState::Completed, true),
            AgentState::Running
        );

        apply_codex_subagent_status(&mut statuses, surface, &hook("SubagentStop", "one"));
        assert_eq!(codex_subagent_count(&statuses, surface, "session-one"), 1);
        assert_eq!(
            effective_codex_agent_state(AgentState::Completed, true),
            AgentState::Running
        );

        apply_codex_subagent_status(&mut statuses, surface, &hook("SubagentStop", "two"));
        assert_eq!(codex_subagent_count(&statuses, surface, "session-one"), 0);
        assert_eq!(
            effective_codex_agent_state(AgentState::Completed, false),
            AgentState::Completed
        );
    }

    #[test]
    fn codex_subagent_stop_is_idempotent_and_scoped_by_surface_and_session() {
        let surface = SurfaceId::new_v4();
        let other_surface = SurfaceId::new_v4();
        let hook = |kind: &str, session: &str| {
            wmcterm_agent::normalize_codex_subagent_hook_event(
                Some(kind),
                &json!({
                    "hook_event_name": kind,
                    "session_id": session,
                    "turn_id": "turn-one",
                    "agent_id": "shared-agent",
                    "agent_type": "worker"
                }),
            )
            .unwrap()
        };
        let mut statuses = CodexSubagentTracker::default();
        apply_codex_subagent_status(
            &mut statuses,
            surface,
            &hook("SubagentStart", "session-one"),
        );
        apply_codex_subagent_status(
            &mut statuses,
            surface,
            &hook("SubagentStart", "session-two"),
        );
        apply_codex_subagent_status(
            &mut statuses,
            other_surface,
            &hook("SubagentStart", "session-one"),
        );
        assert_eq!(statuses.len(), 3);

        apply_codex_subagent_status(
            &mut statuses,
            surface,
            &hook("SubagentStop", "unknown-session"),
        );
        apply_codex_subagent_status(&mut statuses, surface, &hook("SubagentStop", "session-one"));
        apply_codex_subagent_status(&mut statuses, surface, &hook("SubagentStop", "session-one"));
        assert_eq!(statuses.len(), 2);
        assert_eq!(codex_subagent_count(&statuses, surface, "session-two"), 1);
        assert_eq!(
            codex_subagent_count(&statuses, other_surface, "session-one"),
            1
        );
    }

    #[test]
    fn codex_subagent_stop_tombstone_rejects_a_late_start_for_the_same_key() {
        let surface = SurfaceId::new_v4();
        let hook = |kind: &str, session: &str| {
            wmcterm_agent::normalize_codex_subagent_hook_event(
                Some(kind),
                &json!({
                    "hook_event_name": kind,
                    "session_id": session,
                    "turn_id": "turn-one",
                    "agent_id": "agent-one",
                    "agent_type": "worker"
                }),
            )
            .unwrap()
        };
        let mut statuses = CodexSubagentTracker::default();

        apply_codex_subagent_status(&mut statuses, surface, &hook("SubagentStop", "session-one"));
        apply_codex_subagent_status(
            &mut statuses,
            surface,
            &hook("SubagentStart", "session-one"),
        );
        assert_eq!(codex_subagent_count(&statuses, surface, "session-one"), 0);

        apply_codex_subagent_status(
            &mut statuses,
            surface,
            &hook("SubagentStart", "session-two"),
        );
        assert_eq!(codex_subagent_count(&statuses, surface, "session-two"), 1);
        assert_eq!(statuses.retained_len(), 2);

        clear_codex_subagent_session(&mut statuses, surface, "session-one");
        assert_eq!(statuses.retained_len(), 1);
        clear_codex_subagent_surface(&mut statuses, surface);
        assert_eq!(statuses.retained_len(), 0);
        assert!(statuses.stopped_order.is_empty());
    }

    #[test]
    fn codex_subagent_tombstones_are_bounded_per_session_without_evicting_active_agents() {
        let surface = SurfaceId::new_v4();
        let hook = |kind: &str, agent: &str| {
            wmcterm_agent::normalize_codex_subagent_hook_event(
                Some(kind),
                &json!({
                    "hook_event_name": kind,
                    "session_id": "long-session",
                    "turn_id": "turn-one",
                    "agent_id": agent,
                    "agent_type": "worker"
                }),
            )
            .unwrap()
        };
        let mut statuses = CodexSubagentTracker::default();
        apply_codex_subagent_status(
            &mut statuses,
            surface,
            &hook("SubagentStart", "active-agent"),
        );
        for index in 0..=MAX_CODEX_STOPPED_TOMBSTONES_PER_SESSION {
            apply_codex_subagent_status(
                &mut statuses,
                surface,
                &hook("SubagentStop", &format!("stopped-{index}")),
            );
        }

        assert_eq!(
            statuses.retained_len(),
            MAX_CODEX_STOPPED_TOMBSTONES_PER_SESSION + 1
        );
        assert_eq!(codex_subagent_count(&statuses, surface, "long-session"), 1);

        apply_codex_subagent_status(
            &mut statuses,
            surface,
            &hook(
                "SubagentStart",
                &format!("stopped-{MAX_CODEX_STOPPED_TOMBSTONES_PER_SESSION}"),
            ),
        );
        assert_eq!(
            codex_subagent_count(&statuses, surface, "long-session"),
            1,
            "a recent stopped key must remain fail-closed"
        );

        apply_codex_subagent_status(&mut statuses, surface, &hook("SubagentStart", "stopped-0"));
        assert_eq!(
            codex_subagent_count(&statuses, surface, "long-session"),
            2,
            "the bounded replay horizon permits the oldest evicted key"
        );
    }

    #[test]
    fn codex_subagent_cleanup_is_scoped_to_session_or_surface() {
        let surface = SurfaceId::new_v4();
        let other_surface = SurfaceId::new_v4();
        let start = |session: &str, agent: &str| {
            wmcterm_agent::normalize_codex_subagent_hook_event(
                Some("SubagentStart"),
                &json!({
                    "hook_event_name": "SubagentStart",
                    "session_id": session,
                    "turn_id": "turn-one",
                    "agent_id": agent,
                    "agent_type": "worker"
                }),
            )
            .unwrap()
        };
        let mut statuses = CodexSubagentTracker::default();
        apply_codex_subagent_status(&mut statuses, surface, &start("one", "a"));
        apply_codex_subagent_status(&mut statuses, surface, &start("two", "b"));
        apply_codex_subagent_status(&mut statuses, other_surface, &start("one", "c"));

        clear_codex_subagent_session(&mut statuses, surface, "one");
        assert_eq!(statuses.len(), 2);
        assert_eq!(codex_subagent_count(&statuses, surface, "two"), 1);
        assert_eq!(codex_subagent_count(&statuses, other_surface, "one"), 1);

        clear_codex_subagent_surface(&mut statuses, surface);
        assert_eq!(statuses.len(), 1);
        assert_eq!(codex_subagent_count(&statuses, other_surface, "one"), 1);
    }

    #[test]
    fn codex_activity_does_not_override_needs_input_or_error() {
        assert_eq!(
            effective_codex_agent_state(AgentState::WaitingForInput, true),
            AgentState::WaitingForInput
        );
        assert_eq!(
            effective_codex_agent_state(AgentState::WaitingForPermission, true),
            AgentState::WaitingForPermission
        );
        assert_eq!(
            effective_codex_agent_state(AgentState::Error, true),
            AgentState::Error
        );
    }

    #[test]
    fn physical_letters_recover_control_keys_from_linux_input() {
        assert_eq!(
            physical_ascii_character(PhysicalKey::Code(KeyCode::KeyC)),
            Some('c')
        );
        assert_eq!(
            physical_ascii_character(PhysicalKey::Code(KeyCode::KeyD)),
            Some('d')
        );
    }

    #[test]
    fn named_space_is_forwarded_to_the_terminal() {
        assert_eq!(
            terminal_input_key(&Key::Named(NamedKey::Space), None),
            Some(InputKey::Space)
        );
    }

    #[test]
    fn window_switch_shortcuts_are_not_forwarded_to_the_terminal() {
        let tab = Key::Named(NamedKey::Tab);
        let physical_tab = PhysicalKey::Code(KeyCode::Tab);
        assert!(is_window_switch_shortcut(
            &tab,
            physical_tab,
            ModifiersState::ALT,
            false
        ));
        assert!(is_window_switch_shortcut(
            &tab,
            physical_tab,
            ModifiersState::SUPER,
            false
        ));
        assert!(is_window_switch_shortcut(
            &tab,
            physical_tab,
            ModifiersState::ALT | ModifiersState::SHIFT,
            false
        ));
        // Some window managers deliver Tab before ModifiersChanged reports
        // Alt. The physical Alt state must still keep it out of the PTY.
        assert!(is_window_switch_shortcut(
            &tab,
            physical_tab,
            ModifiersState::empty(),
            true
        ));
        assert!(is_window_switch_shortcut(
            &Key::Unidentified(winit::keyboard::NativeKey::Unidentified),
            physical_tab,
            ModifiersState::empty(),
            true
        ));
        assert!(!is_window_switch_shortcut(
            &tab,
            physical_tab,
            ModifiersState::CONTROL,
            false
        ));
        assert!(!is_window_switch_shortcut(
            &Key::Named(NamedKey::Enter),
            PhysicalKey::Code(KeyCode::Enter),
            ModifiersState::ALT,
            true
        ));
        assert!(should_suppress_focus_transition_key(
            false,
            &Key::Named(NamedKey::Enter),
            PhysicalKey::Code(KeyCode::Enter),
            ModifiersState::empty(),
            false
        ));
        assert!(should_suppress_focus_transition_key(
            true,
            &Key::Character("\t".into()),
            physical_tab,
            ModifiersState::empty(),
            true
        ));
        assert!(!should_suppress_focus_transition_key(
            true,
            &Key::Named(NamedKey::Enter),
            PhysicalKey::Code(KeyCode::Enter),
            ModifiersState::empty(),
            false
        ));
        assert!(should_suppress_recent_focus_tab(
            ElementState::Pressed,
            physical_tab,
            std::time::Duration::from_millis(20)
        ));
        assert!(!should_suppress_recent_focus_tab(
            ElementState::Pressed,
            physical_tab,
            UNMODIFIED_TAB_FOCUS_GRACE
        ));
        assert!(!should_suppress_recent_focus_tab(
            ElementState::Pressed,
            PhysicalKey::Code(KeyCode::Enter),
            std::time::Duration::from_millis(20)
        ));
    }

    #[test]
    fn focus_input_quarantine_covers_late_cross_window_keys() {
        assert!(focus_input_should_remain_quarantined(
            std::time::Duration::from_millis(20),
            false
        ));
        assert!(!focus_input_should_remain_quarantined(
            FOCUS_INPUT_QUARANTINE_GRACE,
            false
        ));
        assert!(focus_input_should_remain_quarantined(
            FOCUS_INPUT_QUARANTINE_GRACE,
            true
        ));
    }

    #[test]
    fn managed_agent_launcher_encodes_explicit_safe_modes() {
        assert!(
            managed_agent_command(AgentRunMode::Interactive, "codex")
                .ends_with(" agent --mode interactive codex")
        );
        assert!(
            managed_agent_command(AgentRunMode::Unattended, "claude")
                .ends_with(" agent --mode unattended claude")
        );
        assert_eq!(
            powershell_executable_command(
                std::path::Path::new(r"C:\Program Files\wmcterm\wmcterm.exe"),
                "agent --mode unattended codex",
            ),
            r"& 'C:\Program Files\wmcterm\wmcterm.exe' agent --mode unattended codex"
        );
    }

    #[test]
    fn full_access_commands_are_provider_native_and_not_managed_cli_flags() {
        assert_eq!(
            ManagedAgent::Claude.full_access_command(),
            "claude --dangerously-skip-permissions"
        );
        assert_eq!(
            ManagedAgent::Codex.full_access_command(),
            "codex --dangerously-bypass-approvals-and-sandbox"
        );
    }

    #[test]
    fn finds_and_resizes_nested_divider() {
        let first = PaneId::new_v4();
        let second = PaneId::new_v4();
        let mut root = PaneNode::leaf(first);
        root.split(first, second, SplitAxis::Vertical, 0.5).unwrap();
        let area = Rect {
            x: 100.0,
            y: 40.0,
            width: 800.0,
            height: 500.0,
        };
        let drag = divider_at(&root, area, 500.0, 200.0, &mut Vec::new()).unwrap();
        assert_eq!(drag.axis, SplitAxis::Vertical);
        set_split_ratio(&mut root, &drag.path, 0.7);
        assert!(matches!(root, PaneNode::Split { ratio, .. } if ratio == 0.7));
    }

    #[test]
    fn lays_out_visible_lines_for_both_split_axes() {
        let first = PaneId::new_v4();
        let second = PaneId::new_v4();
        let third = PaneId::new_v4();
        let mut root = PaneNode::leaf(first);
        root.split(first, second, SplitAxis::Vertical, 0.5).unwrap();
        root.split(second, third, SplitAxis::Horizontal, 0.5)
            .unwrap();
        let area = Rect {
            x: 100.0,
            y: 40.0,
            width: 800.0,
            height: 500.0,
        };
        let mut dividers = Vec::new();
        collect_dividers(&root, area, &mut Vec::new(), &mut dividers);

        assert_eq!(dividers.len(), 2);
        let vertical = divider_line(&dividers[0], 1.0);
        assert_eq!(vertical.x, 499.5);
        assert_eq!(vertical.y, 40.0);
        assert_eq!(vertical.width, 1.0);
        assert_eq!(vertical.height, 500.0);

        let horizontal = divider_line(&dividers[1], 2.0);
        assert_eq!(horizontal.x, 503.0);
        assert_eq!(horizontal.y, 289.0);
        assert_eq!(horizontal.width, 397.0);
        assert_eq!(horizontal.height, 2.0);
    }

    #[test]
    fn terminal_workspace_uses_the_removed_footer_space() {
        let area = terminal_workspace_area(1000.0, 650.0, 240.0);
        assert_eq!(area.x, 240.0);
        assert_eq!(area.y, 40.0);
        assert_eq!(area.width, 760.0);
        assert_eq!(area.height, 606.0);
        assert_eq!(area.y + area.height, 646.0);
    }

    #[test]
    fn terminal_content_hit_excludes_chrome_and_includes_the_terminal_body() {
        let pane = Rect {
            x: 252.0,
            y: 40.0,
            width: 736.0,
            height: 606.0,
        };

        assert!(!terminal_content_contains(pane, 120.0, 200.0));
        assert!(!terminal_content_contains(pane, 400.0, 64.0));
        assert!(!terminal_content_contains(pane, 259.0, 100.0));
        assert!(terminal_content_contains(pane, 260.0, 65.0));
    }

    #[test]
    fn selection_redraws_are_coalesced_to_frame_rate() {
        let start = std::time::Instant::now();
        assert!(selection_redraw_due(None, start));
        assert!(!selection_redraw_due(
            Some(start),
            start + std::time::Duration::from_millis(16)
        ));
        assert!(selection_redraw_due(
            Some(start),
            start + std::time::Duration::from_millis(17)
        ));
    }

    #[test]
    fn terminal_selection_uses_cell_color_and_high_contrast_text() {
        let mut bright = selection_cell("A");
        bright.foreground = Rgb {
            r: 240,
            g: 200,
            b: 80,
        };
        let mut dark = selection_cell("B");
        dark.foreground = Rgb { r: 8, g: 20, b: 45 };
        let row = [bright, dark];

        let bright_background = terminal_selection_background(&row, 0);
        let dark_background = terminal_selection_background(&row, 1);
        assert_eq!(bright_background, [240, 200, 80, 255]);
        assert_eq!(dark_background, [8, 20, 45, 255]);
        assert_eq!(
            contrasting_selection_foreground(bright_background),
            [0, 0, 0, 255]
        );
        assert_eq!(
            contrasting_selection_foreground(dark_background),
            [255, 255, 255, 255]
        );
    }

    #[test]
    fn terminal_selection_handles_wide_continuations_and_blank_cells() {
        let mut wide = selection_cell("中");
        wide.foreground = Rgb {
            r: 40,
            g: 180,
            b: 90,
        };
        let continuation = selection_cell("");
        let mut blank = selection_cell(" ");
        blank.foreground = Rgb {
            r: 210,
            g: 220,
            b: 230,
        };
        let row = [wide, continuation, blank];

        assert_eq!(terminal_selection_background(&row, 1), [40, 180, 90, 255]);
        assert_eq!(terminal_selection_background(&row, 2), [210, 220, 230, 255]);
    }

    #[test]
    fn terminal_selection_bounds_cover_only_the_inclusive_row_ranges() {
        let selection = ((3, 10), (2, 12));
        assert!(!terminal_cell_is_selected(2, 10, selection));
        assert!(terminal_cell_is_selected(3, 10, selection));
        assert!(terminal_cell_is_selected(0, 11, selection));
        assert!(terminal_cell_is_selected(2, 12, selection));
        assert!(!terminal_cell_is_selected(3, 12, selection));
    }

    #[test]
    fn expensive_frames_are_throttled_but_remain_responsive() {
        assert_eq!(
            adaptive_frame_interval(std::time::Duration::ZERO),
            FRAME_INTERVAL
        );
        assert_eq!(
            adaptive_frame_interval(std::time::Duration::from_millis(20)),
            std::time::Duration::from_millis(40)
        );
        assert_eq!(
            adaptive_frame_interval(std::time::Duration::from_millis(40)),
            MAX_FRAME_INTERVAL
        );
    }

    #[test]
    fn frame_work_estimate_smooths_transient_spikes() {
        assert_eq!(
            smooth_frame_work(
                std::time::Duration::from_millis(8),
                std::time::Duration::from_millis(24)
            ),
            std::time::Duration::from_millis(12)
        );
        assert_eq!(
            smooth_frame_work(
                std::time::Duration::ZERO,
                std::time::Duration::from_millis(7)
            ),
            std::time::Duration::from_millis(7)
        );
    }

    #[test]
    fn pixel_scroll_accumulates_sub_cell_touchpad_motion() {
        let mut remainder = 0.0;
        assert_eq!(accumulate_pixel_scroll(&mut remainder, 4.0, 20.0), 0);
        assert_eq!(accumulate_pixel_scroll(&mut remainder, 9.0, 20.0), 0);
        assert_eq!(accumulate_pixel_scroll(&mut remainder, 8.0, 20.0), 1);
        assert_eq!(remainder, 1.0);
        assert_eq!(accumulate_pixel_scroll(&mut remainder, -22.0, 20.0), -1);
        assert_eq!(remainder, -1.0);
    }

    #[test]
    fn cursor_blink_uses_elapsed_half_periods() {
        let start = std::time::Instant::now();
        let mut last = start;
        let mut visible = true;
        assert!(!advance_cursor_blink(
            &mut visible,
            &mut last,
            start + std::time::Duration::from_millis(499)
        ));
        assert!(advance_cursor_blink(
            &mut visible,
            &mut last,
            start + std::time::Duration::from_millis(500)
        ));
        assert!(!visible);
        assert!(!advance_cursor_blink(
            &mut visible,
            &mut last,
            start + std::time::Duration::from_millis(1_500)
        ));
        assert!(!visible);
        assert_eq!(last, start + std::time::Duration::from_millis(1_500));
    }

    #[test]
    fn logical_window_dimensions_follow_scale_factor() {
        assert_eq!(
            logical_window_size(PhysicalSize::new(1500, 975), 1.5),
            PhysicalSize::new(1000, 650)
        );
    }

    #[test]
    fn terminal_mouse_geometry_uses_physical_pixels_at_fractional_scale() {
        let (x, y, cell_width, cell_height) = terminal_mouse_geometry(96.0, 224.0, 9.6, 22.4, 1.25);
        assert!((x - 120.0).abs() < f32::EPSILON);
        assert!((y - 280.0).abs() < f32::EPSILON);
        assert_eq!(cell_width, 12);
        assert_eq!(cell_height, 28);
        assert!((x / cell_width as f32 - 10.0).abs() < f32::EPSILON);
        assert!((y / cell_height as f32 - 10.0).abs() < f32::EPSILON);
        let (x, y, cell_width, cell_height) = terminal_mouse_geometry(84.0, 196.0, 8.4, 19.6, 1.0);
        assert_eq!((x, y, cell_width, cell_height), (80.0, 200.0, 8, 20));
        assert_eq!(x / cell_width as f32, 10.0);
        assert_eq!(y / cell_height as f32, 10.0);
    }

    #[test]
    fn terminal_right_click_respects_application_mouse_tracking() {
        assert!(terminal_right_click_opens_context_menu(false, false));
        assert!(!terminal_right_click_opens_context_menu(false, true));
        assert!(terminal_right_click_opens_context_menu(true, false));
        assert!(terminal_right_click_opens_context_menu(true, true));
    }

    #[test]
    fn background_opacity_scales_alpha_without_dimming_rgb() {
        assert_eq!(
            background_opacity([10, 20, 30, 200], 0.5),
            [10, 20, 30, 100]
        );
        assert_eq!(
            background_opacity([10, 20, 30, 200], 2.0),
            [10, 20, 30, 200]
        );
    }

    #[test]
    fn adjacent_terminal_cells_with_the_same_style_share_a_span() {
        let mut spans = Vec::new();
        push_terminal_span(&mut spans, "a", [1, 2, 3, 255], false, false);
        push_terminal_span(&mut spans, "b", [1, 2, 3, 255], false, false);
        push_terminal_span(&mut spans, "c", [1, 2, 3, 255], true, false);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "ab");
        assert_eq!(spans[1].text, "c");
    }

    #[test]
    fn selection_autoscroll_accelerates_beyond_both_vertical_edges() {
        assert_eq!(
            selection_autoscroll_lines(99.0, 100.0, 500.0, 20.0),
            Some(-1)
        );
        assert_eq!(
            selection_autoscroll_lines(39.0, 100.0, 500.0, 20.0),
            Some(-4)
        );
        assert_eq!(selection_autoscroll_lines(100.0, 100.0, 500.0, 20.0), None);
        assert_eq!(selection_autoscroll_lines(499.9, 100.0, 500.0, 20.0), None);
        assert_eq!(
            selection_autoscroll_lines(500.0, 100.0, 500.0, 20.0),
            Some(1)
        );
        assert_eq!(
            selection_autoscroll_lines(1000.0, 100.0, 500.0, 20.0),
            Some(8)
        );
    }

    #[test]
    fn foreground_terminal_notifications_arrive_read_only_while_focused() {
        let terminal = uuid::Uuid::new_v4();
        assert!(notification_arrives_read(true, Some(terminal), terminal));
        assert!(!notification_arrives_read(false, Some(terminal), terminal));
        assert!(!notification_arrives_read(
            true,
            Some(uuid::Uuid::new_v4()),
            terminal
        ));
    }

    #[test]
    fn approval_uses_native_notification_when_target_is_not_being_viewed() {
        let target = uuid::Uuid::new_v4();
        assert!(approval_needs_native_notification(
            true,
            false,
            Some(target),
            target
        ));
        assert!(approval_needs_native_notification(
            true,
            true,
            Some(uuid::Uuid::new_v4()),
            target
        ));
        assert!(!approval_needs_native_notification(
            true,
            true,
            Some(target),
            target
        ));
        assert!(!approval_needs_native_notification(
            false, false, None, target
        ));
    }

    #[test]
    fn cross_surface_approval_does_not_reveal_panel_or_steal_an_editor() {
        let active = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();

        assert!(!approval_should_reveal_panel(false, Some(active), other));
        assert!(!approval_should_reveal_panel(true, Some(active), active));
        assert!(!approval_should_reveal_panel(true, Some(active), other));
        assert!(approval_should_reveal_panel(false, Some(active), active));
    }

    #[test]
    fn terminal_interaction_reads_only_notifications_for_its_surface() {
        let workspace = uuid::Uuid::new_v4();
        let target = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();
        let make_notification = |surface, read| Notification {
            id: uuid::Uuid::new_v4(),
            workspace,
            surface,
            title: "test".into(),
            body: "body".into(),
            attention: AttentionState::Activity,
            created_at_unix_ms: 0,
            read,
        };
        let mut notifications = vec![
            make_notification(target, false),
            make_notification(target, true),
            make_notification(other, false),
        ];

        assert!(mark_surface_notifications_read(&mut notifications, target));
        assert!(notifications[0].read);
        assert!(notifications[1].read);
        assert!(!notifications[2].read);
        assert!(!mark_surface_notifications_read(&mut notifications, target));
    }

    #[test]
    fn normalizes_reverse_terminal_selection() {
        assert_eq!(ordered_cells((8, 4), (2, 1)), ((2, 1), (8, 4)));
        assert_eq!(ordered_cells((1, 2), (5, 2)), ((1, 2), (5, 2)));
    }

    #[test]
    fn exited_terminal_selection_can_be_copied_from_its_final_snapshot() {
        let snapshot = ScreenSnapshot {
            cols: 6,
            rows: 2,
            lines: vec![],
            cells: vec![
                ["a", "错", "", "b", " ", " "].map(selection_cell).to_vec(),
                ["c", "d", "e", "f", " ", " "].map(selection_cell).to_vec(),
            ],
            cursor: None,
            mouse_tracking: false,
            scrollback_rows: 4,
            viewport_offset: 0,
            screen_row_origin: 4,
        };

        assert_eq!(
            snapshot_selection_text(&snapshot, (1, 4), (2, 5)).as_deref(),
            Some("错b\ncde")
        );
        assert_eq!(snapshot_selection_text(&snapshot, (0, 3), (1, 4)), None);
    }

    #[test]
    fn markdown_blocks_map_to_distinct_visual_styles() {
        let document = MarkdownDocument::parse(
            "# Heading\n\nParagraph\n\n- item\n\n```rust\nlet x = 1;\n```\n\n> quote",
        );
        let lines = markdown_lines(&document, neutral_document_colors());
        assert!(
            lines
                .iter()
                .any(|line| line.text == "▌ Heading" && line.bold)
        );
        assert!(lines.iter().any(|line| line.text == "  • item"));
        assert!(lines.iter().any(|line| line.text == "│ let x = 1;"));
        assert!(
            lines
                .iter()
                .any(|line| line.text == "│ quote" && line.italic)
        );
    }

    #[test]
    fn log_levels_map_to_severity_colors() {
        let colors = neutral_document_colors();
        assert_eq!(log_level_color(LogLevel::Debug, colors), colors.muted);
        assert_eq!(log_level_color(LogLevel::Warn, colors), colors.warning);
        assert_eq!(log_level_color(LogLevel::Error, colors), colors.error);
    }

    #[test]
    fn log_filter_prompt_parses_and_round_trips() {
        let filter = parse_log_filter("level:error,warn request failed");
        assert_eq!(filter.query.as_deref(), Some("request failed"));
        assert_eq!(
            filter.levels.as_deref(),
            Some([LogLevel::Error, LogLevel::Warn].as_slice())
        );
        assert_eq!(
            format_log_filter(&filter),
            "level:error,warn request failed"
        );
    }

    #[test]
    fn document_search_offsets_match_filtered_visual_rows() {
        let mut logs = LogSurface::new("test", 10);
        logs.push("INFO hidden");
        logs.push("ERROR first match");
        logs.push("WARN hidden");
        logs.push("ERROR second match");
        logs.set_filter(LogFilter {
            query: None,
            levels: Some(vec![LogLevel::Error]),
        });
        assert_eq!(document_search_offsets(&logs, "match"), vec![0, 1]);
    }

    #[test]
    fn tab_context_allows_closing_the_final_tab() {
        assert!(tab_context_action_enabled(TabContextAction::Close, 1));
        assert!(!tab_context_action_enabled(
            TabContextAction::CloseOthers,
            1
        ));
        assert!(tab_context_action_enabled(TabContextAction::NewSurface, 1));
        assert!(tab_context_action_enabled(TabContextAction::Close, 2));
    }

    #[test]
    fn workspace_context_allows_closing_the_final_workspace() {
        assert!(workspace_context_action_enabled(
            WorkspaceContextAction::Close,
            1
        ));
        assert!(workspace_context_action_enabled(
            WorkspaceContextAction::Close,
            2
        ));
        assert!(workspace_context_action_enabled(
            WorkspaceContextAction::Rename,
            1
        ));
    }

    #[test]
    fn unread_notification_routes_to_its_workspace_pane_and_tab() {
        let mut state = AppState::default();
        let workspace = state.workspaces[0].id;
        let pane = state.workspaces[0].active_pane;
        let surface = state.panes[&pane].surfaces[0];
        state
            .execute(AppCommand::Workspace(WorkspaceCommand::Create {
                name: "other".into(),
                cwd: std::path::PathBuf::from("/tmp"),
            }))
            .unwrap();
        let notification = uuid::Uuid::new_v4();
        state.push_notification(Notification {
            id: notification,
            workspace,
            surface,
            title: "Needs input".into(),
            body: "approve command".into(),
            attention: AttentionState::NeedsInput,
            created_at_unix_ms: 0,
            read: false,
        });

        assert_eq!(
            notification_destination(&state, notification),
            Some(NotificationDestination {
                workspace_index: 0,
                pane: Some((pane, 0)),
                surface,
            })
        );
    }

    #[test]
    fn terminal_bells_only_notify_for_the_configured_context_and_are_debounced() {
        let debounce = std::time::Duration::from_secs(1);
        assert!(!should_notify_terminal_bell(
            BellNotificationMode::Background,
            true,
            true,
            None,
            debounce,
        ));
        assert!(should_notify_terminal_bell(
            BellNotificationMode::Background,
            true,
            false,
            None,
            debounce,
        ));
        assert!(should_notify_terminal_bell(
            BellNotificationMode::Background,
            false,
            true,
            None,
            debounce,
        ));
        assert!(!should_notify_terminal_bell(
            BellNotificationMode::Always,
            false,
            false,
            Some(std::time::Duration::from_millis(999)),
            debounce,
        ));
        assert!(should_notify_terminal_bell(
            BellNotificationMode::Always,
            true,
            true,
            Some(debounce),
            debounce,
        ));
        assert!(!should_notify_terminal_bell(
            BellNotificationMode::Never,
            false,
            false,
            None,
            debounce,
        ));
    }

    #[test]
    fn command_palette_shortcut_hints_are_human_readable() {
        assert_eq!(
            format_shortcut_hint(&"ctrl+shift+p".parse().unwrap()),
            "Ctrl+Shift+P"
        );
        assert_eq!(format_shortcut_hint(&"alt+left".parse().unwrap()), "Alt+←");
        assert_eq!(
            format_shortcut_hint(&"super+pageup".parse().unwrap()),
            "Super+PageUp"
        );
    }

    #[test]
    fn workspace_close_confirmation_matches_terminal_safety_rules() {
        assert!(!workspace_requires_close_confirmation(0, 0));
        assert!(!workspace_requires_close_confirmation(1, 0));
        assert!(workspace_requires_close_confirmation(1, 1));
        assert!(workspace_requires_close_confirmation(2, 0));
    }

    #[test]
    fn application_close_confirmation_protects_processes_and_agent_requests() {
        assert!(!application_requires_close_confirmation(0, 0));
        assert!(application_requires_close_confirmation(1, 0));
        assert!(application_requires_close_confirmation(0, 1));
    }

    #[test]
    fn close_confirmation_cursor_only_points_at_its_buttons() {
        let cancel = ui::Bounds {
            x: 10.0,
            y: 20.0,
            width: 100.0,
            height: 30.0,
        };
        let confirm = ui::Bounds {
            x: 120.0,
            y: 20.0,
            width: 100.0,
            height: 30.0,
        };

        assert_eq!(
            confirmation_cursor_icon(cancel, confirm, 15.0, 25.0),
            CursorIcon::Pointer
        );
        assert_eq!(
            confirmation_cursor_icon(cancel, confirm, 125.0, 25.0),
            CursorIcon::Pointer
        );
        assert_eq!(
            confirmation_cursor_icon(cancel, confirm, 115.0, 25.0),
            CursorIcon::Default
        );
        assert_eq!(
            confirmation_hover(cancel, confirm, 15.0, 25.0),
            Some(NavigationHover::ConfirmationCancel)
        );
        assert_eq!(
            confirmation_hover(cancel, confirm, 125.0, 25.0),
            Some(NavigationHover::ConfirmationConfirm)
        );
        assert_eq!(confirmation_hover(cancel, confirm, 115.0, 25.0), None);
        assert_eq!(
            brighten_rgba([240, 250, 255, 128], 28),
            [255, 255, 255, 128]
        );
    }

    #[test]
    fn codex_trust_confirmation_distinguishes_all_three_actions() {
        let cancel = ui::Bounds {
            x: 10.0,
            y: 20.0,
            width: 90.0,
            height: 30.0,
        };
        let proceed = ui::Bounds {
            x: 110.0,
            y: 20.0,
            width: 140.0,
            height: 30.0,
        };
        let trust = ui::Bounds {
            x: 260.0,
            y: 20.0,
            width: 140.0,
            height: 30.0,
        };
        assert_eq!(
            codex_trust_confirmation_hover(cancel, proceed, trust, 15.0, 25.0),
            Some(NavigationHover::ConfirmationCancel)
        );
        assert_eq!(
            codex_trust_confirmation_hover(cancel, proceed, trust, 115.0, 25.0),
            Some(NavigationHover::ConfirmationContinue)
        );
        assert_eq!(
            codex_trust_confirmation_hover(cancel, proceed, trust, 265.0, 25.0),
            Some(NavigationHover::ConfirmationConfirm)
        );
        assert_eq!(
            codex_trust_confirmation_hover(cancel, proceed, trust, 105.0, 25.0),
            None
        );
    }

    #[test]
    fn workspace_unread_badge_hides_zero_and_caps_large_counts() {
        assert_eq!(unread_badge_label(0), None);
        assert_eq!(unread_badge_label(1).as_deref(), Some("1"));
        assert_eq!(unread_badge_label(42).as_deref(), Some("42"));
        assert_eq!(unread_badge_label(100).as_deref(), Some("99+"));
    }

    #[test]
    fn workspace_sidebar_height_only_reserves_existing_git_row() {
        assert_eq!(workspace_sidebar_row_height(false, false), 48.0);
        assert_eq!(workspace_sidebar_row_height(true, false), 67.0);
        assert_eq!(workspace_sidebar_row_height(false, true), 67.0);
        assert_eq!(workspace_sidebar_row_height(true, true), 86.0);
    }

    #[test]
    fn workspace_agent_state_prioritizes_active_agents_until_all_are_quiet() {
        use wmcterm_core::AgentState;

        assert_eq!(aggregate_agent_priority([]), 0);
        assert_eq!(aggregate_agent_priority([AgentState::Idle]), 1);
        assert_eq!(
            aggregate_agent_priority([AgentState::Idle, AgentState::Completed]),
            2
        );
        assert_eq!(
            aggregate_agent_priority([
                AgentState::Completed,
                AgentState::Running,
                AgentState::Idle,
            ]),
            3
        );
        assert_eq!(
            aggregate_agent_priority([AgentState::Running, AgentState::WaitingForInput]),
            4
        );
        assert_eq!(
            aggregate_agent_priority([AgentState::Running, AgentState::Error]),
            5
        );
    }

    #[test]
    fn terminal_context_contains_workspace_pane_and_surface_ids() {
        let state = AppState::default();
        let workspace = &state.workspaces[0];
        let pane = workspace.active_pane;
        let surface = state.panes[&pane].active_surface().unwrap();
        let SurfaceKind::Terminal(terminal) = state.surfaces[&surface].kind else {
            panic!("default surface should be a terminal");
        };

        assert_eq!(
            terminal_context_ids(&state, terminal),
            Some((workspace.id, pane, surface))
        );
        assert_eq!(terminal_context_ids(&state, uuid::Uuid::new_v4()), None);
    }

    #[test]
    fn agent_lifecycle_rejects_stale_hooks_and_accepts_legacy_hooks() {
        assert_eq!(
            effective_agent_lifecycle("life-current", None).unwrap(),
            "life-current"
        );
        assert_eq!(
            effective_agent_lifecycle("life-current", Some("life-current")).unwrap(),
            "life-current"
        );
        assert!(
            effective_agent_lifecycle("life-current", Some("life-old"))
                .unwrap_err()
                .contains("stale agent event")
        );
    }

    #[test]
    fn agent_hook_events_map_to_authoritative_session_events() {
        let permission = agent_session_event_kind(
            &AgentEvent::PermissionRequest {
                message: "allow shell?".into(),
            },
            "17",
            AgentRunMode::Interactive,
        )
        .unwrap();
        assert!(matches!(
            permission,
            AgentSessionEventKind::Telemetry(AgentTelemetry::StateObserved {
                state: AgentState::WaitingForPermission
            })
        ));

        let unattended_permission = agent_session_event_kind(
            &AgentEvent::PermissionRequest {
                message: "review command?".into(),
            },
            "17-auto",
            AgentRunMode::Unattended,
        )
        .unwrap();
        assert!(matches!(
            unattended_permission,
            AgentSessionEventKind::Telemetry(AgentTelemetry::StateObserved {
                state: AgentState::Running
            })
        ));
        let unattended_transition = agent_transition_for_run_mode(
            &AgentEvent::PermissionRequest {
                message: "review command?".into(),
            },
            AgentRunMode::Unattended,
        );
        assert_eq!(unattended_transition.state, AgentState::Running);
        assert_eq!(unattended_transition.attention, AttentionState::None);
        assert_eq!(unattended_transition.notification, None);
        assert!(matches!(
            agent_session_event_kind(
                &AgentEvent::PermissionRequest {
                    message: "unexpected approval?".into(),
                },
                "17-full",
                AgentRunMode::FullAccess,
            ),
            Some(AgentSessionEventKind::Telemetry(
                AgentTelemetry::StateObserved {
                    state: AgentState::Running
                }
            ))
        ));

        let input = agent_session_event_kind(
            &AgentEvent::NeedsInput {
                message: "choose target".into(),
            },
            "18",
            AgentRunMode::Interactive,
        )
        .unwrap();
        assert!(matches!(
            input,
            AgentSessionEventKind::ActionRequired(AgentActionRequest {
                request_id,
                kind: AgentRequestKind::Input,
                ..
            }) if request_id == "input:18"
        ));

        assert!(matches!(
            agent_session_event_kind(
                &AgentEvent::ToolUse {
                    tool: "cargo test".into()
                },
                "19",
                AgentRunMode::Interactive,
            ),
            Some(AgentSessionEventKind::Telemetry(AgentTelemetry::Activity {
                summary: Some(summary)
            })) if summary == "cargo test"
        ));
        assert!(matches!(
            agent_session_event_kind(
                &AgentEvent::Stop {
                    success: false,
                    message: Some("failed".into())
                },
                "20",
                AgentRunMode::Interactive,
            ),
            Some(AgentSessionEventKind::Telemetry(
                AgentTelemetry::StateObserved {
                    state: AgentState::Error
                }
            ))
        ));
        assert!(matches!(
            agent_session_event_kind(
                &AgentEvent::Stop {
                    success: true,
                    message: Some("done".into())
                },
                "21",
                AgentRunMode::Interactive,
            ),
            Some(AgentSessionEventKind::Telemetry(
                AgentTelemetry::StateObserved {
                    state: AgentState::Completed
                }
            ))
        ));
        assert!(
            agent_session_event_kind(
                &AgentEvent::SessionStart {
                    agent: "codex".into(),
                    session_id: None
                },
                "22",
                AgentRunMode::Interactive,
            )
            .is_none()
        );
    }

    #[test]
    fn focus_false_preserves_active_pane_and_surface() {
        let mut state = AppState::default();
        let pane = state.workspaces[0].active_pane;
        let original_surface = state.panes[&pane].active_surface().unwrap();
        let new_surface = wmcterm_core::Surface::terminal("background");
        let new_surface_id = new_surface.id;
        state.surfaces.insert(new_surface_id, new_surface);
        state
            .panes
            .get_mut(&pane)
            .unwrap()
            .surfaces
            .push(new_surface_id);

        focus_created_surface(&mut state, 0, pane, Some(new_surface_id), false);
        assert_eq!(state.panes[&pane].active_surface(), Some(original_surface));

        focus_created_surface(&mut state, 0, pane, Some(new_surface_id), true);
        assert_eq!(state.panes[&pane].active_surface(), Some(new_surface_id));
    }

    #[test]
    fn rpc_environment_rejects_reserved_and_malformed_values() {
        assert!(parse_environment(Some(&json!({"SAFE_NAME": "value"}))).is_ok());
        for environment in [
            json!({"WMCTERM_SURFACE_ID": "spoof"}),
            json!({"LD_PRELOAD": "/tmp/library.so"}),
            json!({"BAD-NAME": "value"}),
            json!({"OK": 7}),
        ] {
            assert!(parse_environment(Some(&environment)).is_err());
        }
    }

    #[test]
    fn spawned_terminals_advertise_color_capabilities() {
        assert_eq!(TERMINAL_TERM, "xterm-256color");
        assert_eq!(TERMINAL_COLORTERM, "truecolor");
        assert_eq!(TERMINAL_PROGRAM, "wmcterm");
    }

    #[test]
    fn rpc_create_options_validate_direction_title_and_cwd() {
        assert_eq!(
            split_direction(&json!({"direction": "down"})).unwrap(),
            SplitPlacement {
                axis: SplitAxis::Horizontal,
                new_first: false,
            }
        );
        assert!(split_direction(&json!({"direction": "diagonal"})).is_err());
        assert!(parse_terminal_create_options(&json!({"title": "bad\nname"})).is_err());
        assert!(
            parse_terminal_create_options(&json!({"cwd": "/path/that/does/not/exist"})).is_err()
        );
    }

    #[test]
    fn rpc_create_target_uses_explicit_workspace_without_ui_focus_fallback() {
        let mut state = AppState::new("first", std::path::PathBuf::from("/first"));
        state
            .execute(AppCommand::Workspace(WorkspaceCommand::Create {
                name: "second".into(),
                cwd: std::path::PathBuf::from("/second"),
            }))
            .unwrap();
        let second = state.workspaces[1].id;
        let second_pane = state.workspaces[1].active_pane;
        state.active_workspace = 0;

        assert_eq!(
            rpc_target_pane_in_state(&state, &json!({"workspace": second})).unwrap(),
            (1, second_pane)
        );
        assert!(rpc_target_pane_in_state(&state, &Value::Null).is_err());
        assert!(
            rpc_target_pane_in_state(
                &state,
                &json!({"workspace": state.workspaces[0].id, "pane": second_pane})
            )
            .is_err()
        );
    }

    #[test]
    fn agent_terminal_rpc_requires_an_explicit_valid_target() {
        let surface = SurfaceId::new_v4();
        assert_eq!(
            explicit_terminal_target(&json!({"surface": surface})).unwrap(),
            (None, Some(surface))
        );
        assert!(explicit_terminal_target(&Value::Null).is_err());
        assert!(explicit_terminal_target(&json!({"surface": "not-a-uuid"})).is_err());
    }

    #[test]
    fn codex_agent_update_rpc_is_strict_and_new_session_reclaims_old_subagents() {
        let mut builder = EventLoop::<UserEvent>::with_user_event();
        #[cfg(target_os = "linux")]
        {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_any_thread(true);
        }
        #[cfg(windows)]
        {
            use winit::platform::windows::EventLoopBuilderExtWindows;
            builder.with_any_thread(true);
        }
        let event_loop = builder.build().unwrap();
        let state = AppState::new("rpc-test", env::current_dir().unwrap());
        let surface = *state.surfaces.keys().next().unwrap();
        let terminal = match state.surfaces[&surface].kind {
            SurfaceKind::Terminal(terminal) => terminal,
            _ => panic!("test state must start with a terminal"),
        };
        let mut commands = HashMap::new();
        commands.insert(terminal, "exit".into());
        let mut app = WmctermApplication::new_configured(
            event_loop.create_proxy(),
            Config::default(),
            Some(state),
            commands,
            HashMap::new(),
            None,
            HashMap::new(),
            false,
        )
        .unwrap();
        let lifecycle = app.terminals[&terminal].lifecycle_id.clone();
        app.state.surfaces.get_mut(&surface).unwrap().agent = Some(wmcterm_core::AgentMetadata {
            name: "codex".into(),
            state: AgentState::Running,
            session_id: Some("session-one".into()),
            run_mode: AgentRunMode::Interactive,
        });
        let hook = wmcterm_agent::normalize_codex_subagent_hook_event(
            Some("SubagentStart"),
            &json!({
                "hook_event_name": "SubagentStart",
                "session_id": "session-one",
                "turn_id": "turn-one",
                "agent_id": "agent-one",
                "agent_type": "worker"
            }),
        )
        .unwrap();
        let base = || {
            let mut params = serde_json::to_value(hook.to_agent_event()).unwrap();
            let object = params.as_object_mut().unwrap();
            object.insert("terminal".into(), terminal.to_string().into());
            object.insert("surface".into(), surface.to_string().into());
            object.insert("surface_id".into(), surface.to_string().into());
            object.insert("lifecycle_id".into(), lifecycle.clone().into());
            object.insert("codex_hook".into(), serde_json::to_value(&hook).unwrap());
            params
        };
        let call = |app: &mut WmctermApplication, params| {
            app.handle_rpc(Request {
                id: uuid::Uuid::new_v4().to_string(),
                method: "agent.update".into(),
                params,
            })
        };
        let error = |response: Response| response.error.unwrap().message;

        let mut missing_surface = base();
        missing_surface.as_object_mut().unwrap().remove("surface");
        missing_surface
            .as_object_mut()
            .unwrap()
            .remove("surface_id");
        assert!(error(call(&mut app, missing_surface)).contains("surface"));

        let mut missing_lifecycle = base();
        missing_lifecycle
            .as_object_mut()
            .unwrap()
            .remove("lifecycle_id");
        assert!(error(call(&mut app, missing_lifecycle)).contains("lifecycle"));

        let mut wrong_surface = base();
        wrong_surface
            .as_object_mut()
            .unwrap()
            .insert("surface_id".into(), SurfaceId::new_v4().to_string().into());
        assert!(error(call(&mut app, wrong_surface)).contains("surface"));

        let mut wrong_lifecycle = base();
        wrong_lifecycle
            .as_object_mut()
            .unwrap()
            .insert("lifecycle_id".into(), "stale".into());
        assert!(error(call(&mut app, wrong_lifecycle)).contains("lifecycle"));

        app.state.surfaces.get_mut(&surface).unwrap().agent = Some(wmcterm_core::AgentMetadata {
            name: "claude".into(),
            state: AgentState::Running,
            session_id: Some("session-one".into()),
            run_mode: AgentRunMode::Interactive,
        });
        assert!(error(call(&mut app, base())).contains("non-Codex"));

        app.state.surfaces.get_mut(&surface).unwrap().agent = Some(wmcterm_core::AgentMetadata {
            name: "codex".into(),
            state: AgentState::Running,
            session_id: Some("other-session".into()),
            run_mode: AgentRunMode::Interactive,
        });
        assert!(error(call(&mut app, base())).contains("session"));

        app.state.surfaces.get_mut(&surface).unwrap().agent = Some(wmcterm_core::AgentMetadata {
            name: "codex".into(),
            state: AgentState::Running,
            session_id: Some("session-one".into()),
            run_mode: AgentRunMode::Interactive,
        });
        for field in ["turn_id", "agent_id", "agent_type", "session_id"] {
            let mut params = base();
            params["codex_hook"][field] = " \t ".into();
            let response = call(&mut app, params);
            let message = response
                .error
                .unwrap_or_else(|| panic!("blank Codex hook {field} was accepted"))
                .message;
            assert!(message.contains(field), "unexpected error: {message}");
        }
        let accepted = call(&mut app, base());
        assert!(accepted.error.is_none(), "{:?}", accepted.error);
        assert_eq!(accepted.result.unwrap()["codex_subagents"], 1);

        let session_end = |session_id: &str| {
            let mut params = serde_json::to_value(AgentEvent::Stop {
                success: true,
                message: None,
            })
            .unwrap();
            let object = params.as_object_mut().unwrap();
            object.insert("terminal".into(), terminal.to_string().into());
            object.insert("surface".into(), surface.to_string().into());
            object.insert("surface_id".into(), surface.to_string().into());
            object.insert("lifecycle_id".into(), lifecycle.clone().into());
            object.insert("codex_session_end".into(), session_id.into());
            params
        };
        let mut session_end_without_surface = session_end("session-one");
        session_end_without_surface
            .as_object_mut()
            .unwrap()
            .remove("surface_id");
        assert!(error(call(&mut app, session_end_without_surface)).contains("surface"));
        let mut session_end_without_lifecycle = session_end("session-one");
        session_end_without_lifecycle
            .as_object_mut()
            .unwrap()
            .remove("lifecycle_id");
        assert!(error(call(&mut app, session_end_without_lifecycle)).contains("lifecycle"));

        let tracked_hook = |kind: &str, session_id: &str, agent_id: &str| {
            wmcterm_agent::normalize_codex_subagent_hook_event(
                Some(kind),
                &json!({
                    "hook_event_name": kind,
                    "session_id": session_id,
                    "turn_id": "turn-two",
                    "agent_id": agent_id,
                    "agent_type": "worker"
                }),
            )
            .unwrap()
        };
        apply_codex_subagent_status(
            &mut app.codex_subagents,
            surface,
            &tracked_hook("SubagentStop", "session-one", "old-stopped"),
        );
        apply_codex_subagent_status(
            &mut app.codex_subagents,
            surface,
            &tracked_hook("SubagentStart", "session-two", "new-active"),
        );
        let other_surface = SurfaceId::new_v4();
        apply_codex_subagent_status(
            &mut app.codex_subagents,
            other_surface,
            &tracked_hook("SubagentStart", "session-one", "other-active"),
        );
        apply_codex_subagent_status(
            &mut app.codex_subagents,
            other_surface,
            &tracked_hook("SubagentStop", "session-one", "other-stopped"),
        );

        let mut new_session = serde_json::to_value(AgentEvent::SessionStart {
            agent: "codex".into(),
            session_id: Some("session-two".into()),
        })
        .unwrap();
        let object = new_session.as_object_mut().unwrap();
        object.insert("terminal".into(), terminal.to_string().into());
        object.insert("surface".into(), surface.to_string().into());
        object.insert("surface_id".into(), surface.to_string().into());
        object.insert("lifecycle_id".into(), lifecycle.clone().into());
        let accepted_start = call(&mut app, new_session);
        assert!(accepted_start.error.is_none(), "{:?}", accepted_start.error);
        assert!(
            !app.codex_subagents.states.keys().any(|subagent| {
                subagent.surface == surface && subagent.session_id == "session-one"
            }),
            "a routed new Codex SessionStart must clear the old session without SessionEnd"
        );
        assert!(
            !app.codex_subagents
                .stopped_order
                .contains_key(&(surface, "session-one".into()))
        );
        assert_eq!(
            codex_subagent_count(&app.codex_subagents, surface, "session-two"),
            1,
            "the new session must not clear its own status"
        );
        assert_eq!(
            codex_subagent_count(&app.codex_subagents, other_surface, "session-one"),
            1,
            "new SessionStart must not clear another surface"
        );
        assert!(app.codex_subagents.states.iter().any(|(subagent, state)| {
            subagent.surface == other_surface
                && subagent.agent_id == "other-stopped"
                && *state == CodexSubagentState::Stopped
        }));
        assert!(
            app.codex_subagents
                .stopped_order
                .contains_key(&(other_surface, "session-one".into()))
        );

        let accepted_end = call(&mut app, session_end("session-two"));
        assert!(accepted_end.error.is_none(), "{:?}", accepted_end.error);
        assert_eq!(
            codex_subagent_count(&app.codex_subagents, surface, "session-two"),
            0
        );
        assert_eq!(
            codex_subagent_count(&app.codex_subagents, other_surface, "session-one"),
            1
        );

        app.foreground_agents
            .insert(terminal, (AgentKind::Codex, AgentRunMode::Interactive));
        app.automatic_agent_status.remove(&terminal);
        assert!(app.state.surfaces[&surface].agent.is_some());
        assert!(app.update_foreground_agent(terminal, &[]));
        assert!(!app.foreground_agents.contains_key(&terminal));
        assert!(app.state.surfaces[&surface].agent.is_none());
    }

    #[test]
    fn left_and_up_splits_place_the_new_pane_first() {
        let target = PaneId::new_v4();
        let new_pane = PaneId::new_v4();
        let mut root = PaneNode::leaf(target);
        root.split(target, new_pane, SplitAxis::Vertical, 0.5)
            .unwrap();
        assert!(place_new_split_first(&mut root, target, new_pane));
        let mut leaves = Vec::new();
        root.leaves(&mut leaves);
        assert_eq!(leaves, vec![new_pane, target]);
    }

    #[test]
    fn new_workspace_directory_priority_is_inherit_then_default_then_process() {
        let active = std::path::Path::new("/active");
        let default = std::path::Path::new("/default");
        let process = std::path::Path::new("/process");
        assert_eq!(
            select_new_workspace_cwd(true, Some(active), Some(default), process),
            active
        );
        assert_eq!(
            select_new_workspace_cwd(true, None, Some(default), process),
            default
        );
        assert_eq!(
            select_new_workspace_cwd(false, Some(active), Some(default), process),
            default
        );
        assert_eq!(
            select_new_workspace_cwd(false, Some(active), None, process),
            process
        );
    }

    #[test]
    fn workspace_directory_resolution_expands_home_and_rejects_non_directories() {
        let process = std::env::current_dir().unwrap();
        assert_eq!(resolve_existing_directory(".", &process).unwrap(), process);
        assert!(resolve_existing_directory("", &process).is_err());
        assert!(resolve_existing_directory("Cargo.toml", &process).is_err());

        let home = wmcterm_config::expand_home_path(std::path::Path::new("~"));
        if home.is_dir() {
            assert_eq!(
                resolve_existing_directory("~", &process).unwrap(),
                home.canonicalize().unwrap()
            );
        }
    }
}
