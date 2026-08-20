use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

pub type WorkspaceId = Uuid;
pub type PaneId = Uuid;
pub type SurfaceId = Uuid;
pub type TerminalId = Uuid;
pub type NotificationId = Uuid;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CloseOthersError {
    #[error("anchor {0} is not in the requested scope")]
    AnchorNotInScope(Uuid),
}

/// Build a deterministic close set while guaranteeing that `anchor` survives.
///
/// Agent-facing batch operations use this instead of enumerating and closing
/// resources client-side, where a stale list or an omitted id could close the
/// caller itself.
pub fn plan_close_others(candidates: &[Uuid], anchor: Uuid) -> Result<Vec<Uuid>, CloseOthersError> {
    if !candidates.contains(&anchor) {
        return Err(CloseOthersError::AnchorNotInScope(anchor));
    }
    Ok(candidates
        .iter()
        .copied()
        .filter(|candidate| *candidate != anchor)
        .collect())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SplitAxis {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PaneNode {
    Leaf {
        pane: PaneId,
    },
    Split {
        axis: SplitAxis,
        ratio: f32,
        first: Box<PaneNode>,
        second: Box<PaneNode>,
    },
}

#[derive(Debug, Error, PartialEq)]
pub enum LayoutError {
    #[error("pane {0} does not exist")]
    PaneNotFound(PaneId),
    #[error("split ratio must be finite and between 0.1 and 0.9")]
    InvalidRatio,
    #[error("cannot remove the last pane")]
    CannotRemoveLastPane,
    #[error("pane {target} does not exist")]
    PaneNotFoundInTree { target: PaneId, tree: Box<PaneNode> },
}

impl PaneNode {
    pub fn leaf(pane: PaneId) -> Self {
        Self::Leaf { pane }
    }

    pub fn contains(&self, target: PaneId) -> bool {
        match self {
            Self::Leaf { pane } => *pane == target,
            Self::Split { first, second, .. } => first.contains(target) || second.contains(target),
        }
    }

    pub fn split(
        &mut self,
        target: PaneId,
        new_pane: PaneId,
        axis: SplitAxis,
        ratio: f32,
    ) -> Result<(), LayoutError> {
        if !ratio.is_finite() || !(0.1..=0.9).contains(&ratio) {
            return Err(LayoutError::InvalidRatio);
        }
        match self {
            Self::Leaf { pane } if *pane == target => {
                *self = Self::Split {
                    axis,
                    ratio,
                    first: Box::new(Self::leaf(target)),
                    second: Box::new(Self::leaf(new_pane)),
                };
                Ok(())
            }
            Self::Leaf { .. } => Err(LayoutError::PaneNotFound(target)),
            Self::Split { first, second, .. } => {
                if first.contains(target) {
                    first.split(target, new_pane, axis, ratio)
                } else {
                    second.split(target, new_pane, axis, ratio)
                }
            }
        }
    }

    pub fn leaves(&self, output: &mut Vec<PaneId>) {
        match self {
            Self::Leaf { pane } => output.push(*pane),
            Self::Split { first, second, .. } => {
                first.leaves(output);
                second.leaves(output);
            }
        }
    }

    pub fn remove(self, target: PaneId) -> Result<Self, LayoutError> {
        match self {
            Self::Leaf { pane } if pane == target => Err(LayoutError::CannotRemoveLastPane),
            leaf @ Self::Leaf { .. } => Err(LayoutError::PaneNotFoundInTree {
                target,
                tree: Box::new(leaf),
            }),
            Self::Split {
                axis,
                ratio,
                first,
                second,
            } => {
                if matches!(&*first, Self::Leaf { pane } if *pane == target) {
                    return Ok(*second);
                }
                if matches!(&*second, Self::Leaf { pane } if *pane == target) {
                    return Ok(*first);
                }
                if first.contains(target) {
                    Ok(Self::Split {
                        axis,
                        ratio,
                        first: Box::new(first.remove(target)?),
                        second,
                    })
                } else if second.contains(target) {
                    Ok(Self::Split {
                        axis,
                        ratio,
                        first,
                        second: Box::new(second.remove(target)?),
                    })
                } else {
                    Err(LayoutError::PaneNotFoundInTree {
                        target,
                        tree: Box::new(Self::Split {
                            axis,
                            ratio,
                            first,
                            second,
                        }),
                    })
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    #[default]
    Idle,
    Running,
    NeedsInput,
    Completed,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: WorkspaceId,
    pub name: String,
    pub root: PaneNode,
    pub cwd: PathBuf,
    pub active_pane: PaneId,
    pub status: WorkspaceStatus,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub git_branch: Option<String>,
    #[serde(default)]
    pub pull_request: Option<String>,
    #[serde(default)]
    pub listening_ports: Vec<u16>,
    #[serde(default)]
    pub pinned: bool,
}

impl Workspace {
    pub fn new(name: impl Into<String>, cwd: PathBuf) -> Self {
        let pane = PaneId::new_v4();
        Self {
            id: WorkspaceId::new_v4(),
            name: name.into(),
            root: PaneNode::leaf(pane),
            cwd,
            active_pane: pane,
            status: WorkspaceStatus::Idle,
            description: String::new(),
            git_branch: None,
            pull_request: None,
            listening_ports: Vec::new(),
            pinned: false,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionState {
    #[default]
    None,
    Activity,
    NeedsInput,
    Error,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    #[default]
    Unknown,
    Idle,
    Running,
    WaitingForInput,
    WaitingForPermission,
    Completed,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentMetadata {
    pub name: String,
    pub state: AgentState,
    pub session_id: Option<String>,
}

pub const AGENT_SESSION_SCHEMA_VERSION: u32 = 1;
const MAX_RECENT_AGENT_EVENTS: usize = 256;

/// Stable, dependency-free agent identifier (`claude`, `codex`, `opencode`, …).
/// Keeping this open rather than using an enum lets newer adapters round-trip
/// through older wmcterm session files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentKindId(pub String);

impl AgentKindId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLifecyclePhase {
    #[default]
    Unknown,
    Running,
    Exited,
    Failed,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentLifecycle {
    /// Monotonic process/session incarnation. A restarted agent must use a
    /// greater generation so delayed hook events cannot mutate the new run.
    pub generation: u64,
    pub phase: AgentLifecyclePhase,
    /// Producer lifecycle identity (for example the hook envelope's
    /// `lifecycle_id`). Unlike a PID, it remains safe across PID reuse.
    pub external_id: Option<String>,
    pub last_sequence: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentLaunchInfo {
    pub agent_kind: AgentKindId,
    pub session_id: Option<String>,
    pub pid: Option<u32>,
    pub process_started_at_unix_ms: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub transcript: Option<PathBuf>,
    pub launch_command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRequestKind {
    Permission,
    Input,
    Confirmation,
}

/// A request that needs a human or controlling client to act. Telemetry never
/// uses this type, which prevents informational events from creating prompts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActionRequest {
    pub request_id: String,
    pub kind: AgentRequestKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub choices: Vec<String>,
}

/// Informational observations. These may update display metadata/state but are
/// never interpreted as permission or input requests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentTelemetry {
    Activity {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
    },
    StateObserved {
        state: AgentState,
    },
    SessionObserved {
        session_id: String,
    },
    ProcessObserved {
        pid: u32,
        started_at_unix_ms: u64,
    },
    CwdObserved {
        cwd: PathBuf,
    },
    TranscriptObserved {
        transcript: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentLifecycleEvent {
    Started {
        #[serde(flatten)]
        launch: AgentLaunchInfo,
    },
    Exited {
        success: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "category", content = "payload", rename_all = "snake_case")]
pub enum AgentSessionEventKind {
    Lifecycle(AgentLifecycleEvent),
    Telemetry(AgentTelemetry),
    ActionRequired(AgentActionRequest),
    ActionResolved { request_id: String },
}

impl AgentSessionEventKind {
    pub const fn is_actionable(&self) -> bool {
        matches!(self, Self::ActionRequired(_))
    }

    pub const fn is_telemetry(&self) -> bool {
        matches!(self, Self::Telemetry(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSessionEvent {
    /// Stable producer-supplied identity used for idempotence across IPC retries
    /// and process restarts.
    pub event_id: String,
    pub lifecycle_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    pub occurred_at_unix_ms: u64,
    pub kind: AgentSessionEventKind,
}

/// Authoritative state for one agent-backed Surface. Every field has a serde
/// default so session JSON written before this model existed remains readable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentSession {
    pub schema_version: u32,
    #[serde(alias = "surface")]
    pub surface_id: SurfaceId,
    pub lifecycle: AgentLifecycle,
    #[serde(alias = "agent")]
    pub agent_kind: AgentKindId,
    #[serde(alias = "session")]
    pub session_id: Option<String>,
    pub pid: Option<u32>,
    #[serde(alias = "process_start_unix_ms")]
    pub process_started_at_unix_ms: Option<u64>,
    pub cwd: Option<PathBuf>,
    pub transcript: Option<PathBuf>,
    #[serde(alias = "command")]
    pub launch_command: Option<String>,
    pub state: AgentState,
    /// Optimistic-concurrency/state revision. It increments exactly once per
    /// successfully applied event and never for duplicates/rejections.
    pub version: u64,
    pub last_activity_unix_ms: u64,
    pub pending_requests: Vec<AgentActionRequest>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    recent_event_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    retired_lifecycle_ids: Vec<String>,
}

impl Default for AgentSession {
    fn default() -> Self {
        Self {
            schema_version: AGENT_SESSION_SCHEMA_VERSION,
            surface_id: SurfaceId::nil(),
            lifecycle: AgentLifecycle::default(),
            agent_kind: AgentKindId::default(),
            session_id: None,
            pid: None,
            process_started_at_unix_ms: None,
            cwd: None,
            transcript: None,
            launch_command: None,
            state: AgentState::Unknown,
            version: 0,
            last_activity_unix_ms: 0,
            pending_requests: Vec::new(),
            recent_event_ids: Vec::new(),
            retired_lifecycle_ids: Vec::new(),
        }
    }
}

impl AgentSession {
    pub fn new(surface_id: SurfaceId) -> Self {
        Self {
            surface_id,
            ..Self::default()
        }
    }

    pub fn has_seen_event(&self, event_id: &str) -> bool {
        self.recent_event_ids.iter().any(|seen| seen == event_id)
    }

    pub fn next_lifecycle_generation(&self) -> Result<u64, AgentSessionError> {
        self.lifecycle
            .generation
            .checked_add(1)
            .ok_or(AgentSessionError::LifecycleGenerationOverflow)
    }

    pub fn apply_event(
        &mut self,
        event: &AgentSessionEvent,
    ) -> Result<AgentEventApplyOutcome, AgentSessionError> {
        if event.event_id.trim().is_empty() {
            return Err(AgentSessionError::EmptyEventId);
        }
        if self.has_seen_event(&event.event_id) {
            return Ok(AgentEventApplyOutcome::Duplicate {
                version: self.version,
            });
        }
        let next_version = self
            .version
            .checked_add(1)
            .ok_or(AgentSessionError::VersionOverflow)?;

        match &event.kind {
            AgentSessionEventKind::Lifecycle(AgentLifecycleEvent::Started { launch }) => {
                if event.lifecycle_generation == 0 {
                    return Err(AgentSessionError::InvalidLifecycleGeneration);
                }
                if event.lifecycle_generation < self.lifecycle.generation {
                    return Err(AgentSessionError::StaleLifecycle {
                        event: event.lifecycle_generation,
                        current: self.lifecycle.generation,
                    });
                }
                if event.lifecycle_generation == self.lifecycle.generation
                    && self.lifecycle.phase != AgentLifecyclePhase::Unknown
                {
                    return Err(AgentSessionError::LifecycleAlreadyStarted(
                        event.lifecycle_generation,
                    ));
                }
                if let Some(lifecycle_id) = event.lifecycle_id.as_deref() {
                    if lifecycle_id.trim().is_empty() {
                        return Err(AgentSessionError::EmptyLifecycleId);
                    }
                    if self
                        .retired_lifecycle_ids
                        .iter()
                        .any(|retired| retired == lifecycle_id)
                    {
                        return Err(AgentSessionError::RetiredLifecycle(lifecycle_id.to_owned()));
                    }
                }
                if launch.agent_kind.as_str().trim().is_empty() {
                    return Err(AgentSessionError::EmptyAgentKind);
                }
                if launch.pid.is_some() != launch.process_started_at_unix_ms.is_some() {
                    return Err(AgentSessionError::IncompleteProcessIdentity);
                }
                if let Some(current) = self.lifecycle.external_id.take()
                    && event.lifecycle_id.as_deref() != Some(current.as_str())
                {
                    self.remember_retired_lifecycle(current);
                }
                self.lifecycle = AgentLifecycle {
                    generation: event.lifecycle_generation,
                    phase: AgentLifecyclePhase::Running,
                    external_id: event.lifecycle_id.clone(),
                    last_sequence: event.sequence,
                };
                self.agent_kind.clone_from(&launch.agent_kind);
                self.session_id.clone_from(&launch.session_id);
                self.pid = launch.pid;
                self.process_started_at_unix_ms = launch.process_started_at_unix_ms;
                self.cwd.clone_from(&launch.cwd);
                self.transcript.clone_from(&launch.transcript);
                self.launch_command.clone_from(&launch.launch_command);
                self.pending_requests.clear();
                self.state = AgentState::Running;
            }
            kind => {
                self.require_current_running_lifecycle(event.lifecycle_generation)?;
                self.require_matching_external_lifecycle(event.lifecycle_id.as_deref())?;
                self.require_newer_sequence(event.sequence)?;
                match kind {
                    AgentSessionEventKind::Lifecycle(AgentLifecycleEvent::Exited {
                        success,
                        ..
                    }) => {
                        self.lifecycle.phase = AgentLifecyclePhase::Exited;
                        self.state = if *success {
                            AgentState::Completed
                        } else {
                            AgentState::Error
                        };
                        self.pending_requests.clear();
                    }
                    AgentSessionEventKind::Lifecycle(AgentLifecycleEvent::Failed { .. }) => {
                        self.lifecycle.phase = AgentLifecyclePhase::Failed;
                        self.state = AgentState::Error;
                        self.pending_requests.clear();
                    }
                    AgentSessionEventKind::Telemetry(telemetry) => {
                        self.apply_telemetry(telemetry);
                    }
                    AgentSessionEventKind::ActionRequired(request) => {
                        if request.request_id.trim().is_empty() {
                            return Err(AgentSessionError::EmptyRequestId);
                        }
                        if !self
                            .pending_requests
                            .iter()
                            .any(|pending| pending.request_id == request.request_id)
                        {
                            self.pending_requests.push(request.clone());
                        }
                        self.refresh_waiting_state();
                    }
                    AgentSessionEventKind::ActionResolved { request_id } => {
                        let before = self.pending_requests.len();
                        self.pending_requests
                            .retain(|request| request.request_id != *request_id);
                        if before == self.pending_requests.len() {
                            return Err(AgentSessionError::UnknownRequest(request_id.clone()));
                        }
                        self.refresh_waiting_state();
                    }
                    AgentSessionEventKind::Lifecycle(AgentLifecycleEvent::Started { .. }) => {
                        unreachable!("lifecycle start handled above")
                    }
                }
            }
        }

        self.version = next_version;
        if event.sequence.is_some() {
            self.lifecycle.last_sequence = event.sequence;
        }
        self.last_activity_unix_ms = self.last_activity_unix_ms.max(event.occurred_at_unix_ms);
        self.remember_event(event.event_id.clone());
        Ok(AgentEventApplyOutcome::Applied {
            version: self.version,
        })
    }

    fn require_current_running_lifecycle(&self, generation: u64) -> Result<(), AgentSessionError> {
        if generation < self.lifecycle.generation {
            return Err(AgentSessionError::StaleLifecycle {
                event: generation,
                current: self.lifecycle.generation,
            });
        }
        if generation > self.lifecycle.generation {
            return Err(AgentSessionError::FutureLifecycle {
                event: generation,
                current: self.lifecycle.generation,
            });
        }
        if self.lifecycle.phase != AgentLifecyclePhase::Running {
            return Err(AgentSessionError::LifecycleNotRunning {
                generation,
                phase: self.lifecycle.phase,
            });
        }
        Ok(())
    }

    fn require_matching_external_lifecycle(
        &self,
        lifecycle_id: Option<&str>,
    ) -> Result<(), AgentSessionError> {
        if let (Some(event), Some(current)) = (lifecycle_id, self.lifecycle.external_id.as_deref())
            && event != current
        {
            return Err(AgentSessionError::MismatchedLifecycle {
                event: event.to_owned(),
                current: current.to_owned(),
            });
        }
        Ok(())
    }

    fn require_newer_sequence(&self, sequence: Option<u64>) -> Result<(), AgentSessionError> {
        if let (Some(event), Some(current)) = (sequence, self.lifecycle.last_sequence)
            && event <= current
        {
            return Err(AgentSessionError::OutOfOrderSequence { event, current });
        }
        Ok(())
    }

    fn apply_telemetry(&mut self, telemetry: &AgentTelemetry) {
        match telemetry {
            AgentTelemetry::Activity { .. } => {
                if self.pending_requests.is_empty() {
                    self.state = AgentState::Running;
                }
            }
            AgentTelemetry::StateObserved { state } => {
                if self.pending_requests.is_empty() {
                    self.state = *state;
                }
            }
            AgentTelemetry::SessionObserved { session_id } => {
                self.session_id = Some(session_id.clone());
            }
            AgentTelemetry::ProcessObserved {
                pid,
                started_at_unix_ms,
            } => {
                self.pid = Some(*pid);
                self.process_started_at_unix_ms = Some(*started_at_unix_ms);
            }
            AgentTelemetry::CwdObserved { cwd } => self.cwd = Some(cwd.clone()),
            AgentTelemetry::TranscriptObserved { transcript } => {
                self.transcript = Some(transcript.clone())
            }
        }
    }

    fn refresh_waiting_state(&mut self) {
        self.state = if self
            .pending_requests
            .iter()
            .any(|request| request.kind == AgentRequestKind::Permission)
        {
            AgentState::WaitingForPermission
        } else if self.pending_requests.is_empty() {
            AgentState::Running
        } else {
            AgentState::WaitingForInput
        };
    }

    fn remember_event(&mut self, event_id: String) {
        if self.recent_event_ids.len() == MAX_RECENT_AGENT_EVENTS {
            self.recent_event_ids.remove(0);
        }
        self.recent_event_ids.push(event_id);
    }

    fn remember_retired_lifecycle(&mut self, lifecycle_id: String) {
        const MAX_RETIRED_LIFECYCLES: usize = 32;
        if self.retired_lifecycle_ids.len() == MAX_RETIRED_LIFECYCLES {
            self.retired_lifecycle_ids.remove(0);
        }
        self.retired_lifecycle_ids.push(lifecycle_id);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentEventApplyOutcome {
    Applied { version: u64 },
    Duplicate { version: u64 },
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AgentSessionError {
    #[error("agent event id must not be empty")]
    EmptyEventId,
    #[error("agent lifecycle generation must be greater than zero")]
    InvalidLifecycleGeneration,
    #[error("agent lifecycle id must not be empty")]
    EmptyLifecycleId,
    #[error("agent lifecycle generation overflow")]
    LifecycleGenerationOverflow,
    #[error("agent kind must not be empty when a lifecycle starts")]
    EmptyAgentKind,
    #[error("agent process identity requires both pid and process start time")]
    IncompleteProcessIdentity,
    #[error("stale agent lifecycle {event}; current lifecycle is {current}")]
    StaleLifecycle { event: u64, current: u64 },
    #[error("future agent lifecycle {event} has not started; current lifecycle is {current}")]
    FutureLifecycle { event: u64, current: u64 },
    #[error("agent lifecycle {0} has already started")]
    LifecycleAlreadyStarted(u64),
    #[error("agent lifecycle `{0}` has already been retired")]
    RetiredLifecycle(String),
    #[error("agent event lifecycle `{event}` does not match current lifecycle `{current}`")]
    MismatchedLifecycle { event: String, current: String },
    #[error("agent event sequence {event} is not newer than {current}")]
    OutOfOrderSequence { event: u64, current: u64 },
    #[error("agent lifecycle {generation} is not running ({phase:?})")]
    LifecycleNotRunning {
        generation: u64,
        phase: AgentLifecyclePhase,
    },
    #[error("agent request `{0}` is not pending")]
    UnknownRequest(String),
    #[error("agent request id must not be empty")]
    EmptyRequestId,
    #[error("agent session version overflow")]
    VersionOverflow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum SurfaceKind {
    Terminal(TerminalId),
    Browser(Uuid),
    Markdown(Uuid),
    Logs(Uuid),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Surface {
    pub id: SurfaceId,
    pub title: String,
    pub kind: SurfaceKind,
    pub attention: AttentionState,
    pub agent: Option<AgentMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

impl Surface {
    pub fn terminal(title: impl Into<String>) -> Self {
        Self {
            id: SurfaceId::new_v4(),
            title: title.into(),
            kind: SurfaceKind::Terminal(TerminalId::new_v4()),
            attention: AttentionState::None,
            agent: None,
            resource: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub surfaces: Vec<SurfaceId>,
    pub active_surface: usize,
    pub zoomed: bool,
}

impl Pane {
    pub fn active_surface(&self) -> Option<SurfaceId> {
        self.surfaces.get(self.active_surface).copied()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: NotificationId,
    pub workspace: WorkspaceId,
    pub surface: SurfaceId,
    pub title: String,
    pub body: String,
    pub attention: AttentionState,
    pub created_at_unix_ms: u64,
    pub read: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    pub workspaces: Vec<Workspace>,
    pub active_workspace: usize,
    pub panes: HashMap<PaneId, Pane>,
    pub surfaces: HashMap<SurfaceId, Surface>,
    pub notifications: Vec<Notification>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new("default", PathBuf::from("."))
    }
}

impl AppState {
    pub fn new(name: impl Into<String>, cwd: PathBuf) -> Self {
        let workspace = Workspace::new(name, cwd);
        let pane_id = workspace.active_pane;
        let surface = Surface::terminal("Shell");
        let surface_id = surface.id;
        Self {
            workspaces: vec![workspace],
            active_workspace: 0,
            panes: HashMap::from([(
                pane_id,
                Pane {
                    id: pane_id,
                    surfaces: vec![surface_id],
                    active_surface: 0,
                    zoomed: false,
                },
            )]),
            surfaces: HashMap::from([(surface_id, surface)]),
            notifications: Vec::new(),
        }
    }

    pub fn active_workspace(&self) -> Option<&Workspace> {
        self.workspaces.get(self.active_workspace)
    }

    pub fn split_active(&mut self, axis: SplitAxis, ratio: f32) -> Result<PaneId, LayoutError> {
        let workspace = self
            .workspaces
            .get_mut(self.active_workspace)
            .ok_or(LayoutError::PaneNotFound(Uuid::nil()))?;
        let new_pane = PaneId::new_v4();
        workspace
            .root
            .split(workspace.active_pane, new_pane, axis, ratio)?;
        let surface = Surface::terminal("Shell");
        let surface_id = surface.id;
        self.surfaces.insert(surface_id, surface);
        self.panes.insert(
            new_pane,
            Pane {
                id: new_pane,
                surfaces: vec![surface_id],
                active_surface: 0,
                zoomed: false,
            },
        );
        workspace.active_pane = new_pane;
        Ok(new_pane)
    }

    pub fn push_notification(&mut self, notification: Notification) {
        if let Some(surface) = self.surfaces.get_mut(&notification.surface) {
            surface.attention = notification.attention;
        }
        self.notifications.push(notification);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "scope", content = "command", rename_all = "snake_case")]
pub enum AppCommand {
    Workspace(WorkspaceCommand),
    Pane(PaneCommand),
    Surface(SurfaceCommand),
    Terminal(TerminalCommand),
    App(AppAction),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorkspaceCommand {
    Create {
        name: String,
        cwd: PathBuf,
    },
    Activate {
        id: WorkspaceId,
    },
    Rename {
        id: WorkspaceId,
        name: String,
    },
    Describe {
        id: WorkspaceId,
        description: String,
    },
    ChangeDirectory {
        id: WorkspaceId,
        cwd: PathBuf,
    },
    Move {
        id: WorkspaceId,
        offset: isize,
    },
    Pin {
        id: WorkspaceId,
        pinned: bool,
    },
    Close {
        id: WorkspaceId,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum PaneCommand {
    Split { axis: SplitAxis, ratio: f32 },
    Focus { id: PaneId },
    Close { id: PaneId },
    ToggleZoom { id: PaneId },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SurfaceCommand {
    NewTerminal {
        pane: PaneId,
        command: Option<String>,
    },
    Activate {
        pane: PaneId,
        surface: SurfaceId,
    },
    Close {
        id: SurfaceId,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum TerminalCommand {
    Input {
        id: TerminalId,
        bytes: Vec<u8>,
    },
    Resize {
        id: TerminalId,
        cols: u16,
        rows: u16,
    },
    Scroll {
        id: TerminalId,
        lines: i32,
    },
    Shutdown {
        id: TerminalId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppAction {
    CommandPalette,
    ToggleSidebar,
    ToggleNotifications,
    JumpLatestUnread,
    ToggleCurrentUnread,
    Find,
    ClearScrollback,
    IncreaseFontSize,
    DecreaseFontSize,
    ResetFontSize,
    NewWindow,
    Quit,
}

#[derive(Debug, Error, PartialEq)]
pub enum CommandError {
    #[error(transparent)]
    Layout(#[from] LayoutError),
    #[error("workspace {0} does not exist")]
    WorkspaceNotFound(WorkspaceId),
    #[error("pane {0} does not exist")]
    PaneNotFound(PaneId),
    #[error("surface {0} does not exist")]
    SurfaceNotFound(SurfaceId),
    #[error("command must be handled by the terminal runtime")]
    TerminalRuntimeRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOutcome {
    StateChanged,
    TerminalRuntimeRequired,
    QuitRequested,
}

pub trait Action {
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    fn command(&self) -> AppCommand;
}

#[derive(Debug, Clone, Copy)]
pub struct BuiltinAction {
    id: &'static str,
    title: &'static str,
    command: fn() -> AppCommand,
}

impl Action for BuiltinAction {
    fn id(&self) -> &'static str {
        self.id
    }
    fn title(&self) -> &'static str {
        self.title
    }
    fn command(&self) -> AppCommand {
        (self.command)()
    }
}

pub struct ActionRegistry {
    actions: Vec<BuiltinAction>,
}

impl Default for ActionRegistry {
    fn default() -> Self {
        Self {
            actions: vec![
                BuiltinAction {
                    id: "workspace.new",
                    title: "New Workspace",
                    command: || {
                        AppCommand::Workspace(WorkspaceCommand::Create {
                            name: "Workspace".into(),
                            cwd: PathBuf::from("."),
                        })
                    },
                },
                BuiltinAction {
                    id: "workspace.close",
                    title: "Close Workspace",
                    command: || AppCommand::Workspace(WorkspaceCommand::Close { id: Uuid::nil() }),
                },
                BuiltinAction {
                    id: "workspace.rename",
                    title: "Rename Workspace",
                    command: || {
                        AppCommand::Workspace(WorkspaceCommand::Rename {
                            id: Uuid::nil(),
                            name: String::new(),
                        })
                    },
                },
                BuiltinAction {
                    id: "workspace.describe",
                    title: "Edit Workspace Description",
                    command: || {
                        AppCommand::Workspace(WorkspaceCommand::Describe {
                            id: Uuid::nil(),
                            description: String::new(),
                        })
                    },
                },
                BuiltinAction {
                    id: "workspace.change_directory",
                    title: "Change Workspace Directory",
                    command: || {
                        AppCommand::Workspace(WorkspaceCommand::ChangeDirectory {
                            id: Uuid::nil(),
                            cwd: PathBuf::new(),
                        })
                    },
                },
                BuiltinAction {
                    id: "workspace.pin",
                    title: "Pin or Unpin Workspace",
                    command: || {
                        AppCommand::Workspace(WorkspaceCommand::Pin {
                            id: Uuid::nil(),
                            pinned: true,
                        })
                    },
                },
                BuiltinAction {
                    id: "pane.split_right",
                    title: "Split Right",
                    command: || {
                        AppCommand::Pane(PaneCommand::Split {
                            axis: SplitAxis::Vertical,
                            ratio: 0.5,
                        })
                    },
                },
                BuiltinAction {
                    id: "pane.split_down",
                    title: "Split Down",
                    command: || {
                        AppCommand::Pane(PaneCommand::Split {
                            axis: SplitAxis::Horizontal,
                            ratio: 0.5,
                        })
                    },
                },
                BuiltinAction {
                    id: "surface.new",
                    title: "New Terminal Surface",
                    command: || {
                        AppCommand::Surface(SurfaceCommand::NewTerminal {
                            pane: Uuid::nil(),
                            command: None,
                        })
                    },
                },
                BuiltinAction {
                    id: "surface.close",
                    title: "Close Terminal Surface",
                    command: || AppCommand::Surface(SurfaceCommand::Close { id: Uuid::nil() }),
                },
                BuiltinAction {
                    id: "surface.filter_logs",
                    title: "Filter Active Log",
                    command: || AppCommand::App(AppAction::Find),
                },
                BuiltinAction {
                    id: "agent.claude",
                    title: "New Claude Code",
                    command: || {
                        AppCommand::Surface(SurfaceCommand::NewTerminal {
                            pane: Uuid::nil(),
                            command: Some("claude".into()),
                        })
                    },
                },
                BuiltinAction {
                    id: "agent.codex",
                    title: "New Codex",
                    command: || {
                        AppCommand::Surface(SurfaceCommand::NewTerminal {
                            pane: Uuid::nil(),
                            command: Some("wmcterm codex-teams".into()),
                        })
                    },
                },
                BuiltinAction {
                    id: "agent.gemini",
                    title: "New Gemini CLI",
                    command: || {
                        AppCommand::Surface(SurfaceCommand::NewTerminal {
                            pane: Uuid::nil(),
                            command: Some("gemini".into()),
                        })
                    },
                },
                BuiltinAction {
                    id: "agent.opencode",
                    title: "New OpenCode",
                    command: || {
                        AppCommand::Surface(SurfaceCommand::NewTerminal {
                            pane: Uuid::nil(),
                            command: Some("opencode".into()),
                        })
                    },
                },
                BuiltinAction {
                    id: "pane.close",
                    title: "Close Pane",
                    command: || AppCommand::Pane(PaneCommand::Close { id: Uuid::nil() }),
                },
                BuiltinAction {
                    id: "pane.zoom",
                    title: "Toggle Pane Zoom",
                    command: || AppCommand::Pane(PaneCommand::ToggleZoom { id: Uuid::nil() }),
                },
                BuiltinAction {
                    id: "terminal.find",
                    title: "Find in Terminal",
                    command: || AppCommand::App(AppAction::Find),
                },
                BuiltinAction {
                    id: "terminal.find_directory",
                    title: "Find in Workspace Files",
                    command: || AppCommand::App(AppAction::Find),
                },
                BuiltinAction {
                    id: "terminal.clear_scrollback",
                    title: "Clear Terminal Scrollback",
                    command: || AppCommand::App(AppAction::ClearScrollback),
                },
                BuiltinAction {
                    id: "terminal.font_increase",
                    title: "Increase Font Size",
                    command: || AppCommand::App(AppAction::IncreaseFontSize),
                },
                BuiltinAction {
                    id: "terminal.font_decrease",
                    title: "Decrease Font Size",
                    command: || AppCommand::App(AppAction::DecreaseFontSize),
                },
                BuiltinAction {
                    id: "terminal.font_reset",
                    title: "Reset Font Size",
                    command: || AppCommand::App(AppAction::ResetFontSize),
                },
                BuiltinAction {
                    id: "sidebar.toggle",
                    title: "Toggle Sidebar",
                    command: || AppCommand::App(AppAction::ToggleSidebar),
                },
                BuiltinAction {
                    id: "notifications.toggle",
                    title: "Toggle Notifications",
                    command: || AppCommand::App(AppAction::ToggleNotifications),
                },
                BuiltinAction {
                    id: "notifications.latest_unread",
                    title: "Jump to Latest Unread Notification",
                    command: || AppCommand::App(AppAction::JumpLatestUnread),
                },
                BuiltinAction {
                    id: "notifications.toggle_unread",
                    title: "Toggle Current Notification Unread",
                    command: || AppCommand::App(AppAction::ToggleCurrentUnread),
                },
                BuiltinAction {
                    id: "settings.open",
                    title: "Open Settings",
                    command: || AppCommand::App(AppAction::CommandPalette),
                },
                BuiltinAction {
                    id: "window.new",
                    title: "New Window",
                    command: || AppCommand::App(AppAction::NewWindow),
                },
                BuiltinAction {
                    id: "app.quit",
                    title: "Quit wmcterm",
                    command: || AppCommand::App(AppAction::Quit),
                },
            ],
        }
    }
}

impl ActionRegistry {
    pub fn actions(&self) -> &[BuiltinAction] {
        &self.actions
    }
    pub fn search(&self, query: &str) -> Vec<&BuiltinAction> {
        let query = query.to_ascii_lowercase();
        self.actions
            .iter()
            .filter(|action| {
                action.id.contains(&query) || action.title.to_ascii_lowercase().contains(&query)
            })
            .collect()
    }
}

impl AppState {
    pub fn execute(&mut self, command: AppCommand) -> Result<CommandOutcome, CommandError> {
        match command {
            AppCommand::Workspace(WorkspaceCommand::Create { name, cwd }) => {
                let workspace = Workspace::new(name, cwd);
                let pane = workspace.active_pane;
                let surface = Surface::terminal("Shell");
                let surface_id = surface.id;
                self.panes.insert(
                    pane,
                    Pane {
                        id: pane,
                        surfaces: vec![surface_id],
                        active_surface: 0,
                        zoomed: false,
                    },
                );
                self.surfaces.insert(surface_id, surface);
                self.workspaces.push(workspace);
                self.active_workspace = self.workspaces.len() - 1;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Workspace(WorkspaceCommand::Activate { id }) => {
                self.active_workspace = self
                    .workspaces
                    .iter()
                    .position(|item| item.id == id)
                    .ok_or(CommandError::WorkspaceNotFound(id))?;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Workspace(WorkspaceCommand::Rename { id, name }) => {
                self.workspaces
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or(CommandError::WorkspaceNotFound(id))?
                    .name = name;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Workspace(WorkspaceCommand::Describe { id, description }) => {
                self.workspaces
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or(CommandError::WorkspaceNotFound(id))?
                    .description = description;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Workspace(WorkspaceCommand::ChangeDirectory { id, cwd }) => {
                self.workspaces
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or(CommandError::WorkspaceNotFound(id))?
                    .cwd = cwd;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Workspace(WorkspaceCommand::Move { id, offset }) => {
                let index = self
                    .workspaces
                    .iter()
                    .position(|item| item.id == id)
                    .ok_or(CommandError::WorkspaceNotFound(id))?;
                let target = index
                    .saturating_add_signed(offset)
                    .min(self.workspaces.len().saturating_sub(1));
                if target != index {
                    let workspace = self.workspaces.remove(index);
                    self.workspaces.insert(target, workspace);
                    self.active_workspace = target;
                }
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Workspace(WorkspaceCommand::Pin { id, pinned }) => {
                self.workspaces
                    .iter_mut()
                    .find(|item| item.id == id)
                    .ok_or(CommandError::WorkspaceNotFound(id))?
                    .pinned = pinned;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Workspace(WorkspaceCommand::Close { id }) => {
                let index = self
                    .workspaces
                    .iter()
                    .position(|item| item.id == id)
                    .ok_or(CommandError::WorkspaceNotFound(id))?;
                let mut panes = Vec::new();
                self.workspaces[index].root.leaves(&mut panes);
                for pane in panes {
                    if let Some(pane) = self.panes.remove(&pane) {
                        for surface in pane.surfaces {
                            self.surfaces.remove(&surface);
                        }
                    }
                }
                self.workspaces.remove(index);
                self.notifications
                    .retain(|notification| notification.workspace != id);
                self.active_workspace = if self.workspaces.is_empty() {
                    0
                } else {
                    self.active_workspace.min(self.workspaces.len() - 1)
                };
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Pane(PaneCommand::Split { axis, ratio }) => {
                self.split_active(axis, ratio)?;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Pane(PaneCommand::Focus { id }) => {
                if !self.panes.contains_key(&id) {
                    return Err(CommandError::PaneNotFound(id));
                }
                let workspace = self
                    .workspaces
                    .get_mut(self.active_workspace)
                    .ok_or(CommandError::PaneNotFound(id))?;
                if !workspace.root.contains(id) {
                    return Err(CommandError::PaneNotFound(id));
                }
                workspace.active_pane = id;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Pane(PaneCommand::ToggleZoom { id }) => {
                let pane = self
                    .panes
                    .get_mut(&id)
                    .ok_or(CommandError::PaneNotFound(id))?;
                pane.zoomed = !pane.zoomed;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Surface(SurfaceCommand::Activate { pane, surface }) => {
                let pane = self
                    .panes
                    .get_mut(&pane)
                    .ok_or(CommandError::PaneNotFound(pane))?;
                pane.active_surface = pane
                    .surfaces
                    .iter()
                    .position(|id| *id == surface)
                    .ok_or(CommandError::SurfaceNotFound(surface))?;
                Ok(CommandOutcome::StateChanged)
            }
            AppCommand::Terminal(_)
            | AppCommand::Surface(SurfaceCommand::NewTerminal { .. })
            | AppCommand::Surface(SurfaceCommand::Close { .. })
            | AppCommand::Pane(PaneCommand::Close { .. }) => {
                Ok(CommandOutcome::TerminalRuntimeRequired)
            }
            AppCommand::App(AppAction::Quit) => Ok(CommandOutcome::QuitRequested),
            AppCommand::App(_) => Ok(CommandOutcome::StateChanged),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_others_plan_always_preserves_anchor() {
        let anchor = Uuid::new_v4();
        let first = Uuid::new_v4();
        let last = Uuid::new_v4();
        assert_eq!(
            plan_close_others(&[first, anchor, last], anchor).unwrap(),
            vec![first, last]
        );
        assert_eq!(
            plan_close_others(&[anchor], anchor).unwrap(),
            Vec::<Uuid>::new()
        );
    }

    #[test]
    fn close_others_plan_rejects_an_anchor_outside_scope() {
        let anchor = Uuid::new_v4();
        assert_eq!(
            plan_close_others(&[Uuid::new_v4()], anchor),
            Err(CloseOthersError::AnchorNotInScope(anchor))
        );
    }

    #[test]
    fn splits_a_nested_pane() {
        let first = PaneId::new_v4();
        let second = PaneId::new_v4();
        let third = PaneId::new_v4();
        let mut tree = PaneNode::leaf(first);

        tree.split(first, second, SplitAxis::Vertical, 0.6).unwrap();
        tree.split(second, third, SplitAxis::Horizontal, 0.5)
            .unwrap();

        assert!(tree.contains(first));
        assert!(tree.contains(second));
        assert!(tree.contains(third));
    }

    #[test]
    fn rejects_an_unsafe_ratio_without_changing_the_tree() {
        let pane = PaneId::new_v4();
        let mut tree = PaneNode::leaf(pane);
        let before = tree.clone();

        assert_eq!(
            tree.split(pane, PaneId::new_v4(), SplitAxis::Vertical, 1.0),
            Err(LayoutError::InvalidRatio)
        );
        assert_eq!(tree, before);
    }

    #[test]
    fn unified_commands_create_and_activate_workspaces() {
        let mut app = AppState::default();
        app.execute(AppCommand::Workspace(WorkspaceCommand::Create {
            name: "second".into(),
            cwd: PathBuf::from("/tmp"),
        }))
        .unwrap();
        assert_eq!(app.active_workspace().unwrap().name, "second");
        let first = app.workspaces[0].id;
        app.execute(AppCommand::Workspace(WorkspaceCommand::Activate {
            id: first,
        }))
        .unwrap();
        assert_eq!(app.active_workspace, 0);
    }

    #[test]
    fn workspace_metadata_edits_and_reorders() {
        let mut app = AppState::default();
        let first = app.workspaces[0].id;
        app.execute(AppCommand::Workspace(WorkspaceCommand::Create {
            name: "second".into(),
            cwd: PathBuf::from("/tmp"),
        }))
        .unwrap();
        app.execute(AppCommand::Workspace(WorkspaceCommand::Describe {
            id: first,
            description: "agent review".into(),
        }))
        .unwrap();
        app.execute(AppCommand::Workspace(WorkspaceCommand::ChangeDirectory {
            id: first,
            cwd: PathBuf::from("/work/review"),
        }))
        .unwrap();
        app.execute(AppCommand::Workspace(WorkspaceCommand::Move {
            id: first,
            offset: 1,
        }))
        .unwrap();
        assert_eq!(app.workspaces[1].id, first);
        assert_eq!(app.workspaces[1].description, "agent review");
        assert_eq!(app.workspaces[1].cwd, PathBuf::from("/work/review"));
    }

    #[test]
    fn final_workspace_can_be_closed_and_recreated() {
        let mut app = AppState::default();
        let workspace = app.workspaces[0].id;
        app.execute(AppCommand::Workspace(WorkspaceCommand::Close {
            id: workspace,
        }))
        .unwrap();
        assert!(app.workspaces.is_empty());
        assert!(app.panes.is_empty());
        assert!(app.surfaces.is_empty());
        assert!(app.active_workspace().is_none());

        app.execute(AppCommand::Workspace(WorkspaceCommand::Create {
            name: "again".into(),
            cwd: PathBuf::from("/tmp"),
        }))
        .unwrap();
        assert_eq!(app.active_workspace().unwrap().name, "again");
    }

    #[test]
    fn action_registry_searches_one_command_source() {
        let registry = ActionRegistry::default();
        let matches = registry.search("split");
        assert_eq!(matches.len(), 2);
        assert!(matches.iter().all(|action| matches!(
            action.command(),
            AppCommand::Pane(PaneCommand::Split { .. })
        )));
        let codex = registry
            .actions()
            .iter()
            .find(|action| action.id() == "agent.codex")
            .unwrap();
        assert!(matches!(
            codex.command(),
            AppCommand::Surface(SurfaceCommand::NewTerminal {
                command: Some(command),
                ..
            }) if command == "wmcterm codex-teams"
        ));
    }

    fn agent_event(
        id: &str,
        lifecycle_generation: u64,
        time: u64,
        kind: AgentSessionEventKind,
    ) -> AgentSessionEvent {
        AgentSessionEvent {
            event_id: id.into(),
            lifecycle_generation,
            lifecycle_id: None,
            sequence: None,
            occurred_at_unix_ms: time,
            kind,
        }
    }

    fn agent_start(id: &str, generation: u64, time: u64) -> AgentSessionEvent {
        agent_event(
            id,
            generation,
            time,
            AgentSessionEventKind::Lifecycle(AgentLifecycleEvent::Started {
                launch: AgentLaunchInfo {
                    agent_kind: AgentKindId::new("codex"),
                    session_id: Some(format!("session-{generation}")),
                    pid: Some(4200 + generation as u32),
                    process_started_at_unix_ms: Some(time),
                    cwd: Some(PathBuf::from("/work")),
                    transcript: Some(PathBuf::from("/work/transcript.jsonl")),
                    launch_command: Some("codex --resume session".into()),
                },
            }),
        )
    }

    #[test]
    fn agent_session_start_populates_authoritative_identity() {
        let surface = SurfaceId::new_v4();
        let mut session = AgentSession::new(surface);
        assert_eq!(
            session.apply_event(&agent_start("start-1", 1, 100)),
            Ok(AgentEventApplyOutcome::Applied { version: 1 })
        );
        assert_eq!(session.surface_id, surface);
        assert_eq!(session.lifecycle.generation, 1);
        assert_eq!(session.lifecycle.phase, AgentLifecyclePhase::Running);
        assert_eq!(session.agent_kind.as_str(), "codex");
        assert_eq!(session.session_id.as_deref(), Some("session-1"));
        assert_eq!(session.pid, Some(4201));
        assert_eq!(session.cwd.as_deref(), Some(std::path::Path::new("/work")));
        assert_eq!(session.state, AgentState::Running);
        assert_eq!(session.last_activity_unix_ms, 100);
    }

    #[test]
    fn agent_requests_are_actionable_and_telemetry_cannot_clear_them() {
        let mut session = AgentSession::new(SurfaceId::new_v4());
        session.apply_event(&agent_start("start", 1, 10)).unwrap();
        let request = AgentActionRequest {
            request_id: "permission-1".into(),
            kind: AgentRequestKind::Permission,
            message: "Allow shell?".into(),
            choices: vec!["allow".into(), "deny".into()],
        };
        session
            .apply_event(&agent_event(
                "request",
                1,
                20,
                AgentSessionEventKind::ActionRequired(request.clone()),
            ))
            .unwrap();
        session
            .apply_event(&agent_event(
                "activity",
                1,
                19,
                AgentSessionEventKind::Telemetry(AgentTelemetry::Activity {
                    summary: Some("background output".into()),
                }),
            ))
            .unwrap();
        assert_eq!(session.pending_requests, vec![request]);
        assert_eq!(session.state, AgentState::WaitingForPermission);
        assert_eq!(session.last_activity_unix_ms, 20);

        session
            .apply_event(&agent_event(
                "resolved",
                1,
                30,
                AgentSessionEventKind::ActionResolved {
                    request_id: "permission-1".into(),
                },
            ))
            .unwrap();
        assert!(session.pending_requests.is_empty());
        assert_eq!(session.state, AgentState::Running);
    }

    #[test]
    fn duplicate_events_are_idempotent_without_version_changes() {
        let mut session = AgentSession::new(SurfaceId::new_v4());
        let start = agent_start("same-id", 1, 10);
        session.apply_event(&start).unwrap();
        assert_eq!(
            session.apply_event(&start),
            Ok(AgentEventApplyOutcome::Duplicate { version: 1 })
        );
        assert_eq!(session.version, 1);
    }

    #[test]
    fn newer_lifecycle_rejects_delayed_events_from_old_process() {
        let mut session = AgentSession::new(SurfaceId::new_v4());
        session.apply_event(&agent_start("start-1", 1, 10)).unwrap();
        session.apply_event(&agent_start("start-2", 2, 20)).unwrap();
        let delayed = agent_event(
            "old-output",
            1,
            30,
            AgentSessionEventKind::Telemetry(AgentTelemetry::Activity { summary: None }),
        );
        assert_eq!(
            session.apply_event(&delayed),
            Err(AgentSessionError::StaleLifecycle {
                event: 1,
                current: 2
            })
        );
        assert_eq!(session.version, 2);
        assert!(!session.has_seen_event("old-output"));
    }

    #[test]
    fn closed_and_future_lifecycles_reject_non_start_events() {
        let mut session = AgentSession::new(SurfaceId::new_v4());
        session.apply_event(&agent_start("start", 1, 10)).unwrap();
        let future = agent_event(
            "future",
            2,
            11,
            AgentSessionEventKind::Telemetry(AgentTelemetry::Activity { summary: None }),
        );
        assert_eq!(
            session.apply_event(&future),
            Err(AgentSessionError::FutureLifecycle {
                event: 2,
                current: 1
            })
        );
        session
            .apply_event(&agent_event(
                "exit",
                1,
                12,
                AgentSessionEventKind::Lifecycle(AgentLifecycleEvent::Exited {
                    success: true,
                    message: None,
                }),
            ))
            .unwrap();
        assert_eq!(session.state, AgentState::Completed);
        let after_exit = agent_event(
            "late-same-run",
            1,
            13,
            AgentSessionEventKind::Telemetry(AgentTelemetry::Activity { summary: None }),
        );
        assert!(matches!(
            session.apply_event(&after_exit),
            Err(AgentSessionError::LifecycleNotRunning { .. })
        ));
    }

    #[test]
    fn agent_session_deserializes_legacy_sparse_json_and_round_trips_unknown_agent() {
        let surface = SurfaceId::new_v4();
        let legacy = format!(
            r#"{{"surface":"{surface}","agent":"future-agent","session":"abc","command":"future --resume abc","state":"idle"}}"#
        );
        let session: AgentSession = serde_json::from_str(&legacy).unwrap();
        assert_eq!(session.surface_id, surface);
        assert_eq!(session.agent_kind.as_str(), "future-agent");
        assert_eq!(session.session_id.as_deref(), Some("abc"));
        assert_eq!(session.schema_version, AGENT_SESSION_SCHEMA_VERSION);
        assert_eq!(session.lifecycle, AgentLifecycle::default());
        assert_eq!(session.version, 0);

        let restored: AgentSession =
            serde_json::from_str(&serde_json::to_string(&session).unwrap()).unwrap();
        assert_eq!(restored, session);
    }

    #[test]
    fn agent_event_types_are_explicit_and_invalid_identity_is_atomic() {
        let telemetry =
            AgentSessionEventKind::Telemetry(AgentTelemetry::Activity { summary: None });
        let request = AgentSessionEventKind::ActionRequired(AgentActionRequest {
            request_id: "input-1".into(),
            kind: AgentRequestKind::Input,
            message: "Continue?".into(),
            choices: Vec::new(),
        });
        assert!(telemetry.is_telemetry());
        assert!(!telemetry.is_actionable());
        assert!(request.is_actionable());

        let mut session = AgentSession::new(SurfaceId::new_v4());
        let mut invalid = agent_start("start", 1, 10);
        if let AgentSessionEventKind::Lifecycle(AgentLifecycleEvent::Started { launch }) =
            &mut invalid.kind
        {
            launch.process_started_at_unix_ms = None;
        }
        assert_eq!(
            session.apply_event(&invalid),
            Err(AgentSessionError::IncompleteProcessIdentity)
        );
        assert_eq!(session, AgentSession::new(session.surface_id));
    }

    #[test]
    fn hook_lifecycle_ids_and_sequences_reject_delayed_delivery() {
        let mut session = AgentSession::new(SurfaceId::new_v4());
        let mut first = agent_start("start-a", 1, 10);
        first.lifecycle_id = Some("life-a".into());
        first.sequence = Some(1);
        session.apply_event(&first).unwrap();

        let mut second = agent_start("start-b", 2, 20);
        second.lifecycle_id = Some("life-b".into());
        second.sequence = Some(1);
        session.apply_event(&second).unwrap();

        let mut delayed = agent_event(
            "delayed-a",
            2,
            30,
            AgentSessionEventKind::Telemetry(AgentTelemetry::Activity { summary: None }),
        );
        delayed.lifecycle_id = Some("life-a".into());
        delayed.sequence = Some(2);
        assert_eq!(
            session.apply_event(&delayed),
            Err(AgentSessionError::MismatchedLifecycle {
                event: "life-a".into(),
                current: "life-b".into()
            })
        );

        let mut out_of_order = delayed;
        out_of_order.event_id = "same-life-old-sequence".into();
        out_of_order.lifecycle_id = Some("life-b".into());
        out_of_order.sequence = Some(1);
        assert_eq!(
            session.apply_event(&out_of_order),
            Err(AgentSessionError::OutOfOrderSequence {
                event: 1,
                current: 1
            })
        );

        let mut retired_start = agent_start("old-start-replayed", 3, 40);
        retired_start.lifecycle_id = Some("life-a".into());
        assert_eq!(
            session.apply_event(&retired_start),
            Err(AgentSessionError::RetiredLifecycle("life-a".into()))
        );
    }
}
